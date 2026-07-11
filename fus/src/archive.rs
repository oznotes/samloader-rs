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

//! Inspection of Samsung firmware ZIP archives for direct-from-ZIP flashing.
//!
//! Samsung firmware ZIPs store their `.tar.md5` members without compression,
//! so every member's bytes occupy one contiguous range of the archive file.
//! This module exposes those ranges so a flasher can memory-map partition
//! payloads in place instead of extracting the archive.

use crate::error::{Error, Result};
use std::fs::File;
use std::io::{Read, Seek};

/// A single file member of a Samsung firmware ZIP archive.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FirmwareZipEntry {
    /// Member name exactly as stored in the archive (always a flat file name).
    pub name: String,
    /// Absolute byte offset of the member's data within the ZIP file.
    pub data_start: u64,
    /// Uncompressed size of the member in bytes.
    pub size: u64,
    /// Whether the member is stored without compression (method 0, STORE).
    /// Only stored members can be flashed in place.
    pub is_stored: bool,
}

/// Lists the file members of a firmware ZIP archive with their raw data offsets.
///
/// Fails closed if the archive is not a valid ZIP, contains no file entries,
/// or contains a member whose name is not a flat file name (path separators,
/// parent references, or drive prefixes) — Samsung firmware archives only
/// contain flat `.tar.md5` members, so anything else is treated as hostile.
pub fn list_firmware_zip_entries(file: &File) -> Result<Vec<FirmwareZipEntry>> {
    let reader = file.try_clone()?;
    list_zip_entries_from_reader(reader)
}

fn list_zip_entries_from_reader<R: Read + Seek>(reader: R) -> Result<Vec<FirmwareZipEntry>> {
    let mut archive = zip::ZipArchive::new(reader).map_err(|error| Error::Integrity {
        message: format!("file is not a valid ZIP archive: {error}"),
    })?;

    let mut entries = Vec::new();
    for index in 0..archive.len() {
        let entry = archive
            .by_index_raw(index)
            .map_err(|error| Error::Integrity {
                message: format!("failed to read ZIP entry {index}: {error}"),
            })?;
        if entry.is_dir() {
            continue;
        }
        let name = entry.name().to_string();
        // The `..` clause is a deliberate fail-closed over-approximation: it
        // rejects any name containing two consecutive dots, not only true parent
        // references. It can only reject more names, never accept a traversal,
        // and real Samsung firmware members never contain `..`.
        if name.contains('/') || name.contains('\\') || name.contains("..") || name.contains(':') {
            return Err(Error::Integrity {
                message: format!("ZIP member {name:?} is not a flat file name."),
            });
        }
        entries.push(FirmwareZipEntry {
            name,
            data_start: entry.data_start(),
            size: entry.size(),
            is_stored: matches!(entry.compression(), zip::CompressionMethod::Stored),
        });
    }

    if entries.is_empty() {
        return Err(Error::Integrity {
            message: "ZIP archive contains no file entries.".to_string(),
        });
    }

    Ok(entries)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Seek, SeekFrom, Write};

    fn temp_zip_path(name: &str) -> std::path::PathBuf {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!(
            "fus-archive-{name}-{}-{nonce}.zip",
            std::process::id()
        ))
    }

    fn write_zip(path: &std::path::Path, entries: &[(&str, &[u8], zip::CompressionMethod)]) {
        let file = std::fs::File::create(path).unwrap();
        let mut writer = zip::ZipWriter::new(file);
        for (name, bytes, method) in entries {
            let options = zip::write::SimpleFileOptions::default().compression_method(*method);
            writer.start_file(*name, options).unwrap();
            writer.write_all(bytes).unwrap();
        }
        writer.finish().unwrap();
    }

    #[test]
    fn stored_members_are_listed_with_correct_raw_offsets() {
        let path = temp_zip_path("stored-offsets");
        let ap_payload = b"ap payload bytes".as_slice();
        let csc_payload = b"csc bytes".as_slice();
        write_zip(
            &path,
            &[
                (
                    "AP_test.tar.md5",
                    ap_payload,
                    zip::CompressionMethod::Stored,
                ),
                (
                    "HOME_CSC_test.tar.md5",
                    csc_payload,
                    zip::CompressionMethod::Stored,
                ),
            ],
        );

        let file = std::fs::File::open(&path).unwrap();
        let entries = list_firmware_zip_entries(&file).unwrap();
        assert_eq!(entries.len(), 2);

        // The listed (data_start, size) range must contain exactly the member bytes.
        let mut reader = file.try_clone().unwrap();
        for (entry, payload) in entries.iter().zip([ap_payload, csc_payload]) {
            assert!(entry.is_stored);
            assert_eq!(entry.size, payload.len() as u64);
            reader.seek(SeekFrom::Start(entry.data_start)).unwrap();
            let mut got = vec![0u8; payload.len()];
            reader.read_exact(&mut got).unwrap();
            assert_eq!(got, payload);
        }
        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn deflated_members_are_flagged_not_stored() {
        let path = temp_zip_path("deflated-flag");
        write_zip(
            &path,
            &[(
                "AP_test.tar.md5",
                &[0u8; 4096],
                zip::CompressionMethod::Deflated,
            )],
        );
        let file = std::fs::File::open(&path).unwrap();
        let entries = list_firmware_zip_entries(&file).unwrap();
        assert_eq!(entries.len(), 1);
        assert!(!entries[0].is_stored);
        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn nested_member_names_fail_closed() {
        let path = temp_zip_path("nested-name");
        write_zip(
            &path,
            &[(
                "nested/AP_test.tar.md5",
                b"x".as_slice(),
                zip::CompressionMethod::Stored,
            )],
        );
        let file = std::fs::File::open(&path).unwrap();
        let error = list_firmware_zip_entries(&file).unwrap_err();
        assert!(error.to_string().contains("flat file name"));
        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn non_zip_and_empty_archives_fail_closed() {
        let path = temp_zip_path("not-a-zip");
        std::fs::write(&path, b"this is not a zip").unwrap();
        let file = std::fs::File::open(&path).unwrap();
        assert!(list_firmware_zip_entries(&file).is_err());
        std::fs::remove_file(&path).unwrap();

        let path = temp_zip_path("empty-zip");
        write_zip(&path, &[]);
        let file = std::fs::File::open(&path).unwrap();
        let error = list_firmware_zip_entries(&file).unwrap_err();
        assert!(error.to_string().contains("no file entries"));
        std::fs::remove_file(&path).unwrap();
    }
}
