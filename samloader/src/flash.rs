// Copyright 2026 John "topjohnwu" Wu
// Copyright 2021-2024 Henrik Grimler
// Copyright 2010-2017 Benjamin Dobell, Glass Echidna
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

use crate::{FolderPackages, PartitionArg, print_error, validate_package_selection};
use memmap2::{Mmap, MmapOptions};
use samloader_odin::{
    FirmwareFile, FirmwareInfo, FirmwareLz4File, Lz4FrameHeader, OdinManager, UsbBackendOption,
    create_backend, validate_firmware_plan, verify_md5_footer,
};
use samloader_pit::{DeviceType, PitData, PitEntry};
use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;
use std::sync::Arc;
use tar::Archive;

struct IndexedEntry {
    original_name: String,
    normalized_name: String,
    mmap: Mmap,
    is_lz4: bool,
    source_file: Arc<File>,
    source_offset: u64,
    source_size: usize,
}

type PitLayoutGroup = (u32, u32);
type PitExtent = (u32, u32, String);

fn normalize_basename(path_str: &str) -> (String, bool) {
    let mut filename = Path::new(path_str)
        .file_name()
        .map(|f| f.to_string_lossy().to_string())
        .unwrap_or_default();

    if filename.to_lowercase().ends_with(".lz4") {
        filename.truncate(filename.len() - 4);
        (filename, true)
    } else {
        (filename, false)
    }
}

fn is_lz4_payload(bytes: &[u8], has_lz4_suffix: bool) -> bool {
    has_lz4_suffix || bytes.starts_with(&0x184d_2204_u32.to_le_bytes())
}

fn is_known_package_metadata(path: &str) -> bool {
    let normalized = path.replace('\\', "/").to_ascii_lowercase();
    let normalized = normalized.trim_start_matches("./");
    normalized.starts_with("meta-data/")
        || normalized.starts_with("meta-inf/")
        || normalized.starts_with("metadata/")
}

fn validate_payload_entry(
    is_file: bool,
    is_directory: bool,
    size: u64,
    path: &str,
) -> Result<bool, String> {
    if is_known_package_metadata(path) || is_directory {
        return Ok(false);
    }
    if !is_file {
        return Err(format!("Archive payload \"{path}\" is not a regular file."));
    }
    if size == 0 {
        return Err(format!("Archive payload \"{path}\" is empty."));
    }
    Ok(true)
}

const MAX_DOWNLOAD_LIST_SIZE: u64 = 1024 * 1024;

