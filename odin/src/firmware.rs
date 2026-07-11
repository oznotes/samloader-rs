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

use fast_md5::Md5;
use lz4_flex::frame::FrameDecoder;
use memmap2::Mmap;
use samloader_pit::PitEntry;
use std::io::{self, Read, Seek, SeekFrom};

// This is asserted by Lz4FrameHeader so the header size is always 15
const LZ4_HEADER_SIZE: usize = 15;

/// Verifies the trailing MD5 checksum appended to `.tar.md5` package files.
pub fn verify_md5_footer<R: Read + Seek>(mut reader: R) -> io::Result<()> {
    let file_size = reader.seek(SeekFrom::End(0))?;

    if file_size < 34 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "File is too small to contain a valid MD5 footer",
        ));
    }

    // Read the last 512 bytes of the file. Since the TAR file must end with at least
    // two blocks of zeroes, the last null byte (0x00) marks the exact boundary
    // between the TAR payload and the appended plain-text MD5 footer.
    let seek_pos = file_size.saturating_sub(512);
    reader.seek(SeekFrom::Start(seek_pos))?;
    let mut last_bytes = vec![0u8; (file_size - seek_pos) as usize];
    reader.read_exact(&mut last_bytes)?;

    // Find the last null byte (0x00)
    let mut last_null_idx = None;
    for i in (0..last_bytes.len()).rev() {
        if last_bytes[i] == 0 {
            last_null_idx = Some(i);
            break;
        }
    }

    let Some(null_idx) = last_null_idx else {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "Could not find a valid null separator for the MD5 footer",
        ));
    };

    let footer_start = null_idx + 1;
    let footer_bytes = &last_bytes[footer_start..];
    let footer_str = String::from_utf8_lossy(footer_bytes);
    let footer_line = footer_str.lines().last().unwrap_or_default();

    if footer_line.len() < 32 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "Could not find a valid MD5 checksum at the end of the file",
        ));
    }

    let expected_hex = &footer_line[..32];
    let mut expected_bytes = [0u8; 16];
    for i in 0..16 {
        let hex_byte = &expected_hex[i * 2..i * 2 + 2];
        expected_bytes[i] = u8::from_str_radix(hex_byte, 16)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    }

    // The payload size is the exact position up to the MD5 footer text
    let payload_size = file_size - footer_line.len() as u64 - 1;

    // Reset file pointer and compute MD5 over the payload only
    reader.seek(SeekFrom::Start(0))?;
    let mut hasher = Md5::new();
    let mut buffer = [0u8; 128 * 1024];
    let mut remaining = payload_size;

    while remaining > 0 {
        let to_read = std::cmp::min(remaining, buffer.len() as u64) as usize;
        reader.read_exact(&mut buffer[..to_read])?;
        hasher.update(&buffer[..to_read]);
        remaining -= to_read as u64;
    }

    let calculated_digest = hasher.finalize();
    if calculated_digest != expected_bytes {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "MD5 verification failed! File is corrupted or modified.",
        ));
    }

    Ok(())
}

/// Header containing metadata parsed from an LZ4 frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Lz4FrameHeader {
    /// Total size of the decompressed content in bytes.
    pub content_size: u64,
    /// Maximum size of a block in bytes.
    pub block_max_size: u64,
}

