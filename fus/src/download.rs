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
use std::fs::{self, File, OpenOptions};
use std::io::Read;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
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
    /// may take up to the client's 10-second I/O timeout to observe it.
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
    /// can wait for the current blocking network operation's 10-second timeout.
    pub fn download<P, PathRef>(&self, path: PathRef, threads: u64, progress: &P) -> Result<()>
    where
        P: DownloadProgress,
        PathRef: AsRef<Path>,
    {
        validate_thread_count(threads)?;
        validate_download_info(&self.info)?;
        wait_until_active(progress)?;
        let path = path.as_ref().to_path_buf();
        let file = open_new_destination(&path)?;
        let mut destination = OwnedDestination::new(path);

        // Only the caller that successfully created this path may clean it up.
        // A concurrent caller receives DestinationExists and can never remove or
        // truncate the first caller's mapped file.
        let result = self.download_into_file(file, threads, progress);
        if result.is_ok() {
            destination.persist();
        }
        result
    }

    fn download_into_file<P>(&self, file: File, threads: u64, progress: &P) -> Result<()>
    where
        P: DownloadProgress,
    {
        self.init_download()?;
        wait_until_active(progress)?;

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
                let end = if is_last {
                    None
                } else {
                    Some(start + chunk_size - 1)
                };
                queue.push_back(Chunk { buf, start, end });
            }
        }

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
            abort: AtomicBool::new(false),
        };

        thread::scope(|s| {
            let pool = &pool;
            let client = self;
            for _ in 0..n_workers {
                s.spawn(move || run_worker(pool, client, progress));
                thread::sleep(Duration::from_millis(100));
            }
        });

        let pool_error = {
            let mut state = lock_unpoisoned(&pool.inner);
            state.error.take()
        };
        drop(pool);

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
        file.sync_all()?;

        progress.println("Verifying decrypted firmware archive and checksums...".to_string());
        verify_firmware_archive(&file, progress)?;
        wait_until_active(progress)?;
        Ok(())
    }
}

fn open_new_destination(path: &Path) -> Result<File> {
    match OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .open(path)
    {
        Ok(file) => Ok(file),
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            Err(Error::DestinationExists {
                path: path.display().to_string(),
            })
        }
        Err(error) => Err(error.into()),
    }
}

/// Removes a staging file on errors and unwinding, but only after this caller
/// has created it with `create_new`. Successful downloads disarm the guard so
/// the caller can atomically install the verified file.
struct OwnedDestination {
    path: std::path::PathBuf,
    persist: bool,
}

impl OwnedDestination {
    fn new(path: std::path::PathBuf) -> Self {
        Self {
            path,
            persist: false,
        }
    }

    fn persist(&mut self) {
        self.persist = true;
    }
}

impl Drop for OwnedDestination {
    fn drop(&mut self) {
        if !self.persist {
            let _ = fs::remove_file(&self.path);
        }
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
    /// Set as soon as any worker records a terminal error. Peer workers check
    /// it between reads, requests, and backoff intervals so they stop touching
    /// the shared mapping as quickly as blocking I/O permits.
    abort: AtomicBool,
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
    Stalled { decrypted: usize, cause: String },
    /// The caller requested cancellation.
    Cancelled,
    /// The server returned a response that cannot safely satisfy this range.
    Failed(Error),
    /// A peer worker has already recorded the terminal error.
    Aborted,
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
        if pool.abort.load(Ordering::Acquire) {
            let mut state = lock_unpoisoned(&pool.inner);
            state.live -= 1;
            pool.available.notify_all();
            return;
        }
        if wait_until_active(progress).is_err() {
            pool.abort.store(true, Ordering::Release);
            let mut state = lock_unpoisoned(&pool.inner);
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
            let mut state = lock_unpoisoned(&pool.inner);
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
                state = pool
                    .available
                    .wait(state)
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
            }
        };

        let outcome = download_chunk(
            client,
            &mut chunk.buf[..],
            chunk.start,
            chunk.end,
            progress,
            &pool.abort,
        );