fn scan_tar_packages(
    packages: &[String],
    skip_md5: bool,
) -> Result<(Vec<IndexedEntry>, Option<Vec<u8>>), i32> {
    // Open all packages into a File first
    let mut opened_packages = Vec::new();
    for pkg in packages {
        let package_path = Path::new(pkg);
        match package_path.metadata() {
            Ok(metadata) if metadata.is_file() && metadata.len() > 0 => {}
            Ok(_) => {
                print_error!("Firmware package \"{}\" is not a non-empty file.", pkg);
                return Err(1);
            }
            Err(error) => {
                print_error!("Failed to inspect package file \"{}\": {}", pkg, error);
                return Err(1);
            }
        }
        let file = match File::open(pkg) {
            Ok(f) => f,
            Err(_) => {
                print_error!("Failed to open package file \"{}\"", pkg);
                return Err(1);
            }
        };
        opened_packages.push((pkg, file));
    }

    // MD5 verification of .tar.md5 packages
    if !skip_md5 {
        for (pkg, file) in &mut opened_packages {
            if pkg.to_lowercase().ends_with(".md5") {
                println!("Verifying MD5 checksum for {}...", pkg);
                if let Err(e) = verify_md5_footer(&*file) {
                    print_error!("{}", e);
                    return Err(1);
                }
                if let Err(e) = file.seek(SeekFrom::Start(0)) {
                    print_error!(
                        "Failed to seek package file \"{}\" back to start: {}",
                        pkg,
                        e
                    );
                    return Err(1);
                }
                println!("MD5 verification successful!\n");
            }
        }
    }

    // Scan and index TAR containers
    let mut archives_download_lists: Vec<HashSet<String>> = Vec::new();
    let mut all_packages_entries: Vec<Vec<IndexedEntry>> = Vec::new();

    for (pkg, file) in &opened_packages {
        let source_file = match file.try_clone() {
            Ok(file) => Arc::new(file),
            Err(error) => {
                print_error!("Failed to retain package file \"{}\": {}", pkg, error);
                return Err(1);
            }
        };
        let mut archive = Archive::new(file);
        let entries = match archive.entries() {
            Ok(e) => e,
            Err(e) => {
                print_error!("Failed to read archive entries for \"{}\": {}", pkg, e);
                return Err(1);
            }
        };

        let mut package_entries = Vec::new();

        for entry_res in entries {
            let entry = match entry_res {
                Ok(e) => e,
                Err(e) => {
                    print_error!("Corrupted archive entry in \"{}\": {}", pkg, e);
                    return Err(1);
                }
            };

            let entry_type = entry.header().entry_type();
            let is_file = entry_type.is_file();
            let entry_path = match entry.path() {
                Ok(p) => p.to_string_lossy().to_string(),
                Err(error) => {
                    print_error!("Invalid archive entry path in \"{}\": {}", pkg, error);
                    return Err(1);
                }
            };

            let offset = entry.raw_file_position();
            let size = entry.size();
            let mapping_size = match usize::try_from(size) {
                Ok(size) => size,
                Err(_) => {
                    print_error!(
                        "Archive entry \"{}\" in \"{}\" is too large for this platform.",
                        entry_path,
                        pkg
                    );
                    return Err(1);
                }
            };

            let (normalized_name, is_lz4) = normalize_basename(&entry_path);

            if normalized_name.eq_ignore_ascii_case("download-list.txt") {
                // Read the allowlist manifest
                if !is_file || size == 0 {
                    print_error!("download-list.txt in \"{}\" is not a non-empty file.", pkg);
                    return Err(1);
                }
                if size > MAX_DOWNLOAD_LIST_SIZE {
                    print_error!(
                        "download-list.txt in \"{}\" is unexpectedly large ({} bytes).",
                        pkg,
                        size
                    );
                    return Err(1);
                }
                let mut reader = entry;
                let mut content = String::new();
                if let Err(error) = reader.read_to_string(&mut content) {
                    print_error!("Failed to read download-list.txt in \"{}\": {}", pkg, error);
                    return Err(1);
                }
                let mut download_list = HashSet::new();
                for line in content
                    .lines()
                    .map(str::trim)
                    .filter(|line| !line.is_empty())
                {
                    let (name, _) = normalize_basename(line);
                    if name.is_empty() {
                        print_error!(
                            "download-list.txt in \"{}\" contains an invalid entry: {:?}.",
                            pkg,
                            line
                        );
                        return Err(1);
                    }
                    download_list.insert(name.to_ascii_lowercase());
                }
                if download_list.is_empty() {
                    print_error!(
                        "download-list.txt in \"{}\" contains no payload entries.",
                        pkg
                    );
                    return Err(1);
                }
                archives_download_lists.push(download_list);
            } else {
                match validate_payload_entry(is_file, entry_type.is_dir(), size, &entry_path) {
                    Ok(true) => {}
                    Ok(false) => continue,
                    Err(error) => {
                        print_error!("Invalid archive entry in \"{}\": {}", pkg, error);
                        return Err(1);
                    }
                }
                let mmap = match unsafe {
                    MmapOptions::new()
                        .offset(offset)
                        .len(mapping_size)
                        .map(file)
                } {
                    Ok(m) => m,
                    Err(_) => {
                        print_error!(
                            "Failed to memory map entry \"{}\" in package \"{}\"",
                            entry_path,
                            pkg
                        );
                        return Err(1);
                    }
                };

                package_entries.push(IndexedEntry {
                    original_name: entry_path,
                    normalized_name,
                    mmap,
                    is_lz4,
                    source_file: Arc::clone(&source_file),
                    source_offset: offset,
                    source_size: mapping_size,
                });
            }
        }

        all_packages_entries.push(package_entries);
    }

    // If any package contained a manifest, we use it as our global download allowlist
    let download_allowlist = if let Some((first, rest)) = archives_download_lists.split_first() {
        // Cross-archive manifest consistency check
        if !rest.iter().all(|m| m == first) {
            print_error!("Cross-archive consistency check failed! download-list.txt do not match.");
            return Err(1);
        }
        Some(first)
    } else {
        None
    };

    let mut resolved_entries: Vec<IndexedEntry> = Vec::new();
    let mut pit_entry: Option<IndexedEntry> = None;

    // Apply manifest filtering and positional precedence (last-writer-wins)
    for package_entries in all_packages_entries {
        for entry in package_entries {
            if entry.normalized_name.to_ascii_lowercase().ends_with(".pit") {
                if pit_entry.is_some() {
                    print_error!(
                        "More than one PIT file was found in the selected firmware packages. Select one coherent firmware set."
                    );
                    return Err(1);
                }
                pit_entry = Some(entry);
            } else if let Some(allowlist) = download_allowlist {
                if allowlist.contains(&entry.normalized_name.to_ascii_lowercase()) {
                    resolved_entries.push(entry);
                } else {
                    println!(
                        "Skipping {} (not in download-list.txt)",
                        entry.original_name
                    );
                }
            } else {
                resolved_entries.push(entry);
            }
        }
    }

    // Extract PIT local bytes if any from TAR archives
    let mut local_pit_file = None;

    if let Some(entry) = pit_entry {
        local_pit_file = Some(entry.mmap.to_vec());
    }

    Ok((resolved_entries, local_pit_file))
}

fn find_pit_entries_by_filename<'a>(pit_data: &'a PitData, filename: &str) -> Vec<&'a PitEntry> {
    pit_data
        .entries
        .iter()
        .filter(|entry| {
            entry
                .flash_filename
                .to_string_lossy()
                .eq_ignore_ascii_case(filename)
        })
        .collect()
}

fn find_unique_pit_entry_by_id(pit_data: &PitData, id: u32) -> Result<Option<&PitEntry>, String> {
    let mut matches = pit_data
        .entries
        .iter()
        .filter(|entry| entry.identifier == id);
    let first = matches.next();
    if matches.next().is_some() {
        return Err(format!(
            "Partition identifier {id} is ambiguous in the PIT; select the partition by its unique name instead."
        ));
    }
    Ok(first)
}

fn find_unique_pit_entry_by_name<'a>(
    pit_data: &'a PitData,
    name: &str,
) -> Result<Option<&'a PitEntry>, String> {
    let mut matches = pit_data.entries.iter().filter(|entry| {
        entry
            .partition_name
            .to_string_lossy()
            .trim()
            .eq_ignore_ascii_case(name.trim())
    });
    let first = matches.next();
    if matches.next().is_some() {
        return Err(format!(
            "Partition name \"{name}\" is ambiguous in the PIT; use a unique partition identifier or package mapping."
        ));
    }
    Ok(first)
}