impl Lz4FrameHeader {
    /// Parses the LZ4 frame header from a reader.
    pub fn parse<R: Read>(mut reader: R) -> Result<Self, String> {
        let mut magic_bytes = [0u8; 4];
        reader
            .read_exact(&mut magic_bytes)
            .map_err(|_| "Failed to read magic number")?;
        let magic = u32::from_le_bytes(magic_bytes);

        if magic != 0x184D2204 {
            // We only support the standard LZ4 frame magic in this context,
            // not the skippable frames (0x184D2A50 - 0x184D2A5F)
            return Err(format!("Not a valid LZ4 frame. Magic: 0x{:08X}", magic));
        }

        let mut flg_byte = [0u8; 1];
        reader
            .read_exact(&mut flg_byte)
            .map_err(|_| "Failed to read FLG byte")?;
        let flg = flg_byte[0];

        let version = (flg >> 6) & 0x03;
        if version != 1 {
            return Err(format!("Unsupported LZ4 version: {}", version));
        }

        let block_independence = ((flg >> 5) & 0x01) == 1;
        let block_checksum = ((flg >> 4) & 0x01) == 1;
        let content_size_flag = ((flg >> 3) & 0x01) == 1;
        let dict_id_flag = (flg & 0x01) == 1;

        if !content_size_flag {
            return Err("LZ4 content size must be enabled".to_string());
        }
        if block_checksum {
            return Err("LZ4 block checksum must be disabled".to_string());
        }
        if !block_independence {
            return Err("LZ4 block independence must be enabled".to_string());
        }
        if dict_id_flag {
            return Err("LZ4 dictionary ID must be disabled".to_string());
        }

        let mut bd_byte = [0u8; 1];
        reader
            .read_exact(&mut bd_byte)
            .map_err(|_| "Failed to read BD byte")?;
        let bd = bd_byte[0];

        let block_max_size_code = (bd >> 4) & 0x07;
        let block_max_size = match block_max_size_code {
            4 => 64 * 1024,
            5 => 256 * 1024,
            6 => 1024 * 1024,
            7 => 4 * 1024 * 1024,
            _ => {
                return Err(format!(
                    "Invalid block max size code: {}",
                    block_max_size_code
                ));
            }
        };

        let mut content_size_bytes = [0u8; 8];
        reader
            .read_exact(&mut content_size_bytes)
            .map_err(|_| "Failed to read content size")?;
        let content_size = u64::from_le_bytes(content_size_bytes);

        let mut hc_byte = [0u8; 1];
        reader
            .read_exact(&mut hc_byte)
            .map_err(|_| "Failed to read header checksum")?;

        Ok(Self {
            content_size,
            block_max_size,
        })
    }
}

/// Fully validates one Samsung-compatible LZ4 frame and returns its parsed header.
///
/// Validation decodes the complete payload, verifies the declared decompressed
/// content size and any enabled checksums, requires an explicit frame end marker,
/// and rejects trailing bytes. This is intended as a preflight check before any
/// firmware bytes are sent to a device.
pub fn validate_lz4_frame(frame: &[u8]) -> Result<Lz4FrameHeader, String> {
    let header = Lz4FrameHeader::parse(frame)
        .map_err(|error| format!("Invalid LZ4 frame header: {error}"))?;
    if header.content_size == 0 {
        return Err("LZ4 firmware frame declares an empty decompressed payload".to_string());
    }

    validate_lz4_frame_structure(frame, &header)?;

    let mut decoder = FrameDecoder::new(frame);
    let mut buffer = [0_u8; 128 * 1024];
    let mut decompressed_size = 0_u64;
    loop {
        let bytes_read = decoder
            .read(&mut buffer)
            .map_err(|error| format!("LZ4 decompression failed: {error}"))?;
        if bytes_read == 0 {
            break;
        }
        decompressed_size = decompressed_size
            .checked_add(bytes_read as u64)
            .ok_or_else(|| "LZ4 decompressed size overflowed u64".to_string())?;
    }

    if decompressed_size != header.content_size {
        return Err(format!(
            "LZ4 decompressed content size mismatch: header declares {} bytes but the frame produced {} bytes",
            header.content_size, decompressed_size
        ));
    }

    Ok(header)
}

fn validate_lz4_frame_structure(frame: &[u8], header: &Lz4FrameHeader) -> Result<(), String> {
    if frame.len() < LZ4_HEADER_SIZE {
        return Err("LZ4 frame is truncated before the complete header".to_string());
    }

    let block_checksums = frame[4] & 0x10 != 0;
    let content_checksum = frame[4] & 0x04 != 0;
    let block_max_size = usize::try_from(header.block_max_size)
        .map_err(|_| "LZ4 block maximum size does not fit this platform".to_string())?;
    let mut offset = LZ4_HEADER_SIZE;

    loop {
        let block_header_end = offset
            .checked_add(4)
            .ok_or_else(|| "LZ4 block header offset overflowed".to_string())?;
        let block_header = frame.get(offset..block_header_end).ok_or_else(|| {
            "LZ4 frame is truncated before a complete block header or end marker".to_string()
        })?;
        let block_size = u32::from_le_bytes(block_header.try_into().unwrap());
        offset = block_header_end;

        if block_size == 0 {
            if content_checksum {
                offset = offset
                    .checked_add(4)
                    .ok_or_else(|| "LZ4 content checksum offset overflowed".to_string())?;
                if offset > frame.len() {
                    return Err("LZ4 frame is truncated in its content checksum".to_string());
                }
            }

            if offset != frame.len() {
                return Err(format!(
                    "LZ4 frame contains {} trailing byte(s) after its end marker",
                    frame.len() - offset
                ));
            }
            return Ok(());
        }

        let data_size = (block_size & 0x7fff_ffff) as usize;
        if data_size == 0 {
            return Err("LZ4 frame contains an invalid zero-length data block".to_string());
        }
        if data_size > block_max_size {
            return Err(format!(
                "LZ4 data block is {data_size} bytes, exceeding the declared {block_max_size}-byte maximum"
            ));
        }

        offset = offset
            .checked_add(data_size)
            .ok_or_else(|| "LZ4 data block offset overflowed".to_string())?;
        if offset > frame.len() {
            return Err("LZ4 frame is truncated in a data block".to_string());
        }

        if block_checksums {
            offset = offset
                .checked_add(4)
                .ok_or_else(|| "LZ4 block checksum offset overflowed".to_string())?;
            if offset > frame.len() {
                return Err("LZ4 frame is truncated in a block checksum".to_string());
            }
        }
    }
}

