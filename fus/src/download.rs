// Copyright 2026 John "topjohnwu" Wu
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     https://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! Multi-threaded firmware download mechanism.

use crate::FusClient;
use crate::error::{Error, Result};
use aes::cipher::BlockModeDecrypt;
use aes::cipher::inout::InOutBuf;
use memmap2::MmapMut;
use std::collections::VecDeque;
use std::fs::OpenOptions;
use std::io::Read;
use std::path::Path;
use std::sync::{Condvar, Mutex};
use std::thread;
use std::time::Duration;

/// A trait for reporting firmware download progress and logging messages.
pub trait DownloadProgress: Send + Sync {
    /// Increments the download progress by the specified number of bytes.
    fn inc(&self, bytes: u64);

    /// Gets the current absolute byte position of the progress.
    fn position(&self) -> u64;

    /// Prints a standard log or status message.
    fn println(&self, msg: String);

    /// Prints a verbose log or status message (implementation decides if it is shown).
    fn println_verbose(&self, msg: String);

    /// Returns whether the caller has requested cancellation.
    ///
    /// The default keeps existing progress implementations source-compatible.
    /// Cancellation is cooperative: a worker already blocked in network I/O
    /// may take up to the client's 30-second I/O timeout to observe it.
    fn is_cancelled(&self) -> bool {
        false
    }

    /// Blocks while the caller has paused the download.
    ///
    /// Implementations should return when either the download resumes or is
    /// cancelled; [`DownloadProgress::is_cancelled`] is checked immediately
    /// after this hook returns. The default is a no-op.
    fn wait_if_paused(&self) {}
}

/// Maximum number of parallel connections accepted by [`FusClient::download`].
///
/// Keeping this bounded prevents an accidental extreme worker count from
/// creating millions of tiny ranges and exhausting memory before any download
/// begins.
pub const MAX_DOWNLOAD_THREADS: u64 = 64;

// Convenient no-op implementation for silent downloads (e.g. in tests)
impl DownloadProgress for () {
    fn inc(&self, _bytes: u64) {}
    fn position(&self) -> u64 {
        0
    }
    fn println(&self, _msg: String) {}
    fn println_verbose(&self, _msg: String) {}
}

impl FusClient {
    /// Downloads the firmware binary in parallel across multiple threads, decrypting it in place,
    /// and writing to the file at `path`.
    ///
    /// `threads` must be between 1 and [`MAX_DOWNLOAD_THREADS`]. Cancellation
    /// requested through [`DownloadProgress::is_cancelled`] is cooperative and
    /// can wait for the current blocking network operation's 30-second timeout.
    pub fn download<P, PathRef>(&self, path: PathRef, threads: u64, progress: &P) -> Result<()>
    where
        P: DownloadProgress,
        PathRef: AsRef<Path>,
    {
        validate_thread_count(threads)?;
        validate_download_info(&self.info)?;
        wait_until_active(progress)?;
        self.init_download()?;
        wait_until_active(progress)?;

        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(path)?;

        // Pre-allocate file
        file.set_len(self.info.size)?;

        let mut map = unsafe { MmapMut::map_mut(&file)? };

        // Split only on AES block boundaries. Metadata alignment was validated
        // above, so this calculation cannot overflow even for very large files.
        let chunk_size = (self.info.size / 16).div_ceil(threads) * 16;

        let mut queue = VecDeque::new();
        {
            let mut chunks = map.chunks_mut(chunk_size as usize).enumerate().peekable();
            while let Some((i, buf)) = chunks.next() {
                let is_last = chunks.peek().is_none();

                let start = i as u64 * chunk_size;
                // Ensure the last range covers the remainder of the file
                let end = if is_last {
                    None
                } else {
                    Some(start + chunk_size - 1)
                };

                queue.push_back(Chunk { buf, start, end });
            }
        }

        // Never spawn more connections than there are ranges to download.
        let n_workers = queue
            .len()
            .min(usize::try_from(threads).unwrap_or(usize::MAX));
        let pool = Pool {
            inner: Mutex::new(PoolInner {
                queue,
                in_flight: 0,
                live: n_workers,
                error: None,
            }),
            available: Condvar::new(),
        };

        thread::scope(|s| {
            let pool = &pool;
            let client = self;
            for _ in 0..n_workers {
                s.spawn(move || run_worker(pool, client, progress));

                // Stagger connection setup to avoid hammering the server
                thread::sleep(Duration::from_millis(100));
            }
        });

        let pool_error = {
            let mut state = pool.inner.lock().unwrap();
            state.error.take()
        };

        // The pool still borrows `map` through its (now-drained) work queue; drop it
        // before reading back from the mapping.
        drop(pool);

        // A worker may have cancelled or encountered a fatal stall after
        // partially writing the mapping. Do not flush, inspect padding, or
        // report success in that case.
        if let Some(error) = pool_error {
            drop(map);
            return Err(error);
        }

        let unpadded_len = validate_pkcs7_padding(&map)?;
        map.flush()?;
        drop(map);

        file.set_len(
            u64::try_from(unpadded_len).map_err(|_| Error::InvalidResponse {
                message: "decrypted firmware length exceeded the supported file size".to_string(),
            })?,
        )?;

        Ok(())
    }
}