fn pit_target_key(entry: &PitEntry) -> (u32, u32, u32) {
    (
        entry.binary_type as u32,
        entry.device_type as u32,
        entry.identifier,
    )
}

fn pit_layout_group(entry: &PitEntry) -> PitLayoutGroup {
    match entry.device_type {
        // Samsung UFS PITs store the logical-unit index in the otherwise
        // obsolete file_offset field. MMC has one address space.
        DeviceType::UFS => (entry.device_type as u32, entry.file_offset),
        DeviceType::MMC => (entry.device_type as u32, 0),
        _ => (entry.device_type as u32, entry.file_offset),
    }
}

fn pit_layout_name_key(entry: &PitEntry) -> (u32, u32, String) {
    let (device, unit) = pit_layout_group(entry);
    (
        device,
        unit,
        entry
            .partition_name
            .to_string_lossy()
            .trim()
            .to_ascii_uppercase(),
    )
}

fn is_pit_metadata_partition(name: &str) -> bool {
    let name = name.to_ascii_uppercase();
    name == "PIT" || name.starts_with("PGPT") || name.starts_with("SGPT") || name.starts_with("MD5")
}

fn is_growable_zero_partition(name: &str) -> bool {
    name.trim().eq_ignore_ascii_case("USERDATA")
}

fn is_allowed_zero_sized_payload_mapping(partition_name: &str, flash_filename: &str) -> bool {
    partition_name.trim().eq_ignore_ascii_case("UL_KEYS")
        && flash_filename.trim().eq_ignore_ascii_case("ul_key.bin")
}

pub(crate) fn validate_pit_layout(pit_data: &PitData, label: &str) -> Result<(), String> {
    if pit_data.entries.is_empty() {
        return Err(format!("The {label} PIT contains no partition entries."));
    }

    let mut targets = HashSet::new();
    let mut names = HashSet::new();
    let mut extents: HashMap<PitLayoutGroup, Vec<PitExtent>> = HashMap::new();
    let mut flashable_count = 0_usize;
    for entry in &pit_data.entries {
        let partition_name = entry.partition_name.to_string_lossy().trim().to_string();
        if partition_name.is_empty() {
            return Err(format!("The {label} PIT contains an unnamed partition."));
        }
        if !targets.insert(pit_target_key(entry)) {
            return Err(format!(
                "The {label} PIT contains duplicate partition target {partition_name}."
            ));
        }
        if !names.insert(pit_layout_name_key(entry)) {
            return Err(format!(
                "The {label} PIT contains duplicate partition name {partition_name} in one storage unit."
            ));
        }

        if entry.device_type == DeviceType::UFS
            && (pit_data.lu_count == 0 || entry.file_offset >= u32::from(pit_data.lu_count))
        {
            return Err(format!(
                "The {label} PIT assigns partition {partition_name} to invalid UFS logical unit {} (declared units: {}).",
                entry.file_offset, pit_data.lu_count
            ));
        }

        let flash_filename = entry.flash_filename.to_string_lossy();
        let flash_filename = flash_filename.trim();
        if entry.block_count == 0 {
            // Modern Samsung PITs contain legitimate zero-sized reserved/key
            // payload mappings such as UL_KEYS/ul_key.bin. USERDATA may
            // intentionally be grow-to-fill.
            if !flash_filename.is_empty()
                && !is_growable_zero_partition(&partition_name)
                && !is_allowed_zero_sized_payload_mapping(&partition_name, flash_filename)
            {
                return Err(format!(
                    "The {label} PIT maps payload {flash_filename} to zero-sized partition {partition_name}."
                ));
            }
        } else if matches!(entry.device_type, DeviceType::MMC | DeviceType::UFS)
            && !is_pit_metadata_partition(&partition_name)
        {
            let end = entry
                .block_size_or_offset
                .checked_add(entry.block_count)
                .ok_or_else(|| {
                    format!(
                        "The {label} PIT partition {partition_name} overflows its 32-bit block address space."
                    )
                })?;
            extents.entry(pit_layout_group(entry)).or_default().push((
                entry.block_size_or_offset,
                end,
                partition_name.clone(),
            ));
        }

        if !flash_filename.is_empty()
            && (entry.block_count > 0
                || is_growable_zero_partition(&partition_name)
                || is_allowed_zero_sized_payload_mapping(&partition_name, flash_filename))
        {
            flashable_count += 1;
        }
    }

    for ((device, unit), group_extents) in &mut extents {
        group_extents.sort_by_key(|extent| (extent.0, extent.1));
        for adjacent in group_extents.windows(2) {
            let previous = &adjacent[0];
            let next = &adjacent[1];
            if next.0 < previous.1 {
                return Err(format!(
                    "The {label} PIT overlaps partitions {} and {} in storage type {device}, unit {unit}.",
                    previous.2, next.2
                ));
            }
        }
    }

    if flashable_count == 0 {
        return Err(format!("The {label} PIT contains no flashable partitions."));
    }
    Ok(())
}