/// An uncompressed firmware file mapped in memory.
pub struct FirmwareFile<'a> {
    /// PIT partition entry associated with this file.
    pub pit_entry: &'a PitEntry,
    /// Memory-mapped file content payload.
    pub file: Mmap,
}

impl<'a> FirmwareFile<'a> {
    pub(crate) fn sequences(&self, sequence_max_bytes: usize) -> std::slice::Chunks<'_, u8> {
        self.file.chunks(sequence_max_bytes)
    }
}

/// An LZ4-compressed firmware file mapped in memory.
pub struct FirmwareLz4File<'a> {
    /// PIT partition entry associated with this file.
    pub pit_entry: &'a PitEntry,
    /// Memory-mapped file content payload.
    pub file: Mmap,
    /// Metadata header of the LZ4 frame.
    pub header: Lz4FrameHeader,
}

impl<'a> FirmwareLz4File<'a> {
    /// Validates the complete LZ4 payload and confirms that its header matches
    /// the metadata stored with this firmware file.
    pub fn validate(&self) -> Result<(), String> {
        let validated_header = validate_lz4_frame(&self.file)?;
        if validated_header.content_size != self.header.content_size
            || validated_header.block_max_size != self.header.block_max_size
        {
            return Err("LZ4 firmware metadata does not match the mapped frame header".to_string());
        }
        Ok(())
    }

    pub(crate) fn sequences(&self, sequence_max_bytes: usize) -> Lz4SequenceIterator<'_> {
        Lz4SequenceIterator {
            file: &self.file,
            max_blocks: sequence_max_bytes / (1024 * 1024),
            remaining_decompressed: self.header.content_size,
            bytes_read: LZ4_HEADER_SIZE,
            finished: false,
        }
    }

    pub(crate) fn decompressed_sequences(
        &self,
        sequence_max_bytes: usize,
    ) -> Lz4DecompressedSequenceIterator<'_> {
        Lz4DecompressedSequenceIterator {
            decoder: FrameDecoder::new(&self.file[..]),
            sequence_max_bytes,
        }
    }
}