        match outcome {
            ChunkOutcome::Done => {
                dead_stalls = 0;
                let mut state = lock_unpoisoned(&pool.inner);
                state.in_flight -= 1;
                // A completion can be the event that drains the pool; wake idle
                // peers so they can observe it and exit.
                pool.available.notify_all();
            }
            ChunkOutcome::Stalled { decrypted, cause } => {
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

                let mut state = lock_unpoisoned(&pool.inner);

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
                    pool.abort.store(true, Ordering::Release);
                    let mut state = lock_unpoisoned(&pool.inner);
                    state.error = Some(crate::Error::RetryExhausted {
                        offset: stall_off,
                        cause,
                    });
                    state.live -= 1;
                    pool.available.notify_all();
                    return;
                }
            }
            ChunkOutcome::Cancelled => {
                pool.abort.store(true, Ordering::Release);
                let mut state = lock_unpoisoned(&pool.inner);
                state.in_flight -= 1;
                state.live -= 1;
                if state.error.is_none() {
                    state.error = Some(Error::Cancelled);
                }
                pool.available.notify_all();
                return;
            }
            ChunkOutcome::Failed(error) => {
                pool.abort.store(true, Ordering::Release);
                let mut state = lock_unpoisoned(&pool.inner);
                state.in_flight -= 1;
                state.live -= 1;
                if state.error.is_none() {
                    state.error = Some(error);
                }
                pool.available.notify_all();
                return;
            }
            ChunkOutcome::Aborted => {
                let mut state = lock_unpoisoned(&pool.inner);
                state.in_flight -= 1;
                state.live -= 1;
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
    abort: &AtomicBool,
) -> ChunkOutcome {
    let mut dec = match client.try_get_decryptor() {
        Ok(decryptor) => decryptor,
        Err(error) => return ChunkOutcome::Failed(error),
    };
    let mut dec_pos = 0_usize; // bytes decrypted so far (always a multiple of 16)
    let mut retries = 0_u32;

    loop {
        if abort.load(Ordering::Acquire) {
            return ChunkOutcome::Aborted;
        }
        if wait_until_active(progress).is_err() {
            return ChunkOutcome::Cancelled;
        }

        let mut resp = match client.download_file_checked(Some(start + dec_pos as u64), end) {
            Ok(resp) => resp,
            Err(error) if is_retryable_download_error(&error) => {
                let cause = error.to_string();
                retries += 1;
                if retries > MAX_STALL_RETRIES {
                    return ChunkOutcome::Stalled {
                        decrypted: dec_pos,
                        cause,
                    };
                }
                progress.println_verbose(format!(
                    "Request error ({error}); retry {retries}/{MAX_STALL_RETRIES} at offset {}",
                    start + dec_pos as u64
                ));
                if interruptible_sleep(backoff(retries), progress, abort).is_err() {
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
            if abort.load(Ordering::Acquire) {
                return ChunkOutcome::Aborted;
            }
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
            return ChunkOutcome::Stalled {
                decrypted: dec_pos,
                cause: stall.to_string(),
            };
        }
        progress.println_verbose(format!(
            "Download error ({stall}); retry {retries}/{MAX_STALL_RETRIES}, \
             resuming at offset {}",
            start + dec_pos as u64
        ));
        if interruptible_sleep(backoff(retries), progress, abort).is_err() {
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

fn is_retryable_download_error(error: &Error) -> bool {
    match error {
        Error::Network(error) => error.is_connect() || error.is_timeout() || error.is_body(),
        Error::HttpStatus { retryable, .. } => *retryable,
        _ => false,
    }
}

fn lock_unpoisoned<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
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
    if !info.size.is_multiple_of(16) {
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

/// Opens and fully reads every file in the decrypted ZIP. `zip` validates each
/// entry's CRC32 at EOF; Samsung `.tar.md5` members receive an additional
/// streaming verification of their appended legacy MD5 footer. This catches
/// corruption anywhere in the decrypted payload without loading multi-gigabyte
/// entries into memory.
fn verify_firmware_archive(file: &File, progress: &impl DownloadProgress) -> Result<()> {
    let archive_file = file.try_clone()?;
    let mut archive = zip::ZipArchive::new(archive_file).map_err(|error| Error::Integrity {
        message: format!("decrypted payload is not a valid ZIP archive: {error}"),
    })?;
    if archive.is_empty() {
        return Err(Error::Integrity {
            message: "decrypted ZIP archive contained no entries".to_string(),
        });
    }

    let mut files = 0_usize;
    let mut firmware_packages = 0_usize;
    for index in 0..archive.len() {
        wait_until_active(progress)?;
        let mut entry = archive.by_index(index).map_err(|error| Error::Integrity {
            message: format!("could not open ZIP entry #{index}: {error}"),
        })?;
        if entry.is_dir() {
            continue;
        }

        files += 1;
        let name = entry.name().to_string();
        let lower_name = name.to_ascii_lowercase();
        let is_tar_md5 = lower_name.ends_with(".tar.md5");
        if is_tar_md5 || lower_name.ends_with(".tar") {
            firmware_packages += 1;
        }

        let expected_size = entry.size();
        let mut total = 0_u64;
        let mut buffer = [0_u8; 128 * 1024];
        let mut md5 = is_tar_md5.then(fast_md5::Md5::new);
        let mut md5_tail = Vec::with_capacity(512);
        loop {
            wait_until_active(progress)?;
            let count = entry.read(&mut buffer).map_err(|error| Error::Integrity {
                message: format!("ZIP checksum or data error in {name:?}: {error}"),
            })?;
            if count == 0 {
                break;
            }
            total = total
                .checked_add(count as u64)
                .ok_or_else(|| Error::Integrity {
                    message: format!("uncompressed size overflow in ZIP entry {name:?}"),
                })?;
            if let Some(hasher) = md5.as_mut() {
                update_md5_with_trailing_window(hasher, &mut md5_tail, &buffer[..count]);
            }
        }

        if total != expected_size {
            return Err(Error::Integrity {
                message: format!(
                    "ZIP entry {name:?} produced {total} bytes, expected {expected_size}"
                ),
            });
        }
        if let Some(hasher) = md5 {
            verify_tar_md5_footer(&name, total, hasher, &md5_tail)?;
        }
    }

    if files == 0 {
        return Err(Error::Integrity {
            message: "decrypted ZIP archive contained no files".to_string(),
        });
    }
    if firmware_packages == 0 {
        return Err(Error::Integrity {
            message: "decrypted ZIP archive contained no .tar or .tar.md5 firmware packages"
                .to_string(),
        });
    }
    Ok(())
}

fn update_md5_with_trailing_window(hasher: &mut fast_md5::Md5, tail: &mut Vec<u8>, data: &[u8]) {
    const WINDOW: usize = 512;
    if data.len() >= WINDOW {
        hasher.update(tail);
        hasher.update(&data[..data.len() - WINDOW]);
        tail.clear();
        tail.extend_from_slice(&data[data.len() - WINDOW..]);
        return;
    }

    tail.extend_from_slice(data);
    if tail.len() > WINDOW {
        let excess = tail.len() - WINDOW;
        hasher.update(&tail[..excess]);
        tail.drain(..excess);
    }
}

fn verify_tar_md5_footer(
    name: &str,
    total_size: u64,
    mut hasher: fast_md5::Md5,
    tail: &[u8],
) -> Result<()> {
    if total_size < 34 {
        return Err(Error::Integrity {
            message: format!("{name:?} is too small to contain a Samsung MD5 footer"),
        });
    }
    let null_index = tail
        .iter()
        .rposition(|byte| *byte == 0)
        .ok_or_else(|| Error::Integrity {
            message: format!("{name:?} has no TAR null separator before its MD5 footer"),
        })?;
    let footer_text =
        std::str::from_utf8(&tail[null_index + 1..]).map_err(|error| Error::Integrity {
            message: format!("{name:?} has a non-text MD5 footer: {error}"),
        })?;
    let footer_line = footer_text.lines().last().ok_or_else(|| Error::Integrity {
        message: format!("{name:?} has an empty MD5 footer"),
    })?;
    let expected_hex = footer_line
        .as_bytes()
        .get(..32)
        .ok_or_else(|| Error::Integrity {
            message: format!("{name:?} has a truncated MD5 footer"),
        })?;
    let mut expected = [0_u8; 16];
    for (index, pair) in expected_hex.chunks_exact(2).enumerate() {
        let pair = std::str::from_utf8(pair).map_err(|error| Error::Integrity {
            message: format!("{name:?} has an invalid MD5 footer: {error}"),
        })?;
        expected[index] = u8::from_str_radix(pair, 16).map_err(|error| Error::Integrity {
            message: format!("{name:?} has an invalid MD5 footer: {error}"),
        })?;
    }

    let payload_size = total_size
        .checked_sub(footer_line.len() as u64 + 1)
        .ok_or_else(|| Error::Integrity {
            message: format!("{name:?} has an invalid MD5 footer position"),
        })?;
    let already_hashed = total_size.saturating_sub(tail.len() as u64);
    let tail_payload_len = payload_size
        .checked_sub(already_hashed)
        .and_then(|length| usize::try_from(length).ok())
        .filter(|length| *length <= tail.len())
        .ok_or_else(|| Error::Integrity {
            message: format!("{name:?} has an inconsistent MD5 footer boundary"),
        })?;
    hasher.update(&tail[..tail_payload_len]);
    if hasher.finalize() != expected {
        return Err(Error::Integrity {
            message: format!("legacy MD5 footer mismatch in {name:?}"),
        });
    }
    Ok(())
}

/// Sleeps in short increments so cancellation remains responsive during retry
/// backoff. Pause time is handled by the progress implementation.
fn interruptible_sleep(
    duration: Duration,
    progress: &impl DownloadProgress,
    abort: &AtomicBool,
) -> Result<()> {
    let mut remaining = duration;
    const POLL_INTERVAL: Duration = Duration::from_millis(100);

    while !remaining.is_zero() {
        if abort.load(Ordering::Acquire) {
            return Err(Error::Cancelled);
        }
        wait_until_active(progress)?;
        let interval = remaining.min(POLL_INTERVAL);
        thread::sleep(interval);
        remaining = remaining.saturating_sub(interval);
    }

    if abort.load(Ordering::Acquire) {
        Err(Error::Cancelled)
    } else {
        wait_until_active(progress)
    }
}

/// Exponential backoff capped at 30s: 1s, 2s, 4s, 8s, 16s, 30s, ...
fn backoff(attempt: u32) -> Duration {
    Duration::from_secs((1u64 << attempt.saturating_sub(1).min(5)).min(30))
}

#[cfg(test)]
mod tests {
    use super::*;
    use aes::cipher::inout::InOutBuf;
    use aes::cipher::{BlockModeEncrypt, KeyInit};
    use std::io::Write;
    use std::net::TcpListener;
    use std::sync::{Arc, Mutex as StdMutex};
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_path(test_name: &str, extension: &str) -> std::path::PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!(
            "samloader-fus-{test_name}-{}-{nonce}.{extension}",
            std::process::id()
        ))
    }

    fn tar_md5_payload(valid_footer: bool) -> Vec<u8> {
        let mut payload = vec![0_u8; 1024];
        payload[..8].copy_from_slice(b"firmware");
        let digest = fast_md5::digest(&payload);
        let mut footer = digest
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        if !valid_footer {
            footer.replace_range(..1, if footer.starts_with('0') { "1" } else { "0" });
        }
        payload.push(b'\n');
        payload.extend_from_slice(footer.as_bytes());
        payload
    }

    fn write_test_archive(path: &Path, valid_footer: bool) {
        let file = File::create(path).unwrap();
        let mut writer = zip::ZipWriter::new(file);
        let options = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Stored);
        writer.start_file("AP_test.tar.md5", options).unwrap();
        writer.write_all(&tar_md5_payload(valid_footer)).unwrap();
        writer.finish().unwrap();
    }

    fn encrypt_test_data(key: &[u8; 16], plaintext: &[u8]) -> Vec<u8> {
        assert_eq!(plaintext.len() % 16, 0);
        let mut ciphertext = plaintext.to_vec();
        let mut cipher = ecb::Encryptor::<aes::Aes128>::new(key.into());
        let (blocks, tail) = InOutBuf::from(ciphertext.as_mut_slice()).into_chunks();
        assert!(tail.is_empty());
        cipher.encrypt_blocks_inout(blocks);
        ciphertext
    }

    fn mock_range_server(
        responses: Vec<(u16, Vec<u8>)>,
        total: u64,
    ) -> (String, Arc<StdMutex<Vec<String>>>, thread::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let endpoint = format!("http://{}/download", listener.local_addr().unwrap());
        let ranges = Arc::new(StdMutex::new(Vec::new()));
        let recorded = Arc::clone(&ranges);
        let handle = thread::spawn(move || {
            for (status, body) in responses {
                let (mut stream, _) = listener.accept().unwrap();
                let mut request = Vec::new();
                let mut buffer = [0_u8; 2048];
                while !request.windows(4).any(|window| window == b"\r\n\r\n") {
                    let count = stream.read(&mut buffer).unwrap();
                    if count == 0 {
                        break;
                    }
                    request.extend_from_slice(&buffer[..count]);
                }
                let request = String::from_utf8(request).unwrap();
                let range = request
                    .lines()
                    .find_map(|line| {
                        line.strip_prefix("range: ")
                            .or_else(|| line.strip_prefix("Range: "))
                    })
                    .unwrap_or("")
                    .to_string();
                recorded.lock().unwrap().push(range.clone());
                let reason = match status {
                    206 => "Partial Content",
                    404 => "Not Found",
                    503 => "Service Unavailable",
                    _ => "Test",
                };
                let mut headers = format!(
                    "HTTP/1.1 {status} {reason}\r\nContent-Length: {}\r\nConnection: close\r\n",
                    body.len()
                );
                if status == 206 {
                    let range = range.strip_prefix("bytes=").unwrap();
                    headers.push_str(&format!("Content-Range: bytes {range}/{total}\r\n"));
                }
                headers.push_str("\r\n");
                stream.write_all(headers.as_bytes()).unwrap();
                stream.write_all(&body).unwrap();
            }
        });
        (endpoint, ranges, handle)
    }

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
    fn existing_destination_is_never_truncated() {
        let path = temp_path("exclusive", "part");
        fs::write(&path, b"keep me").unwrap();

        let error = open_new_destination(&path).unwrap_err();

        assert!(matches!(error, Error::DestinationExists { .. }));
        assert_eq!(fs::read(&path).unwrap(), b"keep me");
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn owned_destination_is_removed_on_unwind_but_persisted_on_success() {
        let panic_path = temp_path("panic-cleanup", "part");
        let file = open_new_destination(&panic_path).unwrap();
        drop(file);
        let unwind_path = panic_path.clone();
        let result = std::panic::catch_unwind(move || {
            let _owned = OwnedDestination::new(unwind_path);
            panic!("simulated download panic");
        });
        assert!(result.is_err());
        assert!(!panic_path.exists());

        let success_path = temp_path("successful-staging", "part");
        let file = open_new_destination(&success_path).unwrap();
        drop(file);
        let mut owned = OwnedDestination::new(success_path.clone());
        owned.persist();
        drop(owned);
        assert!(success_path.exists());
        fs::remove_file(success_path).unwrap();
    }

    #[test]
    fn decrypted_archive_and_nested_tar_md5_are_verified() {
        let valid = temp_path("valid-archive", "zip");
        write_test_archive(&valid, true);
        let file = File::open(&valid).unwrap();
        verify_firmware_archive(&file, &()).unwrap();
        fs::remove_file(valid).unwrap();

        let invalid = temp_path("invalid-md5", "zip");
        write_test_archive(&invalid, false);
        let file = File::open(&invalid).unwrap();
        let error = verify_firmware_archive(&file, &()).unwrap_err();
        assert!(matches!(error, Error::Integrity { .. }));
        assert!(error.to_string().contains("MD5"));
        fs::remove_file(invalid).unwrap();
    }

    #[test]
    fn malformed_or_unrelated_zip_payload_is_rejected() {
        let malformed = temp_path("malformed", "zip");
        fs::write(&malformed, b"not a zip").unwrap();
        let error = verify_firmware_archive(&File::open(&malformed).unwrap(), &()).unwrap_err();
        assert!(matches!(error, Error::Integrity { .. }));
        fs::remove_file(malformed).unwrap();

        let unrelated = temp_path("unrelated", "zip");
        let mut writer = zip::ZipWriter::new(File::create(&unrelated).unwrap());
        writer
            .start_file("notes.txt", zip::write::SimpleFileOptions::default())
            .unwrap();
        writer.write_all(b"hello").unwrap();
        writer.finish().unwrap();
        let error = verify_firmware_archive(&File::open(&unrelated).unwrap(), &()).unwrap_err();
        assert!(error.to_string().contains("firmware packages"));
        fs::remove_file(unrelated).unwrap();
    }

    #[test]
    fn retryable_http_status_retries_same_range_and_decrypts_body() {
        let key = [0x2a_u8; 16];
        let plaintext = *b"0123456789abcdefFEDCBA9876543210";
        let ciphertext = encrypt_test_data(&key, &plaintext);
        let (endpoint, ranges, server) = mock_range_server(
            vec![(503, Vec::new()), (206, ciphertext)],
            plaintext.len() as u64,
        );
        let info = crate::BinaryInform {
            filename: "firmware.zip.enc4".to_string(),
            path: "/fixture/".to_string(),
            size: plaintext.len() as u64,
            key: key.to_vec(),
            ..Default::default()
        };
        let client = FusClient::new_for_download_test(endpoint, info).unwrap();
        let mut output = vec![0_u8; plaintext.len()];
        let abort = AtomicBool::new(false);

        let outcome = download_chunk(&client, &mut output, 0, Some(31), &(), &abort);

        assert!(matches!(outcome, ChunkOutcome::Done));
        assert_eq!(output, plaintext);
        server.join().unwrap();
        assert_eq!(
            *ranges.lock().unwrap(),
            ["bytes=0-31".to_string(), "bytes=0-31".to_string()]
        );
    }

    #[test]
    fn permanent_http_status_is_not_retried_and_keeps_status() {
        let (endpoint, ranges, server) = mock_range_server(vec![(404, Vec::new())], 16);
        let info = crate::BinaryInform {
            filename: "missing.zip.enc4".to_string(),
            path: "/fixture/".to_string(),
            size: 16,
            key: vec![0; 16],
            ..Default::default()
        };
        let client = FusClient::new_for_download_test(endpoint, info).unwrap();
        let mut output = [0_u8; 16];
        let abort = AtomicBool::new(false);

        let outcome = download_chunk(&client, &mut output, 0, Some(15), &(), &abort);

        assert!(matches!(
            outcome,
            ChunkOutcome::Failed(Error::HttpStatus {
                status: 404,
                retryable: false,
                ..
            })
        ));
        server.join().unwrap();
        assert_eq!(*ranges.lock().unwrap(), ["bytes=0-15".to_string()]);
    }

    #[test]
    fn peer_abort_stops_before_starting_network_io() {
        let info = crate::BinaryInform {
            size: 16,
            key: vec![0; 16],
            ..Default::default()
        };
        let client =
            FusClient::new_for_download_test("http://127.0.0.1:1/unreachable".to_string(), info)
                .unwrap();
        let mut output = [0_u8; 16];
        let abort = AtomicBool::new(true);

        assert!(matches!(
            download_chunk(&client, &mut output, 0, Some(15), &(), &abort),
            ChunkOutcome::Aborted
        ));
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