/// A contiguous, not-yet-downloaded byte range mapped onto its slice of the
/// output file. `buf` is exactly the destination for `[start, end]` (or
/// `[start, EOF]` when `end` is `None`).
struct Chunk<'a> {
    buf: &'a mut [u8],
    start: u64,
    end: Option<u64>,
}

/// Shared, work-stealing pool of outstanding ranges.
struct Pool<'a> {
    inner: Mutex<PoolInner<'a>>,
    available: Condvar,
}

struct PoolInner<'a> {
    /// Ranges waiting to be downloaded.
    queue: VecDeque<Chunk<'a>>,
    /// Ranges currently being downloaded by some worker. While this is non-zero
    /// more work may still be handed back, so an idle worker must wait rather
    /// than conclude the download is finished.
    in_flight: usize,
    /// Number of worker threads still running. A throttled worker only sheds
    /// itself while this is `> 1`, so at least one connection always remains to
    /// carry the download to completion.
    live: usize,
    /// Holds the first fatal error occurred
    error: Option<crate::Error>,
}

enum ChunkOutcome {
    /// The whole range was downloaded and decrypted.
    Done,
    /// The connection stalled. `decrypted` bytes (a multiple of 16) of the range
    /// are complete; everything after that still needs downloading.
    Stalled { decrypted: usize },
    /// The caller requested cancellation.
    Cancelled,
    /// The server returned a response that cannot safely satisfy this range.
    Failed(Error),
}

/// Consecutive *no-progress* attempts tolerated on a single connection before it
/// is treated as throttled. The counter resets the moment a read advances the
/// decrypted offset, so this bounds a stall (repeated retries that keep failing
/// at the same offset) rather than the total number of blips over a long
/// download. A genuinely slow-but-alive connection delivers *some* bytes within
/// each timeout window and so never trips this.
const MAX_STALL_RETRIES: u32 = 4;

/// When only one connection is left, how many stalls in a row — with no overall
/// progress in between — to tolerate before declaring the server dead and
/// aborting, instead of retrying forever.
const MAX_DEAD_STALLS: u32 = 4;