/// Enum wrapping a firmware file payload variant.
pub enum FirmwareInfo<'a> {
    /// Uncompressed normal firmware file payload.
    Normal(FirmwareFile<'a>),
    /// LZ4-compressed firmware file payload.
    Lz4(FirmwareLz4File<'a>),
}

/// Validates every compressed payload in a complete firmware plan.
///
/// Call this after PIT mapping and before uploading a replacement PIT or any
/// partition payload so a malformed later entry cannot fail after an earlier
/// partition has already been written.
pub fn validate_firmware_plan(plan: &[FirmwareInfo<'_>]) -> Result<(), String> {
    for (index, info) in plan.iter().enumerate() {
        if let FirmwareInfo::Lz4(info) = info {
            info.validate().map_err(|error| {
                format!(
                    "LZ4 preflight failed for partition {} at plan entry {}: {error}",
                    info.pit_entry.partition_name,
                    index + 1
                )
            })?;
        }
    }
    Ok(())
}

pub(crate) struct Lz4SequenceIterator<'a> {
    file: &'a Mmap,
    max_blocks: usize,
    remaining_decompressed: u64,
    bytes_read: usize,
    finished: bool,
}

impl<'a> Iterator for Lz4SequenceIterator<'a> {
    type Item = (usize, &'a [u8]);

    fn next(&mut self) -> Option<Self::Item> {
        if self.finished {
            return None;
        }

        let start_pos = self.bytes_read;
        let mut end_pos = start_pos;
        let mut num_blocks = 0;

        while num_blocks < self.max_blocks {
            if self.bytes_read + 4 > self.file.len() {
                self.finished = true;
                break;
            }
            let block_size = u32::from_le_bytes(
                self.file[self.bytes_read..self.bytes_read + 4]
                    .try_into()
                    .unwrap(),
            );

            if block_size == 0 {
                self.bytes_read += 4; // Advance past EndMark
                self.finished = true;
                break;
            }

            let data_size = (block_size & 0x7FFF_FFFF) as usize;
            if self.bytes_read + 4 + data_size > self.file.len() {
                self.finished = true;
                break;
            }

            self.bytes_read += 4 + data_size;
            end_pos = self.bytes_read;
            num_blocks += 1;
        }

        if start_pos == end_pos {
            return None;
        }

        let decompressed_size = std::cmp::min(
            self.remaining_decompressed,
            (num_blocks as u64) * 1024 * 1024,
        ) as usize;
        self.remaining_decompressed -= decompressed_size as u64;

        Some((decompressed_size, &self.file[start_pos..end_pos]))
    }
}

/// Iterator that produces decompressed byte chunks from an LZ4 stream.
pub struct Lz4DecompressedSequenceIterator<'a> {
    decoder: FrameDecoder<&'a [u8]>,
    sequence_max_bytes: usize,
}

impl<'a> Iterator for Lz4DecompressedSequenceIterator<'a> {
    type Item = Vec<u8>;

    fn next(&mut self) -> Option<Self::Item> {
        let mut buffer = vec![0u8; self.sequence_max_bytes];
        let mut total_read = 0;

        while total_read < self.sequence_max_bytes {
            match self.decoder.read(&mut buffer[total_read..]) {
                Ok(0) => break,
                Ok(n) => total_read += n,
                Err(_) => break,
            }
        }

        if total_read == 0 {
            return None;
        }

        buffer.truncate(total_read);
        Some(buffer)
    }
}

#[cfg(test)]
mod tests {
    use super::validate_lz4_frame;
    use lz4_flex::frame::{BlockSize, FrameEncoder, FrameInfo};
    use std::io::Write;

    fn encode_frame(payload: &[u8], content_checksum: bool) -> Vec<u8> {
        let frame_info = FrameInfo::new()
            .block_size(BlockSize::Max1MB)
            .content_size(Some(payload.len() as u64))
            .content_checksum(content_checksum);
        let mut encoder = FrameEncoder::with_frame_info(frame_info, Vec::new());
        encoder.write_all(payload).unwrap();
        encoder.finish().unwrap()
    }

    #[test]
    fn full_lz4_preflight_accepts_one_complete_frame() {
        let payload = vec![0x5a; 2 * 1024 * 1024 + 17];
        let frame = encode_frame(&payload, true);

        let header = validate_lz4_frame(&frame).unwrap();

        assert_eq!(header.content_size, payload.len() as u64);
        assert_eq!(header.block_max_size, 1024 * 1024);
    }

    #[test]
    fn full_lz4_preflight_rejects_truncation_and_trailing_data() {
        let mut truncated = encode_frame(&vec![0x33; 4096], false);
        truncated.pop();
        assert!(validate_lz4_frame(&truncated).is_err());

        let mut trailing = encode_frame(&vec![0x44; 4096], false);
        trailing.extend_from_slice(b"unexpected");
        let error = validate_lz4_frame(&trailing).unwrap_err();
        assert!(error.contains("trailing byte"), "{error}");
    }

    #[test]
    fn full_lz4_preflight_requires_exact_declared_content_size() {
        let mut frame = encode_frame(b"abc", false);
        let block_size = u32::from_le_bytes(frame[15..19].try_into().unwrap());
        assert_eq!(block_size, 0x8000_0003);

        // Keep a structurally complete frame but shorten its uncompressed block.
        // The original header still declares three decompressed bytes.
        frame[15..19].copy_from_slice(&0x8000_0002_u32.to_le_bytes());
        frame.remove(21);

        let error = validate_lz4_frame(&frame).unwrap_err();
        assert!(
            error.contains("ContentLengthError") || error.contains("content size mismatch"),
            "{error}"
        );
    }

    #[test]
    fn full_lz4_preflight_verifies_enabled_content_checksum() {
        let mut frame = encode_frame(b"abc", true);
        let block_size = u32::from_le_bytes(frame[15..19].try_into().unwrap());
        assert_eq!(block_size, 0x8000_0003);
        frame[19] ^= 0xff;

        let error = validate_lz4_frame(&frame).unwrap_err();
        assert!(error.contains("ContentChecksumError"), "{error}");
    }
}
