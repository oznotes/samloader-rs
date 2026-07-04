#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use memmap2::{Mmap, MmapOptions};
use samloader_fus::{DownloadProgress, FusClient, fetch_version_xml};
use samloader_odin::{
    FirmwareFile, FirmwareInfo, FirmwareLz4File, Lz4FrameHeader, OdinManager, UsbBackendOption,
    create_backend, detect_device, reboot_download, verify_md5_footer,
};
use samloader_pit::{PitData, PitEntry};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::fs::{self, File};
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;
use std::time::Instant;
use tar::Archive;
use tauri::{AppHandle, Emitter};

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct FolderPackages {
    bl: Option<String>,
    ap: Option<String>,
    cp: Option<String>,
    csc: Option<String>,
    userdata: Option<String>,
}

impl FolderPackages {
    fn append_to(&self, packages: &mut Vec<String>) {
        if let Some(bl) = &self.bl {
            packages.push(bl.clone());
        }
        if let Some(ap) = &self.ap {
            packages.push(ap.clone());
        }
        if let Some(cp) = &self.cp {
            packages.push(cp.clone());
        }
        if let Some(csc) = &self.csc {
            packages.push(csc.clone());
        }
        if let Some(userdata) = &self.userdata {
            packages.push(userdata.clone());
        }
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct AppInfo {
    version: &'static str,
    default_backend: &'static str,
    backends: Vec<&'static str>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct UpdateInfo {
    latest: String,
    previous: Vec<String>,
    beta: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct DeviceStatus {
    connected: bool,
    label: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct PitEntryView {
    id: String,
    partition: String,
    flash_file: String,
    size: u64,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct VerifyResult {
    file: String,
    ok: bool,
    message: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct DownloadRequest {
    model: String,
    region: String,
    version: Option<String>,
    out_dir: Option<String>,
    out_file: Option<String>,
    threads: u64,
    verbose: bool,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct FlashRequest {
    backend: String,
    verbose: bool,
    wait: bool,
    no_reboot: bool,
    repartition: bool,
    skip_size_check: bool,
    skip_md5: bool,
    pit: Option<String>,
    folder: Option<String>,
    csc_mode: String,
    packages: FolderPackages,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct DownloadEvent {
    status: String,
    filename: Option<String>,
    path: Option<String>,
    bytes: u64,
    total: u64,
    speed_bps: f64,
    message: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct FlashEvent {
    status: String,
    partition: Option<String>,
    percent: f64,
    partition_bytes: Option<u64>,
    partition_total: Option<u64>,
    overall_bytes: Option<u64>,
    overall_total: Option<u64>,
    message: Option<String>,
}

struct IndexedEntry {
    original_name: String,
    normalized_name: String,
    mmap: Mmap,
    is_lz4: bool,
}

struct TauriDownloadProgress {
    app: AppHandle,
    position: AtomicU64,
    total: u64,
    filename: String,
    output_path: String,
    started: Instant,
    verbose: bool,
}

impl DownloadProgress for TauriDownloadProgress {
    fn inc(&self, bytes: u64) {
        let position = self.position.fetch_add(bytes, Ordering::Relaxed) + bytes;
        let elapsed = self.started.elapsed().as_secs_f64().max(0.001);
        let _ = self.app.emit(
            "download-progress",
            DownloadEvent {
                status: "downloading".to_string(),
                filename: Some(self.filename.clone()),
                path: Some(self.output_path.clone()),
                bytes: position,
                total: self.total,
                speed_bps: position as f64 / elapsed,
                message: None,
            },
        );
    }

    fn position(&self) -> u64 {
        self.position.load(Ordering::Relaxed)
    }

    fn println(&self, msg: String) {
        let _ = self.app.emit(
            "download-progress",
            DownloadEvent {
                status: "message".to_string(),
                filename: Some(self.filename.clone()),
                path: Some(self.output_path.clone()),
                bytes: self.position(),
                total: self.total,
                speed_bps: 0.0,
                message: Some(msg),
            },
        );
    }

    fn println_verbose(&self, msg: String) {
        if self.verbose {
            self.println(msg);
        }
    }
}

fn is_tar_package_name(name: &str) -> bool {
    let name = name.to_ascii_lowercase();
    name.ends_with(".tar") || name.ends_with(".tar.md5")
}

fn path_to_cli_string(path: PathBuf) -> String {
    path.to_string_lossy().to_string()
}

fn select_package(label: &str, mut candidates: Vec<PathBuf>) -> Result<Option<String>, String> {
    candidates.sort();
    let mut md5_candidates = Vec::new();
    let mut tar_candidates = Vec::new();

    for candidate in candidates {
        let name = candidate
            .file_name()
            .map(|n| n.to_string_lossy().to_ascii_lowercase())
            .unwrap_or_default();
        if name.ends_with(".tar.md5") {
            md5_candidates.push(candidate);
        } else {
            tar_candidates.push(candidate);
        }
    }

    let selected = if !md5_candidates.is_empty() {
        if md5_candidates.len() > 1 {
            return Err(format!(
                "Multiple {label} .tar.md5 packages found. Specify the package explicitly."
            ));
        }
        md5_candidates.pop()
    } else {
        if tar_candidates.len() > 1 {
            return Err(format!(
                "Multiple {label} .tar packages found. Specify the package explicitly."
            ));
        }
        tar_candidates.pop()
    };

    Ok(selected.map(path_to_cli_string))
}

fn scan_package_folder_impl(folder: &str, csc_mode: &str) -> Result<FolderPackages, String> {
    let folder_path = Path::new(folder);
    if !folder_path.is_dir() {
        return Err(format!(
            "Firmware folder \"{folder}\" does not exist or is not a directory."
        ));
    }

    let mut bl = Vec::new();
    let mut ap = Vec::new();
    let mut cp = Vec::new();
    let mut home_csc = Vec::new();
    let mut csc = Vec::new();
    let mut userdata = Vec::new();

    let entries = fs::read_dir(folder_path)
        .map_err(|e| format!("Failed to read firmware folder \"{folder}\": {e}"))?;

    for entry in entries {
        let entry = entry.map_err(|e| format!("Failed to read firmware folder entry: {e}"))?;
        let file_type = entry
            .file_type()
            .map_err(|e| format!("Failed to read firmware folder entry type: {e}"))?;
        if !file_type.is_file() {
            continue;
        }

        let name = entry.file_name().to_string_lossy().to_string();
        if !is_tar_package_name(&name) {
            continue;
        }

        let upper_name = name.to_ascii_uppercase();
        let path = entry.path();
        if upper_name.starts_with("BL_") {
            bl.push(path);
        } else if upper_name.starts_with("AP_") {
            ap.push(path);
        } else if upper_name.starts_with("CP_") {
            cp.push(path);
        } else if upper_name.starts_with("HOME_CSC_") {
            home_csc.push(path);
        } else if upper_name.starts_with("CSC_") {
            csc.push(path);
        } else if upper_name.starts_with("USERDATA_") {
            userdata.push(path);
        }
    }

    let home_csc = select_package("HOME_CSC", home_csc)?;
    let csc = select_package("CSC", csc)?;
    let selected_csc = match csc_mode {
        "home" => {
            if home_csc.is_none() && csc.is_some() {
                return Err(
                    "HOME_CSC package was not found. Use wipe mode to select CSC instead."
                        .to_string(),
                );
            }
            home_csc
        }
        "wipe" => {
            if csc.is_none() && home_csc.is_some() {
                return Err("CSC package was not found. Use HOME_CSC mode instead.".to_string());
            }
            csc
        }
        _ => return Err(format!("Unknown CSC mode: {csc_mode}")),
    };

    Ok(FolderPackages {
        bl: select_package("BL", bl)?,
        ap: select_package("AP", ap)?,
        cp: select_package("CP", cp)?,
        csc: selected_csc,
        userdata: select_package("USERDATA", userdata)?,
    })
}

fn backend_from_str(backend: &str) -> Result<UsbBackendOption, String> {
    UsbBackendOption::try_from(backend).map_err(|e| e.to_string())
}

fn emit_flash(
    app: &AppHandle,
    status: &str,
    partition: Option<&str>,
    percent: f64,
    message: impl Into<Option<String>>,
) {
    emit_flash_progress(
        app, status, partition, percent, None, None, None, None, message,
    );
}

#[allow(clippy::too_many_arguments)]
fn emit_flash_progress(
    app: &AppHandle,
    status: &str,
    partition: Option<&str>,
    percent: f64,
    partition_bytes: Option<u64>,
    partition_total: Option<u64>,
    overall_bytes: Option<u64>,
    overall_total: Option<u64>,
    message: impl Into<Option<String>>,
) {
    let _ = app.emit(
        "flash-progress",
        FlashEvent {
            status: status.to_string(),
            partition: partition.map(|p| p.to_string()),
            percent,
            partition_bytes,
            partition_total,
            overall_bytes,
            overall_total,
            message: message.into(),
        },
    );
}

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

fn scan_tar_packages(
    packages: &[String],
    skip_md5: bool,
    app: &AppHandle,
) -> Result<(Vec<IndexedEntry>, Option<Vec<u8>>), String> {
    let mut opened_packages = Vec::new();
    for pkg in packages {
        let file =
            File::open(pkg).map_err(|e| format!("Failed to open package file \"{pkg}\": {e}"))?;
        opened_packages.push((pkg, file));
    }

    if !skip_md5 {
        for (pkg, file) in &mut opened_packages {
            if pkg.to_lowercase().ends_with(".md5") {
                emit_flash(
                    app,
                    "verifying-md5",
                    None,
                    0.0,
                    Some(format!("Verifying {pkg}")),
                );
                verify_md5_footer(&*file)
                    .map_err(|e| format!("MD5 verification failed for \"{pkg}\": {e}"))?;
                file.seek(SeekFrom::Start(0)).map_err(|e| {
                    format!("Failed to seek package file \"{pkg}\" back to start: {e}")
                })?;
            }
        }
    }

    let mut archives_download_lists: Vec<HashSet<String>> = Vec::new();
    let mut all_packages_entries: Vec<Vec<IndexedEntry>> = Vec::new();

    for (pkg, file) in &opened_packages {
        let mut archive = Archive::new(file);
        let entries = archive
            .entries()
            .map_err(|e| format!("Failed to read archive entries for \"{pkg}\": {e}"))?;

        let mut package_entries = Vec::new();

        for entry_res in entries {
            let entry =
                entry_res.map_err(|e| format!("Corrupted archive entry in \"{pkg}\": {e}"))?;
            let entry_path = match entry.path() {
                Ok(p) => p.to_string_lossy().to_string(),
                Err(_) => continue,
            };

            let offset = entry.raw_file_position();
            let size = entry.size();
            let (normalized_name, is_lz4) = normalize_basename(&entry_path);

            if normalized_name == "download-list.txt" {
                let mut reader = entry;
                let mut content = String::new();
                if reader.read_to_string(&mut content).is_ok() {
                    let download_list = content
                        .lines()
                        .filter_map(|s| {
                            let s = s.trim();
                            if s.is_empty() {
                                None
                            } else {
                                Some(s.to_string())
                            }
                        })
                        .collect();
                    archives_download_lists.push(download_list);
                }
            } else {
                let mmap = unsafe {
                    MmapOptions::new()
                        .offset(offset)
                        .len(size as usize)
                        .map(file)
                }
                .map_err(|e| {
                    format!("Failed to memory map entry \"{entry_path}\" in package \"{pkg}\": {e}")
                })?;

                package_entries.push(IndexedEntry {
                    original_name: entry_path,
                    normalized_name,
                    mmap,
                    is_lz4,
                });
            }
        }

        all_packages_entries.push(package_entries);
    }

    let download_allowlist = if let Some((first, rest)) = archives_download_lists.split_first() {
        if !rest.iter().all(|m| m == first) {
            return Err(
                "Cross-archive consistency check failed: download-list.txt entries do not match."
                    .to_string(),
            );
        }
        Some(first)
    } else {
        None
    };

    let mut resolved_entries = Vec::new();
    let mut pit_entry = None;

    for package_entries in all_packages_entries {
        for entry in package_entries {
            if entry.normalized_name.ends_with(".pit") {
                pit_entry = Some(entry);
            } else if let Some(allowlist) = download_allowlist {
                if allowlist.contains(&entry.normalized_name) {
                    resolved_entries.push(entry);
                }
            } else {
                resolved_entries.push(entry);
            }
        }
    }

    let local_pit_file = pit_entry.map(|entry| entry.mmap.to_vec());
    Ok((resolved_entries, local_pit_file))
}

fn find_pit_entry_by_filename<'a>(pit_data: &'a PitData, filename: &str) -> Option<&'a PitEntry> {
    pit_data.entries.iter().find(|e| {
        let flash_fn = e.flash_filename.to_string_lossy();
        flash_fn.eq_ignore_ascii_case(filename)
    })
}

fn create_firmware_info<'a>(
    mmap: Mmap,
    source_size: u64,
    is_lz4_suffix: bool,
    pit_entry: &'a PitEntry,
    skip_size_check: bool,
    file_display_name: &str,
) -> Result<FirmwareInfo<'a>, String> {
    let lz4_frame_header = if is_lz4_suffix {
        let cursor = std::io::Cursor::new(&mmap);
        Some(
            Lz4FrameHeader::parse(cursor)
                .map_err(|e| format!("Failed to parse LZ4 header for {file_display_name}: {e}"))?,
        )
    } else {
        None
    };

    if !skip_size_check {
        let partition_size = pit_entry.partition_size();
        let check_size = lz4_frame_header
            .as_ref()
            .map(|header| header.content_size)
            .unwrap_or(source_size);

        if partition_size > 0 && check_size > partition_size {
            return Err(format!(
                "{} partition is too small for given file. Enable skip size check to flash anyway.",
                pit_entry.partition_name
            ));
        }
    }

    Ok(if let Some(header) = lz4_frame_header {
        FirmwareInfo::Lz4(FirmwareLz4File {
            pit_entry,
            file: mmap,
            header,
        })
    } else {
        FirmwareInfo::Normal(FirmwareFile {
            pit_entry,
            file: mmap,
        })
    })
}

fn run_flash(app: AppHandle, req: FlashRequest) -> Result<(), String> {
    let usb_backend = backend_from_str(&req.backend)?;
    let mut packages = Vec::new();

    if let Some(folder) = &req.folder {
        let folder_packages = scan_package_folder_impl(folder, &req.csc_mode)?;
        folder_packages.append_to(&mut packages);
    } else {
        req.packages.append_to(&mut packages);
    }

    if packages.is_empty() {
        return Err("No firmware packages were selected.".to_string());
    }

    let mut pit_file_bytes = None;
    if let Some(pit_path) = &req.pit {
        let mut f = File::open(pit_path)
            .map_err(|e| format!("Failed to open explicit PIT file \"{pit_path}\": {e}"))?;
        let mut buffer = Vec::new();
        f.read_to_end(&mut buffer)
            .map_err(|e| format!("Failed to read PIT file \"{pit_path}\": {e}"))?;
        pit_file_bytes = Some(buffer);
    }

    let (resolved_entries, local_pit) = scan_tar_packages(&packages, req.skip_md5, &app)?;
    if pit_file_bytes.is_none() {
        pit_file_bytes = local_pit;
    }

    if req.repartition && pit_file_bytes.is_none() {
        return Err(
            "Repartition requires a PIT file in the packages or an explicit PIT path.".to_string(),
        );
    }

    emit_flash(
        &app,
        "connecting",
        None,
        0.0,
        Some("Connecting to device".to_string()),
    );
    let usb = create_backend(usb_backend, req.verbose, req.wait).map_err(|e| e.to_string())?;
    let mut odin_manager = OdinManager::new(usb, req.verbose);

    odin_manager.init().map_err(|e| e.to_string())?;
    odin_manager.begin_session().map_err(|e| e.to_string())?;

    if req.repartition {
        emit_flash(&app, "pit", None, 0.0, Some("Uploading PIT".to_string()));
        odin_manager
            .send_pit_data(pit_file_bytes.as_ref().unwrap())
            .map_err(|e| e.to_string())?;
    }

    emit_flash(
        &app,
        "pit",
        None,
        0.0,
        Some("Downloading device PIT".to_string()),
    );
    let pit_buffer = odin_manager
        .download_pit_file()
        .map_err(|e| e.to_string())?;
    let pit_data =
        PitData::new(&pit_buffer).map_err(|_| "Failed to unpack device PIT file.".to_string())?;

    let mut partition_infos = Vec::new();
    for entry in resolved_entries {
        let Some(pit_entry) = find_pit_entry_by_filename(&pit_data, &entry.normalized_name) else {
            continue;
        };

        let size = entry.mmap.len() as u64;
        let info = create_firmware_info(
            entry.mmap,
            size,
            entry.is_lz4,
            pit_entry,
            req.skip_size_check,
            &entry.original_name,
        )?;
        partition_infos.push(info);
    }

    let mut mapped_partition_ids = HashSet::new();
    let mut unique_partition_infos = Vec::new();
    for info in partition_infos.into_iter().rev() {
        let id = match &info {
            FirmwareInfo::Normal(f) => f.pit_entry.identifier,
            FirmwareInfo::Lz4(f) => f.pit_entry.identifier,
        };
        if mapped_partition_ids.insert(id) {
            unique_partition_infos.push(info);
        }
    }
    unique_partition_infos.reverse();

    if unique_partition_infos.is_empty() {
        return Err("No package entries matched partitions in the device PIT.".to_string());
    }

    let total_bytes: u64 = unique_partition_infos
        .iter()
        .map(|part| match part {
            FirmwareInfo::Normal(f) => f.file.len() as u64,
            FirmwareInfo::Lz4(f) => f.header.content_size,
        })
        .sum();
    odin_manager
        .set_total_bytes(total_bytes)
        .map_err(|e| e.to_string())?;

    let mut overall_done = 0_u64;
    for info in unique_partition_infos {
        match info {
            FirmwareInfo::Normal(f) => {
                let name = f.pit_entry.partition_name.to_string();
                let partition_total = f.file.len() as u64;
                emit_flash_progress(
                    &app,
                    "partition-start",
                    Some(&name),
                    if total_bytes == 0 {
                        0.0
                    } else {
                        (overall_done as f64 / total_bytes as f64) * 100.0
                    },
                    Some(0),
                    Some(partition_total),
                    Some(overall_done),
                    Some(total_bytes),
                    None::<String>,
                );

                let mut partition_done = 0_u64;
                let mut progress = |delta: u64| {
                    partition_done = partition_done.saturating_add(delta).min(partition_total);
                    let overall_current = overall_done.saturating_add(partition_done);
                    let percent = if total_bytes == 0 {
                        0.0
                    } else {
                        (overall_current as f64 / total_bytes as f64) * 100.0
                    };
                    emit_flash_progress(
                        &app,
                        "partition-progress",
                        Some(&name),
                        percent,
                        Some(partition_done),
                        Some(partition_total),
                        Some(overall_current),
                        Some(total_bytes),
                        None::<String>,
                    );
                };
                odin_manager
                    .send_file_with_progress(&f, &mut progress)
                    .map_err(|e| e.to_string())?;
                overall_done = overall_done.saturating_add(partition_total);
                emit_flash_progress(
                    &app,
                    "partition-done",
                    Some(&name),
                    if total_bytes == 0 {
                        100.0
                    } else {
                        (overall_done as f64 / total_bytes as f64) * 100.0
                    },
                    Some(partition_total),
                    Some(partition_total),
                    Some(overall_done),
                    Some(total_bytes),
                    None::<String>,
                );
            }
            FirmwareInfo::Lz4(f) => {
                let name = f.pit_entry.partition_name.to_string();
                let partition_total = f.header.content_size;
                emit_flash_progress(
                    &app,
                    "partition-start",
                    Some(&name),
                    if total_bytes == 0 {
                        0.0
                    } else {
                        (overall_done as f64 / total_bytes as f64) * 100.0
                    },
                    Some(0),
                    Some(partition_total),
                    Some(overall_done),
                    Some(total_bytes),
                    None::<String>,
                );

                let mut partition_done = 0_u64;
                let mut progress = |delta: u64| {
                    partition_done = partition_done.saturating_add(delta).min(partition_total);
                    let overall_current = overall_done.saturating_add(partition_done);
                    let percent = if total_bytes == 0 {
                        0.0
                    } else {
                        (overall_current as f64 / total_bytes as f64) * 100.0
                    };
                    emit_flash_progress(
                        &app,
                        "partition-progress",
                        Some(&name),
                        percent,
                        Some(partition_done),
                        Some(partition_total),
                        Some(overall_current),
                        Some(total_bytes),
                        None::<String>,
                    );
                };
                odin_manager
                    .send_lz4_file_with_progress(&f, &mut progress)
                    .map_err(|e| e.to_string())?;
                overall_done = overall_done.saturating_add(partition_total);
                emit_flash_progress(
                    &app,
                    "partition-done",
                    Some(&name),
                    if total_bytes == 0 {
                        100.0
                    } else {
                        (overall_done as f64 / total_bytes as f64) * 100.0
                    },
                    Some(partition_total),
                    Some(partition_total),
                    Some(overall_done),
                    Some(total_bytes),
                    None::<String>,
                );
            }
        }
    }

    odin_manager.end_session().map_err(|e| e.to_string())?;
    if !req.no_reboot {
        odin_manager.reboot_device().map_err(|e| e.to_string())?;
    }

    emit_flash(
        &app,
        "done",
        None,
        100.0,
        Some("Flash complete".to_string()),
    );
    Ok(())
}

fn run_download(app: AppHandle, req: DownloadRequest) -> Result<(), String> {
    let version = match req
        .version
        .as_deref()
        .map(str::trim)
        .filter(|v| !v.is_empty())
    {
        Some(version) => version.to_string(),
        None => {
            let info = FusClient::new()
                .and_then(|client| client.fetch_history(&req.model, &req.region))
                .or_else(|_| fetch_version_xml(&req.model, &req.region))
                .map_err(|e| e.to_string())?;
            info.latest
        }
    };

    let mut client = FusClient::new().map_err(|e| e.to_string())?;
    client.fetch_binary_info(&req.model, &req.region, &version);

    let filename = req
        .out_file
        .clone()
        .filter(|f| !f.trim().is_empty())
        .unwrap_or_else(|| client.info.filename.clone());
    let output_path = if let Some(out_dir) = req.out_dir.as_ref().filter(|d| !d.trim().is_empty()) {
        Path::new(out_dir).join(&filename)
    } else {
        PathBuf::from(&filename)
    };
    let output_display = output_path.to_string_lossy().to_string();

    let total = client.info.size;
    let _ = app.emit(
        "download-progress",
        DownloadEvent {
            status: "started".to_string(),
            filename: Some(client.info.filename.clone()),
            path: Some(output_display.clone()),
            bytes: 0,
            total,
            speed_bps: 0.0,
            message: Some(format!("Downloading {}", client.info.filename)),
        },
    );

    let progress = TauriDownloadProgress {
        app: app.clone(),
        position: AtomicU64::new(0),
        total,
        filename: client.info.filename.clone(),
        output_path: output_display.clone(),
        started: Instant::now(),
        verbose: req.verbose,
    };

    client
        .download(&output_path, req.threads.max(1), &progress)
        .map_err(|e| e.to_string())?;

    let _ = app.emit(
        "download-progress",
        DownloadEvent {
            status: "done".to_string(),
            filename: Some(client.info.filename),
            path: Some(output_display),
            bytes: total,
            total,
            speed_bps: 0.0,
            message: Some("Download and decrypt complete".to_string()),
        },
    );
    Ok(())
}

fn pit_entries_from_bytes(bytes: &[u8]) -> Result<Vec<PitEntryView>, String> {
    let pit_data = PitData::new(bytes).map_err(|_| "Failed to unpack PIT data.".to_string())?;
    Ok(pit_data
        .entries
        .iter()
        .map(|entry| PitEntryView {
            id: format!("0x{:X}", entry.identifier),
            partition: entry.partition_name.to_string(),
            flash_file: entry.flash_filename.to_string(),
            size: entry.partition_size(),
        })
        .collect())
}

#[tauri::command]
fn app_info() -> AppInfo {
    AppInfo {
        version: env!("CARGO_PKG_VERSION"),
        default_backend: "vcom",
        backends: vec!["vcom", "nusb"],
    }
}

#[tauri::command]
fn scan_firmware_folder(folder: String, csc_mode: String) -> Result<FolderPackages, String> {
    scan_package_folder_impl(&folder, &csc_mode)
}

async fn run_blocking<T, F>(task: F) -> Result<T, String>
where
    T: Send + 'static,
    F: FnOnce() -> Result<T, String> + Send + 'static,
{
    tauri::async_runtime::spawn_blocking(task)
        .await
        .map_err(|e| e.to_string())?
}

#[tauri::command]
async fn check_updates(model: String, region: String) -> Result<UpdateInfo, String> {
    run_blocking(move || {
        let info = FusClient::new()
            .and_then(|client| client.fetch_history(&model, &region))
            .or_else(|_| fetch_version_xml(&model, &region))
            .map_err(|e| e.to_string())?;
        Ok(UpdateInfo {
            latest: info.latest,
            previous: info.previous,
            beta: info.beta,
        })
    })
    .await
}

#[tauri::command]
async fn detect_download_device(backend: String, wait: bool) -> Result<DeviceStatus, String> {
    run_blocking(move || {
        let backend = backend_from_str(&backend)?;
        let connected = detect_device(backend, wait);
        Ok(DeviceStatus {
            connected,
            label: if connected {
                "Download mode device".to_string()
            } else {
                "No download-mode device".to_string()
            },
        })
    })
    .await
}

#[tauri::command]
async fn reboot_to_download(backend: String) -> Result<(), String> {
    run_blocking(move || {
        let backend = backend_from_str(&backend)?;
        reboot_download(backend).map_err(|e| e.to_string())
    })
    .await
}

#[tauri::command]
fn read_pit_file(path: String) -> Result<Vec<PitEntryView>, String> {
    let mut file = File::open(&path).map_err(|e| format!("Failed to open PIT file: {e}"))?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)
        .map_err(|e| format!("Failed to read PIT file: {e}"))?;
    pit_entries_from_bytes(&bytes)
}

#[tauri::command]
async fn read_device_pit(
    backend: String,
    wait: bool,
    verbose: bool,
) -> Result<Vec<PitEntryView>, String> {
    run_blocking(move || {
        let backend = backend_from_str(&backend)?;
        let usb = create_backend(backend, verbose, wait).map_err(|e| e.to_string())?;
        let mut odin_manager = OdinManager::new(usb, verbose);
        odin_manager.init().map_err(|e| e.to_string())?;
        odin_manager.begin_session().map_err(|e| e.to_string())?;
        let bytes = odin_manager
            .download_pit_file()
            .map_err(|e| e.to_string())?;
        odin_manager.end_session().map_err(|e| e.to_string())?;
        pit_entries_from_bytes(&bytes)
    })
    .await
}

#[tauri::command]
async fn dump_device_pit(
    backend: String,
    wait: bool,
    verbose: bool,
    output: String,
) -> Result<(), String> {
    run_blocking(move || {
        let backend = backend_from_str(&backend)?;
        let usb = create_backend(backend, verbose, wait).map_err(|e| e.to_string())?;
        let mut odin_manager = OdinManager::new(usb, verbose);
        odin_manager.init().map_err(|e| e.to_string())?;
        odin_manager.begin_session().map_err(|e| e.to_string())?;
        let bytes = odin_manager
            .download_pit_file()
            .map_err(|e| e.to_string())?;
        odin_manager.end_session().map_err(|e| e.to_string())?;
        fs::write(&output, bytes).map_err(|e| format!("Failed to write PIT file: {e}"))
    })
    .await
}

#[tauri::command]
async fn verify_md5_files(files: Vec<String>) -> Result<Vec<VerifyResult>, String> {
    run_blocking(move || {
        Ok(files
            .into_iter()
            .map(|file| match File::open(&file).and_then(verify_md5_footer) {
                Ok(_) => VerifyResult {
                    file,
                    ok: true,
                    message: "MD5 verification successful".to_string(),
                },
                Err(e) => VerifyResult {
                    file,
                    ok: false,
                    message: e.to_string(),
                },
            })
            .collect())
    })
    .await
}

#[tauri::command]
fn start_download(app: AppHandle, req: DownloadRequest) -> Result<(), String> {
    thread::spawn(move || {
        if let Err(e) = run_download(app.clone(), req) {
            let _ = app.emit(
                "download-progress",
                DownloadEvent {
                    status: "error".to_string(),
                    filename: None,
                    path: None,
                    bytes: 0,
                    total: 0,
                    speed_bps: 0.0,
                    message: Some(e),
                },
            );
        }
    });
    Ok(())
}

#[tauri::command]
fn start_flash(app: AppHandle, req: FlashRequest) -> Result<(), String> {
    thread::spawn(move || {
        if let Err(e) = run_flash(app.clone(), req) {
            emit_flash(&app, "error", None, 0.0, Some(e));
        }
    });
    Ok(())
}

fn main() {
    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .invoke_handler(tauri::generate_handler![
            app_info,
            scan_firmware_folder,
            check_updates,
            detect_download_device,
            reboot_to_download,
            read_pit_file,
            read_device_pit,
            dump_device_pit,
            verify_md5_files,
            start_download,
            start_flash
        ])
        .run(tauri::generate_context!())
        .expect("error while running samloader desktop UI");
}