/// A single download connection.
fn run_worker(pool: &Pool<'_>, client: &FusClient, progress: &impl DownloadProgress) {
    // For the final surviving connection only: how many times in a row the whole
    // download failed to advance. Lets us give up on a truly dead server instead
    // of spinning forever once we can no longer shed connections.
    let mut dead_stalls = 0_u32;

    loop {
        if wait_until_active(progress).is_err() {
            let mut state = pool.inner.lock().unwrap();
            if state.error.is_none() {
                state.error = Some(Error::Cancelled);
            }
            state.live -= 1;
            pool.available.notify_all();
            return;
        }

        // Take the next range, or wait until a throttled peer hands one back.
        // When the queue is empty and nothing is in flight, every byte is in.
        let chunk = {
            let mut state = pool.inner.lock().unwrap();
            loop {
                // If another thread set a fatal error, exit cooperatively.
                if state.error.is_some() {
                    state.live -= 1;
                    pool.available.notify_all();
                    return;
                }
                if let Some(chunk) = state.queue.pop_front() {
                    state.in_flight += 1;
                    break chunk;
                }
                if state.in_flight == 0 {
                    state.live -= 1;
                    pool.available.notify_all();
                    return;
                }
                state = pool.available.wait(state).unwrap();
            }
        };

        let outcome = download_chunk(client, &mut chunk.buf[..], chunk.start, chunk.end, progress);

        match outcome {
            ChunkOutcome::Done => {
                dead_stalls = 0;
                let mut state = pool.inner.lock().unwrap();
                state.in_flight -= 1;
                // A completion can be the event that drains the pool; wake idle
                // peers so they can observe it and exit.
                pool.available.notify_all();
            }
            ChunkOutcome::Stalled { decrypted } => {
                // Split the range: keep the decrypted prefix (already in the
                // file) and hand the rest back. ECB is position-independent, so
                // whichever worker resumes this tail decrypts it correctly from a
                // fresh decryptor at the 16-byte boundary `decrypted`.
                let stall_off = chunk.start + decrypted as u64;
                let Chunk { buf, end, .. } = chunk;
                let (_done, rest) = buf.split_at_mut(decrypted);
                let remainder = Chunk {
                    buf: rest,
                    start: stall_off,
                    end,
                };

                let mut state = pool.inner.lock().unwrap();

                // If another thread already aborted with error, just exit!
                if state.error.is_some() {
                    state.in_flight -= 1;
                    state.live -= 1;
                    pool.available.notify_all();
                    return;
                }

                state.in_flight -= 1;
                state.queue.push_back(remainder);

                if state.live > 1 {
                    // Other connections are still alive (and presumably making
                    // progress); shed this throttled one and let them finish the
                    // returned range.
                    state.live -= 1;
                    let remaining = state.live;
                    pool.available.notify_all();
                    drop(state);
                    progress.println_verbose(format!(
                        "Connection throttled at offset {stall_off}; \
                         reducing to {remaining} connection(s)"
                    ));
                    return;
                }

                // We are the last connection — shedding it would strand the
                // download, so keep retrying. Bail out only if nothing at all
                // comes through across several stall cycles.
                pool.available.notify_all();
                drop(state);

                // `decrypted` is the worker's actual progress in this cycle.
                // Never use the consumer-facing progress reporter for liveness:
                // valid implementations (including `()`) may not track a
                // position at all.
                if dead_stall_limit_reached(&mut dead_stalls, decrypted) {
                    let mut state = pool.inner.lock().unwrap();
                    state.error = Some(crate::Error::Stalled { offset: stall_off });
                    state.live -= 1;
                    pool.available.notify_all();
                    return;
                }
            }
            ChunkOutcome::Cancelled => {
                let mut state = pool.inner.lock().unwrap();
                state.in_flight -= 1;
                state.live -= 1;
                if state.error.is_none() {
                    state.error = Some(Error::Cancelled);
                }
                pool.available.notify_all();
                return;
            }
            ChunkOutcome::Failed(error) => {
                let mut state = pool.inner.lock().unwrap();
                state.in_flight -= 1;
                state.live -= 1;
                if state.error.is_none() {
                    state.error = Some(error);
                }
                pool.available.notify_all();
                return;
            }
        }
    }
}

/// Download the byte range `[start, end]` (or `[start, EOF]` when `end` is
/// `None`) into `chunk`, decrypting in place as bytes arrive.
fn download_chunk(
    client: &FusClient,
    chunk: &mut [u8],
    start: u64,
    end: Option<u64>,
    progress: &impl DownloadProgress,
) -> ChunkOutcome {
    let mut dec = client.get_decryptor();
    let mut dec_pos = 0_usize; // bytes decrypted so far (always a multiple of 16)
    let mut retries = 0_u32;

    loop {
        if wait_until_active(progress).is_err() {
            return ChunkOutcome::Cancelled;
        }

        let mut resp = match client.download_file_checked(Some(start + dec_pos as u64), end) {
            Ok(resp) => resp,
            Err(Error::Network(e)) => {
                retries += 1;
                if retries > MAX_STALL_RETRIES {
                    return ChunkOutcome::Stalled { decrypted: dec_pos };
                }
                progress.println_verbose(format!(
                    "Request error ({e}); retry {retries}/{MAX_STALL_RETRIES} at offset {}",
                    start + dec_pos as u64
                ));
                if interruptible_sleep(backoff(retries), progress).is_err() {
                    return ChunkOutcome::Cancelled;
                }
                continue;
            }
            Err(error) => return ChunkOutcome::Failed(error),
        };

        // We re-requested from `dec_pos`; discard the undecrypted tail and resume
        // writing there. Progress only counts decrypted bytes, so the re-fetched
        // overlap is never double-counted and there is nothing to roll back.
        let resume_from = dec_pos;
        let mut dl_pos = dec_pos; // bytes received into `chunk` this connection

        let stall = loop {
            if wait_until_active(progress).is_err() {
                return ChunkOutcome::Cancelled;
            }

            match resp.read(&mut chunk[dl_pos..]) {
                Ok(0) if dec_pos == chunk.len() => return ChunkOutcome::Done,
                Ok(0) => break std::io::Error::from(std::io::ErrorKind::UnexpectedEof),
                Ok(n) => dl_pos += n,
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(e) => break e, // connection dropped: resume the outer loop
            }

            let prev_dec = dec_pos;
            let (blocks, tail) = InOutBuf::from(&mut chunk[dec_pos..dl_pos]).into_chunks();
            dec.decrypt_blocks_inout(blocks);
            dec_pos = dl_pos - tail.len();
            progress.inc((dec_pos - prev_dec) as u64);
        };

        // Every byte is decrypted: the chunk is complete even though the
        // connection broke before the server signaled EOF (`Ok(0)`). Return now
        if dec_pos == chunk.len() {
            return ChunkOutcome::Done;
        }

        // Reset the budget only on real decryption progress (a whole new block),
        if dec_pos > resume_from {
            retries = 0;
        }
        retries += 1;
        if retries > MAX_STALL_RETRIES {
            return ChunkOutcome::Stalled { decrypted: dec_pos };
        }
        progress.println_verbose(format!(
            "Download error ({stall}); retry {retries}/{MAX_STALL_RETRIES}, \
             resuming at offset {}",
            start + dec_pos as u64
        ));
        if interruptible_sleep(backoff(retries), progress).is_err() {
            return ChunkOutcome::Cancelled;
        }
    }
}