fn validate_repartition_compatibility(
    current: &PitData,
    candidate: &PitData,
) -> Result<(), String> {
    validate_pit_layout(current, "device")?;
    validate_pit_layout(candidate, "selected")?;

    let current_cpu = current
        .cpu_bl_id
        .to_string_lossy()
        .trim()
        .to_ascii_uppercase();
    let candidate_cpu = candidate
        .cpu_bl_id
        .to_string_lossy()
        .trim()
        .to_ascii_uppercase();
    if current_cpu.is_empty() || candidate_cpu.is_empty() {
        return Err(
            "Repartition was blocked because the device or selected PIT has no hardware identity tag."
                .to_string(),
        );
    }
    if current_cpu != candidate_cpu {
        return Err(format!(
            "The selected PIT targets {candidate_cpu}, but the connected device reports {current_cpu}."
        ));
    }

    let current_container = current
        .com_tar2
        .to_string_lossy()
        .trim()
        .to_ascii_uppercase();
    let candidate_container = candidate
        .com_tar2
        .to_string_lossy()
        .trim()
        .to_ascii_uppercase();
    if !current_container.is_empty()
        && !candidate_container.is_empty()
        && current_container != candidate_container
    {
        return Err(format!(
            "The selected PIT container tag ({candidate_container}) does not match the connected device ({current_container})."
        ));
    }
    if current.lu_count != candidate.lu_count {
        return Err(format!(
            "The selected PIT uses {} logical units, but the connected device uses {}.",
            candidate.lu_count, current.lu_count
        ));
    }

    let storage_kind = |pit: &PitData| {
        if pit
            .entries
            .iter()
            .any(|entry| entry.device_type == DeviceType::UFS)
        {
            Some(DeviceType::UFS)
        } else if pit
            .entries
            .iter()
            .any(|entry| entry.device_type == DeviceType::MMC)
        {
            Some(DeviceType::MMC)
        } else {
            None
        }
    };
    let current_storage = storage_kind(current);
    let candidate_storage = storage_kind(candidate);
    if current_storage.is_none() || candidate_storage.is_none() {
        return Err(
            "Repartition was blocked because the storage type could not be verified from both PIT files."
                .to_string(),
        );
    }
    if current_storage != candidate_storage {
        return Err(
            "The selected PIT storage type does not match the connected device.".to_string(),
        );
    }

    let current_entries: HashMap<(u32, u32, String), &PitEntry> = current
        .entries
        .iter()
        .map(|entry| (pit_layout_name_key(entry), entry))
        .collect();
    let mut common_partitions = 0_usize;
    for entry in &candidate.entries {
        let key = pit_layout_name_key(entry);
        let Some(current_entry) = current_entries.get(&key) else {
            continue;
        };
        common_partitions += 1;
        if pit_target_key(current_entry) != pit_target_key(entry)
            || current_entry.attributes != entry.attributes
        {
            return Err(format!(
                "The selected PIT changes the hardware target or block attributes of partition {}.",
                entry.partition_name
            ));
        }
    }
    let required_common = current.entries.len().saturating_mul(4).div_ceil(5);
    if common_partitions < required_common {
        return Err(format!(
            "The selected PIT shares only {common_partitions} partition names with the connected device; at least {required_common} are required for a safe identity check."
        ));
    }

    let allowed_new_entries = current.entries.len().div_ceil(5).max(4);
    if candidate.entries.len() > current.entries.len().saturating_add(allowed_new_entries) {
        return Err(format!(
            "The selected PIT contains too many new partitions ({} selected versus {} on the device).",
            candidate.entries.len(),
            current.entries.len()
        ));
    }

    let mut current_limits: HashMap<PitLayoutGroup, u32> = HashMap::new();
    for entry in &current.entries {
        if !matches!(entry.device_type, DeviceType::MMC | DeviceType::UFS) {
            continue;
        }
        let end = entry
            .block_size_or_offset
            .checked_add(entry.block_count)
            .ok_or_else(|| {
                "The device PIT contains an overflowing partition extent.".to_string()
            })?;
        current_limits
            .entry(pit_layout_group(entry))
            .and_modify(|limit| *limit = (*limit).max(end))
            .or_insert(end);
    }
    for entry in &candidate.entries {
        if !matches!(entry.device_type, DeviceType::MMC | DeviceType::UFS) {
            continue;
        }
        let group = pit_layout_group(entry);
        let current_limit = current_limits.get(&group).ok_or_else(|| {
            format!(
                "The selected PIT introduces an unknown storage unit for partition {}.",
                entry.partition_name
            )
        })?;
        let candidate_end = entry
            .block_size_or_offset
            .checked_add(entry.block_count)
            .ok_or_else(|| {
                format!(
                    "The selected PIT partition {} overflows its block address space.",
                    entry.partition_name
                )
            })?;
        if candidate_end > *current_limit {
            return Err(format!(
                "The selected PIT partition {} extends beyond the connected device's known storage boundary.",
                entry.partition_name
            ));
        }
    }

    Ok(())
}

fn register_unique_target(
    mapped_targets: &mut HashMap<(u32, u32, u32), String>,
    entry: &PitEntry,
    source: &str,
) -> Result<(), String> {
    if let Some(previous) = mapped_targets.insert(pit_target_key(entry), source.to_string()) {
        return Err(format!(
            "Flash inputs \"{previous}\" and \"{source}\" both target partition {}. Remove the duplicate and retry.",
            entry.partition_name
        ));
    }
    Ok(())
}

fn select_pit_source(
    explicit: Option<Vec<u8>>,
    packaged: Option<Vec<u8>>,
) -> Result<Option<Vec<u8>>, String> {
    match (explicit, packaged) {
        (Some(_), Some(_)) => Err(
            "Both an explicit PIT and a packaged PIT were selected. Remove one to avoid an ambiguous repartition target."
                .to_string(),
        ),
        (Some(pit), None) | (None, Some(pit)) => Ok(Some(pit)),
        (None, None) => Ok(None),
    }
}