/// Runs the caller's pause hook and then checks for cancellation.
fn wait_until_active(progress: &impl DownloadProgress) -> Result<()> {
    progress.wait_if_paused();
    if progress.is_cancelled() {
        Err(Error::Cancelled)
    } else {
        Ok(())
    }
}

fn validate_thread_count(threads: u64) -> Result<()> {
    if (1..=MAX_DOWNLOAD_THREADS).contains(&threads) {
        Ok(())
    } else {
        Err(Error::InvalidThreadCount {
            requested: threads,
            max: MAX_DOWNLOAD_THREADS,
        })
    }
}

fn dead_stall_limit_reached(dead_stalls: &mut u32, decrypted: usize) -> bool {
    if decrypted > 0 {
        *dead_stalls = 0;
        false
    } else {
        *dead_stalls = dead_stalls.saturating_add(1);
        *dead_stalls > MAX_DEAD_STALLS
    }
}

fn validate_download_info(info: &crate::BinaryInform) -> Result<()> {
    if info.size == 0 {
        return Err(Error::InvalidResponse {
            message: "firmware metadata reported a zero-byte download".to_string(),
        });
    }
    if info.key.len() != 16 {
        return Err(Error::InvalidResponse {
            message: format!(
                "firmware metadata contained an invalid {}-byte decryption key",
                info.key.len()
            ),
        });
    }
    if info.size % 16 != 0 {
        return Err(Error::InvalidResponse {
            message: format!(
                "firmware metadata reported a {}-byte ciphertext that is not aligned to the 16-byte AES block size",
                info.size
            ),
        });
    }
    Ok(())
}

fn validate_pkcs7_padding(data: &[u8]) -> Result<usize> {
    let padding_len = data.last().copied().ok_or_else(|| Error::InvalidResponse {
        message: "decrypted firmware was empty and had no PKCS#7 padding".to_string(),
    })? as usize;

    if padding_len == 0 || padding_len > 16 || padding_len > data.len() {
        return Err(Error::InvalidResponse {
            message: "decrypted firmware contained invalid PKCS#7 padding".to_string(),
        });
    }

    if !data[data.len() - padding_len..]
        .iter()
        .all(|byte| usize::from(*byte) == padding_len)
    {
        return Err(Error::InvalidResponse {
            message: "decrypted firmware contained inconsistent PKCS#7 padding bytes".to_string(),
        });
    }

    Ok(data.len() - padding_len)
}

/// Sleeps in short increments so cancellation remains responsive during retry
/// backoff. Pause time is handled by the progress implementation.
fn interruptible_sleep(duration: Duration, progress: &impl DownloadProgress) -> Result<()> {
    let mut remaining = duration;
    const POLL_INTERVAL: Duration = Duration::from_millis(100);

    while !remaining.is_zero() {
        wait_until_active(progress)?;
        let interval = remaining.min(POLL_INTERVAL);
        thread::sleep(interval);
        remaining = remaining.saturating_sub(interval);
    }

    wait_until_active(progress)
}

/// Exponential backoff capped at 30s: 1s, 2s, 4s, 8s, 16s, 30s, ...
fn backoff(attempt: u32) -> Duration {
    Duration::from_secs((1u64 << attempt.saturating_sub(1).min(5)).min(30))
}

#[cfg(test)]
mod tests {
    use super::*;

    struct LegacyProgress;

    impl DownloadProgress for LegacyProgress {
        fn inc(&self, _bytes: u64) {}

        fn position(&self) -> u64 {
            0
        }

        fn println(&self, _msg: String) {}

        fn println_verbose(&self, _msg: String) {}
    }

    #[test]
    fn control_hooks_are_backwards_compatible_defaults() {
        let progress = LegacyProgress;

        progress.wait_if_paused();
        assert!(!progress.is_cancelled());
        assert!(wait_until_active(&progress).is_ok());
    }

    struct CancelledProgress;

    impl DownloadProgress for CancelledProgress {
        fn inc(&self, _bytes: u64) {}

        fn position(&self) -> u64 {
            0
        }

        fn println(&self, _msg: String) {}

        fn println_verbose(&self, _msg: String) {}

        fn is_cancelled(&self) -> bool {
            true
        }
    }

    #[test]
    fn cancellation_is_reported_by_control_checks() {
        let error = wait_until_active(&CancelledProgress).unwrap_err();

        assert!(matches!(error, Error::Cancelled));
    }

    #[test]
    fn invalid_download_metadata_is_rejected_before_mapping() {
        let mut info = crate::BinaryInform::default();
        let error = validate_download_info(&info).unwrap_err();
        assert!(matches!(error, Error::InvalidResponse { .. }));

        info.size = 4096;
        info.key = vec![0; 15];
        let error = validate_download_info(&info).unwrap_err();
        assert!(error.to_string().contains("15-byte decryption key"));

        info.key.push(0);
        assert!(validate_download_info(&info).is_ok());

        info.size += 1;
        assert!(validate_download_info(&info).is_err());
    }

    #[test]
    fn download_thread_count_is_bounded() {
        assert!(validate_thread_count(1).is_ok());
        assert!(validate_thread_count(MAX_DOWNLOAD_THREADS).is_ok());

        for requested in [0, MAX_DOWNLOAD_THREADS + 1, u64::MAX] {
            let error = validate_thread_count(requested).unwrap_err();
            assert!(matches!(
                error,
                Error::InvalidThreadCount {
                    requested: actual,
                    max: MAX_DOWNLOAD_THREADS,
                } if actual == requested
            ));
        }
    }

    #[test]
    fn actual_decryption_progress_resets_dead_stall_counter() {
        let mut dead_stalls = MAX_DEAD_STALLS;
        assert!(!dead_stall_limit_reached(&mut dead_stalls, 16));
        assert_eq!(dead_stalls, 0);

        for expected in 1..=MAX_DEAD_STALLS {
            assert!(!dead_stall_limit_reached(&mut dead_stalls, 0));
            assert_eq!(dead_stalls, expected);
        }
        assert!(dead_stall_limit_reached(&mut dead_stalls, 0));
    }

    #[test]
    fn pkcs7_padding_is_validated_before_truncation() {
        let mut one_byte_padding = vec![0x41; 15];
        one_byte_padding.push(1);
        assert_eq!(validate_pkcs7_padding(&one_byte_padding).unwrap(), 15);

        let full_block_padding = vec![16; 16];
        assert_eq!(validate_pkcs7_padding(&full_block_padding).unwrap(), 0);

        for invalid in [
            vec![],
            vec![0; 16],
            vec![17; 32],
            vec![0x41, 0x42, 0x03, 0x03],
            vec![0x41, 0x01, 0x02],
        ] {
            assert!(validate_pkcs7_padding(&invalid).is_err(), "{invalid:?}");
        }
    }

    #[test]
    fn retry_backoff_is_capped() {
        assert_eq!(backoff(0), Duration::from_secs(1));
        assert_eq!(backoff(1), Duration::from_secs(1));
        assert_eq!(backoff(5), Duration::from_secs(16));
        assert_eq!(backoff(6), Duration::from_secs(30));
        assert_eq!(backoff(u32::MAX), Duration::from_secs(30));
    }
}