fn create_firmware_info<'a>(
    mmap: Mmap,
    source_size: u64,
    is_lz4_suffix: bool,
    pit_entry: &'a PitEntry,
    skip_size_check: bool,
    file_display_name: &str,
) -> Option<FirmwareInfo<'a>> {
    if source_size == 0 {
        print_error!("Flash payload \"{}\" is empty.", file_display_name);
        return None;
    }

    let lz4_frame_header = if is_lz4_payload(&mmap, is_lz4_suffix) {
        let cursor = std::io::Cursor::new(&mmap);
        match Lz4FrameHeader::parse(cursor) {
            Ok(fh) if fh.content_size > 0 => Some(fh),
            Ok(_) => {
                print_error!(
                    "LZ4 flash payload \"{}\" reports an empty decompressed size.",
                    file_display_name
                );
                return None;
            }
            Err(e) => {
                print_error!(
                    "Failed to parse LZ4 header for {}: {}",
                    file_display_name,
                    e
                );
                return None;
            }
        }
    } else {
        None
    };

    if !skip_size_check {
        let partition_size = pit_entry.partition_size();
        let check_size = if let Some(fh) = &lz4_frame_header {
            fh.content_size
        } else {
            source_size
        };

        if partition_size > 0 && check_size > partition_size {
            print_error!(
                "{} partition is too small for given file. Use --skip-size-check to flash anyways.",
                pit_entry.partition_name
            );
            return None;
        }
    }

    if let Some(header) = lz4_frame_header {
        Some(FirmwareInfo::Lz4(FirmwareLz4File {
            pit_entry,
            file: mmap,
            header,
        }))
    } else {
        Some(FirmwareInfo::Normal(FirmwareFile {
            pit_entry,
            file: mmap,
        }))
    }
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn action_flash(
    usb_backend: UsbBackendOption,
    repartition: bool,
    verbose: bool,
    reboot_device: bool,
    wait: bool,
    skip_size_check: bool,
    skip_md5: bool,
    pit: Option<&str>,
    csc_mode: &str,
    package_selection: &FolderPackages,
    partitions: &[PartitionArg],
) -> i32 {
    // Validate package identity and destructive-mode requirements before
    // opening a device session.
    if let Err(error) = validate_package_selection(package_selection, csc_mode, repartition) {
        print_error!("{}", error);
        return 1;
    }
    if !repartition && pit.is_some() {
        print_error!(
            "A PIT file may only be selected when the explicit --repartition option is enabled."
        );
        return 1;
    }

    let mut packages = Vec::new();
    package_selection.append_to(&mut packages);

    // Resolve and validate the candidate PIT before connecting. An explicit
    // and packaged PIT together are ambiguous and must never use precedence.
    let mut pit_file_bytes = None;
    if let Some(pit_path) = pit {
        let mut f = match File::open(pit_path) {
            Ok(file) => file,
            Err(_) => {
                print_error!("Failed to open explicit PIT file \"{}\"", pit_path);
                return 1;
            }
        };
        let mut buffer = Vec::new();
        if f.read_to_end(&mut buffer).is_err() {
            print_error!("Failed to read PIT file.");
            return 1;
        }
        if buffer.is_empty() {
            print_error!("The explicit PIT file \"{}\" is empty.", pit_path);
            return 1;
        }
        pit_file_bytes = Some(buffer);
    }

    let mut resolved_entries = Vec::new();
    let mut packaged_pit = None;
    if !packages.is_empty() {
        let (entries, local_pit) = match scan_tar_packages(&packages, skip_md5) {
            Ok(res) => res,
            Err(code) => return code,
        };
        resolved_entries = entries;
        packaged_pit = local_pit;
    }
    if !packages.is_empty() && resolved_entries.is_empty() {
        print_error!(
            "The selected firmware packages do not contain any flashable partition payloads."
        );
        return 1;
    }
    pit_file_bytes = match select_pit_source(pit_file_bytes, packaged_pit) {
        Ok(pit) => pit,
        Err(error) => {
            print_error!("{}", error);
            return 1;
        }
    };

    if repartition && pit_file_bytes.is_none() {
        print_error!("Repartition requires a PIT file in the packages or via --pit.");
        return 1;
    }
    let selected_pit = if repartition {
        let Some(pit_bytes) = pit_file_bytes.as_deref() else {
            print_error!("Repartition requires a valid PIT file.");
            return 1;
        };
        let pit_data = match PitData::new(pit_bytes) {
            Ok(data) => data,
            Err(_) => {
                print_error!("The selected firmware contains an invalid PIT file.");
                return 1;
            }
        };
        if let Err(error) = validate_pit_layout(&pit_data, "selected") {
            print_error!("{}", error);
            return 1;
        }
        Some(pit_data)
    } else {
        None
    };

    // Connect and download the current device PIT before sending any data.
    let usb = match create_backend(usb_backend, verbose, wait) {
        Ok(u) => u,
        Err(e) => {
            print_error!("{}", e);
            return 1;
        }
    };
    let mut odin_manager = OdinManager::new(usb, verbose);

    if let Err(e) = odin_manager.init() {
        print_error!("{}", e);
        return 1;
    }

    if let Err(e) = odin_manager.begin_session() {
        print_error!("{}", e);
        return 1;
    }

    let pit_buffer = match odin_manager.download_pit_file() {
        Ok(buf) => buf,
        Err(e) => {
            print_error!("{}", e);
            return 1;
        }
    };

    let pit_data = match PitData::new(&pit_buffer) {
        Ok(data) => data,
        Err(_) => {
            print_error!("Failed to unpack device's PIT file!");
            return 1;
        }
    };
    if let Err(error) = validate_pit_layout(&pit_data, "device") {
        print_error!("{}", error);
        return 1;
    }
    if let Some(candidate) = selected_pit.as_ref()
        && let Err(error) = validate_repartition_compatibility(&pit_data, candidate)
    {
        print_error!("{}", error);
        return 1;
    }

    // Repartition mappings must use the selected, validated layout. Normal
    // flashes map against the PIT read from the device.
    let mapping_pit = selected_pit.as_ref().unwrap_or(&pit_data);
    let mapping_label = if repartition { "selected" } else { "device" };

    let mut partition_infos = Vec::new();
    let mut mapped_targets = HashMap::new();
    let mut unmatched_entries = Vec::new();

    // Build the complete flash plan. Any unmatched or duplicate target blocks
    // the operation before a replacement PIT or partition payload is sent.
    for entry in resolved_entries {
        let pit_entries = find_pit_entries_by_filename(mapping_pit, &entry.normalized_name);
        if pit_entries.is_empty() {
            unmatched_entries.push(entry.original_name);
            continue;
        }
        let IndexedEntry {
            original_name,
            mmap,
            is_lz4,
            source_file,
            source_offset,
            source_size,
            ..
        } = entry;
        let mut first_mapping = Some(mmap);
        for pit_entry in pit_entries {
            if let Err(error) =
                register_unique_target(&mut mapped_targets, pit_entry, &original_name)
            {
                print_error!("{}", error);
                return 1;
            }
            let payload = if let Some(payload) = first_mapping.take() {
                payload
            } else {
                match unsafe {
                    MmapOptions::new()
                        .offset(source_offset)
                        .len(source_size)
                        .map(source_file.as_ref())
                } {
                    Ok(payload) => payload,
                    Err(error) => {
                        print_error!(
                            "Failed to create another mapping for package entry \"{}\": {}",
                            original_name,
                            error
                        );
                        return 1;
                    }
                }
            };
            let size = payload.len() as u64;
            let Some(info) = create_firmware_info(
                payload,
                size,
                is_lz4,
                pit_entry,
                skip_size_check,
                &original_name,
            ) else {
                return 1;
            };
            partition_infos.push(info);
        }
    }
    if !unmatched_entries.is_empty() {
        unmatched_entries.sort();
        print_error!(
            "The following package entries do not exist in the {} PIT: {}. Flashing was stopped before any partition data was written.",
            mapping_label,
            unmatched_entries.join(", ")
        );
        return 1;
    }

    for part in partitions {
        let (filename, is_lz4_suffix) = normalize_basename(&part.filename);
        let entries = match &part.name {
            None => {
                let entries = find_pit_entries_by_filename(mapping_pit, &filename);
                if entries.is_empty() {
                    print_error!(
                        "File \"{}\" does not match any partition in the {} PIT.",
                        part.filename,
                        mapping_label
                    );
                    return 1;
                }
                entries
            }
            Some(name) => {
                if let Ok(id) = name.parse::<u32>() {
                    match find_unique_pit_entry_by_id(mapping_pit, id) {
                        Ok(Some(entry)) => vec![entry],
                        Ok(None) => {
                            print_error!(
                                "Partition identifier {id} does not exist in the {mapping_label} PIT."
                            );
                            return 1;
                        }
                        Err(error) => {
                            print_error!("{}", error);
                            return 1;
                        }
                    }
                } else {
                    match find_unique_pit_entry_by_name(mapping_pit, name) {
                        Ok(Some(entry)) => vec![entry],
                        Ok(None) => {
                            print_error!(
                                "Partition \"{}\" does not exist in the {} PIT.",
                                name,
                                mapping_label
                            );
                            return 1;
                        }
                        Err(error) => {
                            print_error!("{}", error);
                            return 1;
                        }
                    }
                }
            }
        };
        let source = match File::open(&part.filename) {
            Ok(file) => file,
            Err(error) => {
                print_error!("Failed to open file \"{}\": {}", part.filename, error);
                return 1;
            }
        };
        let file_size = match source.metadata() {
            Ok(metadata) if metadata.is_file() && metadata.len() > 0 => metadata.len(),
            _ => {
                print_error!(
                    "Flash payload \"{}\" is not a non-empty file.",
                    part.filename
                );
                return 1;
            }
        };
        let mapping_size = match usize::try_from(file_size) {
            Ok(size) => size,
            Err(_) => {
                print_error!(
                    "Flash payload \"{}\" is too large for this platform.",
                    part.filename
                );
                return 1;
            }
        };
        for entry in entries {
            if let Err(error) = register_unique_target(&mut mapped_targets, entry, &part.filename) {
                print_error!("{}", error);
                return 1;
            }
            let mmap = match unsafe { MmapOptions::new().len(mapping_size).map(&source) } {
                Ok(mmap) => mmap,
                Err(error) => {
                    print_error!("Failed to memory map file \"{}\": {}", part.filename, error);
                    return 1;
                }
            };
            let Some(info) = create_firmware_info(
                mmap,
                file_size,
                is_lz4_suffix,
                entry,
                skip_size_check,
                &part.filename,
            ) else {
                return 1;
            };
            partition_infos.push(info);
        }
    }

    if partition_infos.is_empty() {
        print_error!("No flash payloads matched partitions in the {mapping_label} PIT.");
        return 1;
    }

    if let Err(error) = validate_firmware_plan(&partition_infos) {
        print_error!("{}", error);
        return 1;
    }

    let total_bytes: u64 = partition_infos
        .iter()
        .map(|part| match part {
            FirmwareInfo::Normal(f) => f.file.len() as u64,
            FirmwareInfo::Lz4(f) => f.header.content_size,
        })
        .sum();
    if total_bytes == 0 {
        print_error!("The validated flash plan contains no payload data.");
        return 1;
    }

    // Every candidate/device PIT, package mapping, duplicate-target, and size
    // check has passed. Only now may the replacement PIT be uploaded.
    if repartition {
        println!("Uploading validated PIT");
        let Some(pit_bytes) = pit_file_bytes.as_ref() else {
            print_error!("Repartition requires a valid PIT file.");
            return 1;
        };
        if let Err(e) = odin_manager.send_pit_data(pit_bytes) {
            print_error!("{}", e);
            return 1;
        }
        println!("PIT upload successful\n");
    }

    if let Err(e) = odin_manager.set_total_bytes(total_bytes) {
        print_error!("{}", e);
        return 1;
    }

    for info in partition_infos {
        match info {
            FirmwareInfo::Normal(f) => {
                println!("Uploading {}", f.pit_entry.partition_name);
                if let Err(e) = odin_manager.send_file(&f) {
                    print_error!("{}", e);
                    return 1;
                }
                println!("{} upload successful\n", f.pit_entry.partition_name);
            }
            FirmwareInfo::Lz4(f) => {
                println!("Uploading {}", f.pit_entry.partition_name);
                if let Err(e) = odin_manager.send_lz4_file(&f) {
                    print_error!("{}", e);
                    return 1;
                }
                println!("{} upload successful\n", f.pit_entry.partition_name);
            }
        }
    }

    if let Err(e) = odin_manager.end_session() {
        print_error!("{}", e);
        return 1;
    }

    if reboot_device && let Err(e) = odin_manager.reboot_device() {
        print_error!("{}", e);
        return 1;
    }

    0
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::time::{SystemTime, UNIX_EPOCH};
    use tar::{Builder, Header};

    fn temp_test_dir(name: &str) -> std::path::PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let directory = std::env::temp_dir().join(format!(
            "samloader-flash-{name}-{}-{nonce}",
            std::process::id()
        ));
        fs::create_dir(&directory).unwrap();
        directory
    }

    fn write_tar(path: &Path, entries: &[(&str, &[u8])]) {
        let file = File::create(path).unwrap();
        let mut archive = Builder::new(file);
        for (name, bytes) in entries {
            let mut header = Header::new_gnu();
            header.set_size(bytes.len() as u64);
            header.set_mode(0o644);
            header.set_cksum();
            archive.append_data(&mut header, name, *bytes).unwrap();
        }
        archive.finish().unwrap();
    }

    #[test]
    fn repartition_pit_layout_and_hardware_identity_fail_closed() {
        let bytes = include_bytes!("../../test-data/Q7MQ_EUR_OPENX.pit");
        let current = PitData::new(bytes).unwrap();
        let candidate = PitData::new(bytes).unwrap();
        validate_pit_layout(&current, "device").unwrap();
        validate_repartition_compatibility(&current, &candidate).unwrap();

        assert!(is_allowed_zero_sized_payload_mapping(
            "UL_KEYS",
            "ul_key.bin"
        ));
        assert!(!is_allowed_zero_sized_payload_mapping("BOOT_A", "boot.img"));

        let mut incompatible = PitData::new(bytes).unwrap();
        incompatible.lu_count = incompatible.lu_count.saturating_add(1);
        assert!(validate_repartition_compatibility(&current, &incompatible).is_err());

        let mut duplicate = PitData::new(bytes).unwrap();
        let first_binary_type = duplicate.entries[0].binary_type;
        let first_device_type = duplicate.entries[0].device_type;
        let first_identifier = duplicate.entries[0].identifier;
        duplicate.entries[1].binary_type = first_binary_type;
        duplicate.entries[1].device_type = first_device_type;
        duplicate.entries[1].identifier = first_identifier;
        assert!(validate_pit_layout(&duplicate, "selected").is_err());

        let mut ambiguous_id = PitData::new(bytes).unwrap();
        let first_identifier = ambiguous_id.entries[0].identifier;
        ambiguous_id.entries[1].identifier = first_identifier;
        assert!(find_unique_pit_entry_by_id(&ambiguous_id, first_identifier).is_err());
    }

    #[test]
    fn duplicate_flash_targets_are_rejected() {
        let pit = PitData::new(include_bytes!("../../test-data/Q7MQ_EUR_OPENX.pit")).unwrap();
        let entry = find_pit_entries_by_filename(&pit, "persist.img")[0];
        let mut targets = HashMap::new();
        register_unique_target(&mut targets, entry, "first.img").unwrap();
        let error = register_unique_target(&mut targets, entry, "second.img").unwrap_err();
        assert!(error.contains("both target partition"));
    }

    #[test]
    fn one_payload_filename_can_target_both_slot_partitions() {
        let pit = PitData::new(include_bytes!("../../test-data/Q7MQ_EUR_OPENX.pit")).unwrap();
        let entries = find_pit_entries_by_filename(&pit, "dspso.bin");
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].partition_name.to_string_lossy(), "DSP_A");
        assert_eq!(entries[1].partition_name.to_string_lossy(), "DSP_B");
    }

    #[test]
    fn explicit_and_packaged_pit_sources_are_ambiguous() {
        assert!(select_pit_source(Some(vec![1]), Some(vec![2])).is_err());
        assert_eq!(
            select_pit_source(Some(vec![1]), None).unwrap(),
            Some(vec![1])
        );
    }

    #[test]
    fn multiple_packaged_pit_files_are_rejected() {
        let directory = temp_test_dir("multiple-pit");
        let package = directory.join("AP_test.tar");
        let pit = include_bytes!("../../test-data/Q7MQ_EUR_OPENX.pit");
        write_tar(&package, &[("first.pit", pit), ("SECOND.PIT", pit)]);

        let result = scan_tar_packages(&[package.to_string_lossy().to_string()], true);
        assert!(result.is_err());
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn metadata_download_list_is_applied_before_metadata_filtering() {
        let directory = temp_test_dir("metadata-download-list");
        let package = directory.join("AP_test.tar");
        write_tar(
            &package,
            &[
                ("meta-data/download-list.txt", b"persist.img\n"),
                ("persist.img", b"allowed"),
                ("userdata.img", b"must-not-flash"),
            ],
        );

        let (entries, pit) =
            scan_tar_packages(&[package.to_string_lossy().to_string()], true).unwrap();

        assert!(pit.is_none());
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].normalized_name, "persist.img");
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn unreadable_download_list_fails_closed() {
        let directory = temp_test_dir("invalid-download-list");
        let package = directory.join("AP_test.tar");
        write_tar(
            &package,
            &[
                ("meta-data/download-list.txt", &[0xff, 0xfe]),
                ("persist.img", b"must-not-flash"),
            ],
        );

        assert!(scan_tar_packages(&[package.to_string_lossy().to_string()], true).is_err());
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn empty_download_list_fails_closed() {
        let directory = temp_test_dir("empty-download-list");
        let package = directory.join("AP_test.tar");
        write_tar(
            &package,
            &[
                ("meta-data/download-list.txt", b""),
                ("persist.img", b"must-not-flash"),
            ],
        );

        assert!(scan_tar_packages(&[package.to_string_lossy().to_string()], true).is_err());
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn empty_and_non_regular_payload_entries_fail_closed() {
        assert!(validate_payload_entry(true, false, 0, "boot.img").is_err());
        assert!(validate_payload_entry(false, false, 1, "boot.img").is_err());
        assert!(!validate_payload_entry(false, true, 0, "images").unwrap());
        assert!(!validate_payload_entry(true, false, 0, "meta-data/empty.bin").unwrap());
    }

    #[test]
    fn lz4_magic_is_detected_even_without_a_filename_suffix() {
        assert!(is_lz4_payload(&0x184d_2204_u32.to_le_bytes(), false));
        assert!(is_lz4_payload(b"raw", true));
        assert!(!is_lz4_payload(b"raw", false));
    }

    fn package_selection_with_ap(path: &Path) -> FolderPackages {
        FolderPackages {
            ap: Some(path.to_string_lossy().to_string()),
            ..FolderPackages::default()
        }
    }

    #[test]
    fn unmatched_package_entry_blocks_the_flash_plan() {
        let directory = temp_test_dir("unmatched-entry");
        let package = directory.join("AP_S931BXXU1AYF1_release.tar");
        write_tar(&package, &[("not-a-device-partition.img", b"payload")]);
        let packages = package_selection_with_ap(&package);

        let result = action_flash(
            UsbBackendOption::Mock,
            false,
            false,
            false,
            false,
            false,
            true,
            None,
            "home",
            &packages,
            &[],
        );
        assert_eq!(result, 1);
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn duplicate_package_mapping_blocks_the_flash_plan() {
        let directory = temp_test_dir("duplicate-mapping");
        let package = directory.join("AP_S931BXXU1AYF1_release.tar");
        write_tar(
            &package,
            &[("persist.img", b"first"), ("nested/persist.img", b"second")],
        );
        let packages = package_selection_with_ap(&package);

        let result = action_flash(
            UsbBackendOption::Mock,
            false,
            false,
            false,
            false,
            false,
            true,
            None,
            "home",
            &packages,
            &[],
        );
        assert_eq!(result, 1);
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn empty_repartition_packages_fail_before_device_connection() {
        let directory = temp_test_dir("empty-repartition-packages");
        let packages = FolderPackages {
            bl: Some(
                directory
                    .join("BL_S931BXXU1AYF1_release.tar")
                    .to_string_lossy()
                    .to_string(),
            ),
            ap: Some(
                directory
                    .join("AP_S931BXXU1AYF1_release.tar")
                    .to_string_lossy()
                    .to_string(),
            ),
            cp: Some(
                directory
                    .join("CP_S931BXXU1AYE9_release.tar")
                    .to_string_lossy()
                    .to_string(),
            ),
            csc: Some(
                directory
                    .join("CSC_OXM_S931BOXM1AYF1_release.tar")
                    .to_string_lossy()
                    .to_string(),
            ),
            userdata: None,
        };
        let mut paths = Vec::new();
        packages.append_to(&mut paths);
        for path in paths {
            write_tar(Path::new(&path), &[]);
        }
        let pit = Path::new(env!("CARGO_MANIFEST_DIR")).join("../test-data/Q7MQ_EUR_OPENX.pit");

        let result = action_flash(
            UsbBackendOption::Mock,
            true,
            false,
            false,
            false,
            false,
            true,
            pit.to_str(),
            "wipe",
            &packages,
            &[],
        );
        assert_eq!(result, 1);
        fs::remove_dir_all(directory).unwrap();
    }
}
