#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use fs2::available_space;
use memmap2::{Mmap, MmapOptions};
use samloader_fus::{DownloadProgress, FusClient, try_fetch_version_xml};
use samloader_odin::{
    FirmwareFile, FirmwareInfo, FirmwareLz4File, Lz4FrameHeader, OdinManager, UsbBackendOption,
    create_backend, detect_device_present_checked, reboot_download, validate_firmware_plan,
    verify_md5_footer,
};
use samloader_pit::{DeviceType, PitData, PitEntry};
use serde::{Deserialize, Serialize};
use std::any::Any;
use std::collections::{HashMap, HashSet};
use std::fs::{self, File};
use std::io::{Cursor, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant};
use tar::Archive;
use tauri::{AppHandle, Emitter, Manager, State};

#[cfg(windows)]
use std::os::windows::ffi::OsStrExt;
#[cfg(windows)]
use std::os::windows::process::CommandExt;

#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

#[cfg(windows)]
#[link(name = "Kernel32")]
unsafe extern "system" {
    fn MoveFileExW(existing: *const u16, destination: *const u16, flags: u32) -> i32;
}

#[cfg(windows)]
fn move_file_no_replace(existing: &Path, destination: &Path) -> std::io::Result<()> {
    let existing: Vec<u16> = existing
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    let destination: Vec<u16> = destination
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    // SAFETY: Both buffers are live, nul-terminated UTF-16 strings. Zero flags
    // deliberately omits MOVEFILE_REPLACE_EXISTING.
    let moved = unsafe { MoveFileExW(existing.as_ptr(), destination.as_ptr(), 0) };
    if moved == 0 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(())
    }
}

#[cfg(not(windows))]
fn move_file_no_replace(existing: &Path, destination: &Path) -> std::io::Result<()> {
    fs::hard_link(existing, destination)?;
    if let Err(error) = fs::remove_file(existing) {
        let _ = fs::remove_file(destination);
        return Err(error);
    }
    Ok(())
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
#[serde(rename_all = "camelCase")]
struct FolderPackages {
    bl: Option<String>,
    ap: Option<String>,
    cp: Option<String>,
    csc: Option<String>,
    userdata: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
struct PackageIdentity {
    path: String,
    canonical_path: String,
    size: u64,
    modified_unix_nanos: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct FolderScanResult {
    packages: FolderPackages,
    identities: Vec<PackageIdentity>,
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

    fn is_empty(&self) -> bool {
        self.bl.is_none()
            && self.ap.is_none()
            && self.cp.is_none()
            && self.csc.is_none()
            && self.userdata.is_none()
    }

    fn has_core_set(&self) -> bool {
        self.bl.is_some() && self.ap.is_some() && self.cp.is_some() && self.csc.is_some()
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct AppInfo {
    version: &'static str,
    default_backend: &'static str,
    backends: Vec<&'static str>,
    default_download_dir: String,
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

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
struct AndroidDeviceInfo {
    status: String,
    connected: bool,
    model: Option<String>,
    region: Option<String>,
    serial: Option<String>,
    message: String,
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
struct DevicePitRead {
    snapshot_id: String,
    rows: Vec<PitEntryView>,
}

struct PitSnapshot {
    id: u64,
    bytes: Vec<u8>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct VerifyResult {
    file: String,
    ok: bool,
    message: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct DownloadPreflight {
    model: String,
    region: String,
    version: String,
    source_filename: String,
    filename: String,
    path: String,
    part_path: String,
    size: u64,
    available_space: u64,
    enough_space: bool,
    conflict: bool,
    part_conflict: bool,
}

fn default_download_threads() -> u64 {
    8
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct DownloadRequest {
    model: String,
    region: String,
    #[serde(default)]
    version: Option<String>,
    #[serde(default)]
    out_dir: Option<String>,
    #[serde(default)]
    out_file: Option<String>,
    #[serde(default = "default_download_threads")]
    threads: u64,
    #[serde(default)]
    verbose: bool,
    #[serde(default)]
    overwrite: bool,
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
    #[serde(default)]
    reviewed_packages: Option<Vec<PackageIdentity>>,
    #[serde(default)]
    zip: Option<String>,
    #[serde(default)]
    reviewed_zip: Option<PackageIdentity>,
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
    source_file: Arc<File>,
    source_offset: u64,
    source_size: usize,
}

#[derive(Clone, Default)]
struct DownloadIdentity {
    filename: Option<String>,
    output_path: Option<String>,
}

struct DownloadControl {
    cancelled: AtomicBool,
    paused: Mutex<bool>,
    pause_changed: Condvar,
    position: AtomicU64,
    total: AtomicU64,
    identity: Mutex<DownloadIdentity>,
}

impl DownloadControl {
    fn new() -> Self {
        Self {
            cancelled: AtomicBool::new(false),
            paused: Mutex::new(false),
            pause_changed: Condvar::new(),
            position: AtomicU64::new(0),
            total: AtomicU64::new(0),
            identity: Mutex::new(DownloadIdentity::default()),
        }
    }

    fn set_identity(&self, filename: String, output_path: String, total: u64) {
        let mut identity = self
            .identity
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        identity.filename = Some(filename);
        identity.output_path = Some(output_path);
        self.total.store(total, Ordering::Release);
    }

    fn snapshot(&self) -> (DownloadIdentity, u64, u64) {
        let identity = self
            .identity
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone();
        (
            identity,
            self.position.load(Ordering::Acquire),
            self.total.load(Ordering::Acquire),
        )
    }

    fn pause(&self) -> bool {
        if self.cancelled.load(Ordering::Acquire) {
            return false;
        }
        let mut paused = self
            .paused
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if *paused {
            false
        } else {
            *paused = true;
            true
        }
    }

    fn resume(&self) -> bool {
        let mut paused = self
            .paused
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if !*paused {
            false
        } else {
            *paused = false;
            self.pause_changed.notify_all();
            true
        }
    }

    fn cancel(&self) -> bool {
        let newly_cancelled = !self.cancelled.swap(true, Ordering::AcqRel);
        let mut paused = self
            .paused
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        *paused = false;
        self.pause_changed.notify_all();
        newly_cancelled
    }

    fn wait_while_paused(&self) {
        let mut paused = self
            .paused
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        while *paused && !self.cancelled.load(Ordering::Acquire) {
            paused = self
                .pause_changed
                .wait(paused)
                .unwrap_or_else(|poisoned| poisoned.into_inner());
        }
    }
}

enum ActiveOperation {
    Download(Arc<DownloadControl>),
    Flash,
    Device(&'static str),
}

impl ActiveOperation {
    fn label(&self) -> &'static str {
        match self {
            Self::Download(_) => "a firmware download",
            Self::Flash => "a flash operation",
            Self::Device(label) => label,
        }
    }
}

#[derive(Default)]
struct GuiState {
    active_operation: Arc<Mutex<Option<ActiveOperation>>>,
    pit_snapshot: Arc<Mutex<Option<PitSnapshot>>>,
    next_pit_snapshot: Arc<AtomicU64>,
}

struct ActiveOperationGuard {
    active_operation: Arc<Mutex<Option<ActiveOperation>>>,
}

impl Drop for ActiveOperationGuard {
    fn drop(&mut self) {
        let mut active = self
            .active_operation
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        *active = None;
    }
}

fn reserve_operation(
    active_operation: &Arc<Mutex<Option<ActiveOperation>>>,
    operation: ActiveOperation,
) -> Result<ActiveOperationGuard, String> {
    let requested = operation.label();
    let mut active = active_operation
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if let Some(current) = active.as_ref() {
        return Err(format!(
            "Cannot start {requested} while {} is active.",
            current.label()
        ));
    }
    *active = Some(operation);
    drop(active);

    Ok(ActiveOperationGuard {
        active_operation: Arc::clone(active_operation),
    })
}

struct TauriDownloadProgress {
    app: AppHandle,
    control: Arc<DownloadControl>,
    total: u64,
    filename: String,
    output_path: String,
    started: Instant,
    last_emit_ms: AtomicU64,
    verbose: bool,
}

impl DownloadProgress for TauriDownloadProgress {
    fn inc(&self, bytes: u64) {
        let position = self.control.position.fetch_add(bytes, Ordering::AcqRel) + bytes;
        let elapsed = self.started.elapsed().as_secs_f64().max(0.001);
        let elapsed_ms = self.started.elapsed().as_millis().min(u64::MAX as u128) as u64;
        let last_emit = self.last_emit_ms.load(Ordering::Acquire);
        if position < self.total && elapsed_ms.saturating_sub(last_emit) < 150 {
            return;
        }
        if self
            .last_emit_ms
            .compare_exchange(last_emit, elapsed_ms, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return;
        }
        let paused = self
            .control
            .paused
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if *paused || self.control.cancelled.load(Ordering::Acquire) {
            return;
        }
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
        self.control.position.load(Ordering::Acquire)
    }

    fn println(&self, msg: String) {
        let paused = self
            .control
            .paused
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if *paused || self.control.cancelled.load(Ordering::Acquire) {
            return;
        }
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

    fn is_cancelled(&self) -> bool {
        self.control.cancelled.load(Ordering::Acquire)
    }

    fn wait_if_paused(&self) {
        self.control.wait_while_paused();
    }
}

struct ResolvedDownload {
    client: FusClient,
    preflight: DownloadPreflight,
    output_path: PathBuf,
    part_path: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct AdbDeviceRow {
    serial: String,
    state: String,
    model_hint: Option<String>,
}

fn normalize_model(value: &str) -> Result<String, String> {
    let value = value.trim().to_ascii_uppercase();
    if !(2..=48).contains(&value.len()) {
        return Err("Enter a valid Samsung model (for example, SM-S931U1).".to_string());
    }
    if !value
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
    {
        return Err(
            "The model may contain only letters, numbers, hyphens, underscores, and periods."
                .to_string(),
        );
    }
    Ok(value)
}

fn normalize_region(value: &str) -> Result<String, String> {
    let value = value.trim().to_ascii_uppercase();
    if value.len() != 3 || !value.bytes().all(|byte| byte.is_ascii_alphanumeric()) {
        return Err("Enter a valid sales CSC/region code (for example, XAA).".to_string());
    }
    Ok(value)
}

fn normalize_detected_region(value: &str) -> Option<String> {
    let region = normalize_region(value).ok()?;
    if matches!(
        region.as_str(),
        "ODM" | "OJM" | "OLB" | "OXA" | "OXE" | "OXM" | "OXX" | "OYN" | "OWO"
    ) {
        None
    } else {
        Some(region)
    }
}

fn normalize_version(value: &str) -> Result<String, String> {
    let value = value.trim().to_ascii_uppercase();
    if value.is_empty() || value.len() > 256 {
        return Err("The selected firmware version is invalid.".to_string());
    }
    if !value
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'/' | b'-' | b'_' | b'.'))
    {
        return Err("The firmware version contains unsupported characters.".to_string());
    }
    Ok(value)
}

fn decrypted_filename(server_filename: &str) -> Result<String, String> {
    let basename = server_filename
        .trim()
        .rsplit(['/', '\\'])
        .next()
        .filter(|name| !name.is_empty())
        .ok_or_else(|| "Samsung returned an invalid firmware filename.".to_string())?;
    let mut filename = basename.to_string();
    if filename.to_ascii_lowercase().ends_with(".enc4") {
        filename.truncate(filename.len() - ".enc4".len());
    }
    validate_output_filename(&filename)?;
    Ok(filename)
}

fn validate_output_filename(filename: &str) -> Result<(), String> {
    if filename.is_empty() || filename == "." || filename == ".." || filename.contains('\0') {
        return Err("Choose a valid output filename.".to_string());
    }

    #[cfg(windows)]
    {
        if filename.bytes().any(|byte| {
            byte < 32
                || matches!(
                    byte,
                    b'<' | b'>' | b':' | b'"' | b'/' | b'\\' | b'|' | b'?' | b'*'
                )
        }) {
            return Err(
                "The output filename contains characters Windows does not allow.".to_string(),
            );
        }
        if filename.ends_with([' ', '.']) {
            return Err("Windows filenames cannot end with a space or period.".to_string());
        }
        let stem = filename
            .split('.')
            .next()
            .unwrap_or_default()
            .trim_end_matches([' ', '.'])
            .to_ascii_uppercase();
        let reserved = matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL")
            || (stem.len() == 4
                && (stem.starts_with("COM") || stem.starts_with("LPT"))
                && matches!(stem.as_bytes()[3], b'1'..=b'9'));
        if reserved {
            return Err(format!("\"{filename}\" is a reserved Windows filename."));
        }
    }

    Ok(())
}

fn make_absolute(path: PathBuf) -> Result<PathBuf, String> {
    if path.is_absolute() {
        Ok(path)
    } else {
        std::env::current_dir()
            .map(|cwd| cwd.join(path))
            .map_err(|e| format!("Could not determine the current folder: {e}"))
    }
}

fn default_download_dir() -> Result<PathBuf, String> {
    if let Some(downloads) = dirs::download_dir().filter(|path| path.is_dir()) {
        return Ok(downloads);
    }
    std::env::current_dir()
        .map_err(|e| format!("Could not locate the Downloads folder or current folder: {e}"))
}

fn output_path_for_request(
    req: &DownloadRequest,
    server_filename: &str,
) -> Result<PathBuf, String> {
    let output_dir = req
        .out_dir
        .as_deref()
        .map(str::trim)
        .filter(|v| !v.is_empty());
    let requested_file = req
        .out_file
        .as_deref()
        .map(str::trim)
        .filter(|v| !v.is_empty());

    let output_path = match (requested_file, output_dir) {
        (Some(filename), Some(directory)) => {
            let filename_path = Path::new(filename);
            if filename_path
                .parent()
                .is_some_and(|parent| !parent.as_os_str().is_empty())
            {
                return Err(
                    "When an output folder is selected, the custom output name must be a filename, not a path."
                        .to_string(),
                );
            }
            validate_output_filename(filename)?;
            Path::new(directory).join(filename)
        }
        (Some(path), None) => {
            let path = PathBuf::from(path);
            let filename = path
                .file_name()
                .and_then(|name| name.to_str())
                .ok_or_else(|| "Choose a valid output filename.".to_string())?;
            validate_output_filename(filename)?;
            if path.is_absolute()
                || path
                    .parent()
                    .is_some_and(|parent| !parent.as_os_str().is_empty())
            {
                path
            } else {
                default_download_dir()?.join(path)
            }
        }
        (None, Some(directory)) => Path::new(directory).join(decrypted_filename(server_filename)?),
        (None, None) => default_download_dir()?.join(decrypted_filename(server_filename)?),
    };

    make_absolute(output_path)
}

fn part_path_for(output_path: &Path) -> PathBuf {
    let mut part_name = output_path.as_os_str().to_os_string();
    part_name.push(".part");
    PathBuf::from(part_name)
}

fn path_exists_or_symlink(path: &Path) -> bool {
    path.symlink_metadata().is_ok()
}

fn regular_file_size(path: &Path) -> u64 {
    path.symlink_metadata()
        .ok()
        .filter(|metadata| metadata.file_type().is_file())
        .map(|metadata| metadata.len())
        .unwrap_or(0)
}

fn validate_output_location(output_path: &Path, part_path: &Path) -> Result<(u64, u64), String> {
    let parent = output_path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    if !parent.exists() {
        return Err(format!(
            "The output folder \"{}\" does not exist. Choose an existing folder.",
            parent.display()
        ));
    }
    if !parent.is_dir() {
        return Err(format!("\"{}\" is not a folder.", parent.display()));
    }
    if output_path.is_dir() {
        return Err(format!(
            "The output path \"{}\" is a folder. Choose a filename.",
            output_path.display()
        ));
    }
    if part_path.is_dir() {
        return Err(format!(
            "The temporary path \"{}\" is a folder and cannot be replaced.",
            part_path.display()
        ));
    }

    let available = available_space(parent).map_err(|e| {
        format!(
            "Could not check free space in \"{}\": {e}",
            parent.display()
        )
    })?;
    Ok((available, regular_file_size(part_path)))
}

fn resolve_download(req: &DownloadRequest) -> Result<ResolvedDownload, String> {
    let model = normalize_model(&req.model)?;
    let region = normalize_region(&req.region)?;
    if req.threads == 0 || req.threads > 32 {
        return Err("Parallel connections must be between 1 and 32.".to_string());
    }

    let mut client = FusClient::new().map_err(|e| {
        format!("Could not connect to Samsung's firmware service. Check your connection and try again: {e}")
    })?;
    let version = match req
        .version
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        Some(version) => normalize_version(version)?,
        None => {
            let history = client
                .try_fetch_history(&model, &region)
                .or_else(|_| try_fetch_version_xml(&model, &region))
                .map_err(|e| {
                    format!(
                        "No firmware history could be found for {model} / {region}. Verify the model and sales CSC, then try again: {e}"
                    )
                })?;
            if history.latest.trim().is_empty() {
                return Err(format!(
                    "Samsung did not return a latest firmware for {model} / {region}. Verify the model and sales CSC."
                ));
            }
            normalize_version(&history.latest)?
        }
    };

    client
        .try_fetch_binary_info(&model, &region, &version)
        .map_err(|e| {
            format!(
                "Firmware {version} is not available for {model} / {region}. Verify the version and region: {e}"
            )
        })?;
    if client.info.filename.trim().is_empty()
        || client.info.version.trim().is_empty()
        || client.info.path.trim().is_empty()
        || client.info.key.len() != 16
        || client.info.size == 0
    {
        return Err(
            "Samsung returned incomplete firmware metadata. Try another version or region."
                .to_string(),
        );
    }

    let output_path = output_path_for_request(req, &client.info.filename)?;
    let part_path = part_path_for(&output_path);
    let (available, part_size) = validate_output_location(&output_path, &part_path)?;
    let enough_space = available.saturating_add(part_size) >= client.info.size;
    let filename = output_path
        .file_name()
        .map(|name| name.to_string_lossy().to_string())
        .ok_or_else(|| "Choose a valid output filename.".to_string())?;
    let preflight = DownloadPreflight {
        model,
        region,
        version: client.info.version.clone(),
        source_filename: client.info.filename.clone(),
        filename,
        path: output_path.to_string_lossy().to_string(),
        part_path: part_path.to_string_lossy().to_string(),
        size: client.info.size,
        available_space: available,
        enough_space,
        conflict: path_exists_or_symlink(&output_path),
        part_conflict: path_exists_or_symlink(&part_path),
    };

    Ok(ResolvedDownload {
        client,
        preflight,
        output_path,
        part_path,
    })
}

fn parse_adb_devices(output: &str) -> Vec<AdbDeviceRow> {
    output
        .lines()
        .map(str::trim)
        .filter(|line| {
            !line.is_empty() && !line.starts_with("List of devices") && !line.starts_with('*')
        })
        .filter_map(|line| {
            let mut fields = line.split_whitespace();
            let serial = fields.next()?.to_string();
            let state = fields.next()?.to_ascii_lowercase();
            let model_hint = fields
                .find_map(|field| field.strip_prefix("model:"))
                .filter(|model| !model.is_empty())
                .map(str::to_string);
            Some(AdbDeviceRow {
                serial,
                state,
                model_hint,
            })
        })
        .collect()
}

fn device_status_from_rows(rows: &[AdbDeviceRow]) -> AndroidDeviceInfo {
    if rows.is_empty() {
        return AndroidDeviceInfo {
            status: "not_found".to_string(),
            connected: false,
            model: None,
            region: None,
            serial: None,
            message: "No Android device was found. Connect it by USB and enable USB debugging."
                .to_string(),
        };
    }
    if rows.len() > 1 {
        return AndroidDeviceInfo {
            status: "multiple".to_string(),
            connected: false,
            model: None,
            region: None,
            serial: None,
            message: format!(
                "{} Android devices are visible. Leave only the device you want to identify connected.",
                rows.len()
            ),
        };
    }

    let row = &rows[0];
    match row.state.as_str() {
        "device" => AndroidDeviceInfo {
            status: "connected".to_string(),
            connected: true,
            model: row.model_hint.clone(),
            region: None,
            serial: Some(row.serial.clone()),
            message: "Android device connected.".to_string(),
        },
        "unauthorized" => AndroidDeviceInfo {
            status: "unauthorized".to_string(),
            connected: false,
            model: row.model_hint.clone(),
            region: None,
            serial: Some(row.serial.clone()),
            message: "Unlock the device and approve the USB debugging prompt, then try again."
                .to_string(),
        },
        _ => AndroidDeviceInfo {
            status: "offline".to_string(),
            connected: false,
            model: row.model_hint.clone(),
            region: None,
            serial: Some(row.serial.clone()),
            message: format!(
                "The Android device is not ready (ADB state: {}). Reconnect it and try again.",
                row.state
            ),
        },
    }
}

fn adb_command() -> Command {
    let mut command = Command::new("adb");
    #[cfg(windows)]
    command.creation_flags(CREATE_NO_WINDOW);
    command
}

fn command_output_with_timeout(mut command: Command, timeout: Duration) -> std::io::Result<Output> {
    command.stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = command.spawn()?;
    let mut stdout = child
        .stdout
        .take()
        .ok_or_else(|| std::io::Error::other("command stdout could not be captured"))?;
    let mut stderr = child
        .stderr
        .take()
        .ok_or_else(|| std::io::Error::other("command stderr could not be captured"))?;
    let stdout_reader = thread::spawn(move || {
        let mut bytes = Vec::new();
        stdout.read_to_end(&mut bytes).map(|_| bytes)
    });
    let stderr_reader = thread::spawn(move || {
        let mut bytes = Vec::new();
        stderr.read_to_end(&mut bytes).map(|_| bytes)
    });
    let started = Instant::now();
    loop {
        if let Some(status) = child.try_wait()? {
            let stdout = stdout_reader
                .join()
                .map_err(|_| std::io::Error::other("stdout reader failed"))??;
            let stderr = stderr_reader
                .join()
                .map_err(|_| std::io::Error::other("stderr reader failed"))??;
            return Ok(Output {
                status,
                stdout,
                stderr,
            });
        }
        if started.elapsed() >= timeout {
            let _ = child.kill();
            let _ = child.wait();
            let _ = stdout_reader.join();
            let _ = stderr_reader.join();
            return Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "command timed out",
            ));
        }
        thread::sleep(Duration::from_millis(25));
    }
}

fn parse_android_properties(output: &str) -> HashMap<String, String> {
    output
        .lines()
        .filter_map(|line| {
            let (key, value) = line.trim().split_once("]: [")?;
            let key = key.strip_prefix('[')?;
            let value = value.strip_suffix(']')?.trim();
            if value.is_empty() || value.eq_ignore_ascii_case("unknown") {
                None
            } else {
                Some((key.to_string(), value.to_string()))
            }
        })
        .collect()
}

fn adb_properties(serial: &str) -> HashMap<String, String> {
    let mut command = adb_command();
    command.args(["-s", serial, "shell", "getprop"]);
    let output = match command_output_with_timeout(command, Duration::from_secs(5)) {
        Ok(output) => output,
        Err(_) => return HashMap::new(),
    };
    if !output.status.success() {
        return HashMap::new();
    }
    parse_android_properties(&String::from_utf8_lossy(&output.stdout))
}

fn detect_android_device_impl() -> AndroidDeviceInfo {
    let mut command = adb_command();
    command.args(["devices", "-l"]);
    let output = match command_output_with_timeout(command, Duration::from_secs(8)) {
        Ok(output) => output,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return AndroidDeviceInfo {
                status: "adb_missing".to_string(),
                connected: false,
                model: None,
                region: None,
                serial: None,
                message: "ADB was not found. Install Android Platform Tools and make adb available in PATH."
                    .to_string(),
            };
        }
        Err(error) if error.kind() == std::io::ErrorKind::TimedOut => {
            return AndroidDeviceInfo {
                status: "not_found".to_string(),
                connected: false,
                model: None,
                region: None,
                serial: None,
                message: "ADB did not respond. Reconnect the device or restart the ADB server."
                    .to_string(),
            };
        }
        Err(error) => {
            return AndroidDeviceInfo {
                status: "adb_missing".to_string(),
                connected: false,
                model: None,
                region: None,
                serial: None,
                message: format!("ADB could not be started: {error}"),
            };
        }
    };

    if !output.status.success() {
        let detail = String::from_utf8_lossy(&output.stderr).trim().to_string();
        return AndroidDeviceInfo {
            status: "not_found".to_string(),
            connected: false,
            model: None,
            region: None,
            serial: None,
            message: if detail.is_empty() {
                "ADB could not list connected devices. Restart ADB and try again.".to_string()
            } else {
                format!("ADB could not list connected devices: {detail}")
            },
        };
    }

    let rows = parse_adb_devices(&String::from_utf8_lossy(&output.stdout));
    let mut result = device_status_from_rows(&rows);
    if !result.connected {
        return result;
    }

    let serial = result.serial.clone().unwrap_or_default();
    let properties = adb_properties(&serial);
    result.model = [
        "ro.product.vendor.model",
        "ro.product.model",
        "ro.product.system.model",
    ]
    .iter()
    .find_map(|property| {
        properties
            .get(*property)
            .and_then(|model| normalize_model(model).ok())
    })
    .or_else(|| {
        rows[0]
            .model_hint
            .as_deref()
            .and_then(|model| normalize_model(model).ok())
    });
    result.region = [
        "persist.omc.sales_code",
        "ro.csc.sales_code",
        "ril.sales_code",
        "ro.boot.sales_code",
        "ro.boot.carrierid",
        "ro.boot.hwc",
    ]
    .iter()
    .find_map(|property| {
        properties
            .get(*property)
            .and_then(|region| normalize_detected_region(region))
    });

    result.message = match (&result.model, &result.region) {
        (Some(model), Some(region)) => format!("Detected {model} with sales CSC {region}."),
        (Some(model), None) => format!(
            "Detected {model}, but the sales CSC was unavailable. Select the device's CSC manually."
        ),
        (None, _) => {
            "The device is connected, but its Samsung model could not be read.".to_string()
        }
    };
    result
}

fn panic_message(payload: Box<dyn Any + Send>) -> String {
    if let Some(message) = payload.downcast_ref::<&str>() {
        (*message).to_string()
    } else if let Some(message) = payload.downcast_ref::<String>() {
        message.clone()
    } else {
        "unknown internal error".to_string()
    }
}

fn emit_download(
    app: &AppHandle,
    control: &DownloadControl,
    status: &str,
    message: impl Into<Option<String>>,
) {
    let (identity, bytes, total) = control.snapshot();
    let _ = app.emit(
        "download-progress",
        DownloadEvent {
            status: status.to_string(),
            filename: identity.filename,
            path: identity.output_path,
            bytes,
            total,
            speed_bps: 0.0,
            message: message.into(),
        },
    );
}

fn is_tar_package_name(name: &str) -> bool {
    let name = name.to_ascii_lowercase();
    name.ends_with(".tar") || name.ends_with(".tar.md5")
}

fn package_basename(path: &str) -> Result<String, String> {
    Path::new(path)
        .file_name()
        .map(|name| name.to_string_lossy().to_string())
        .filter(|name| !name.trim().is_empty())
        .ok_or_else(|| format!("Firmware package path \"{path}\" has no filename."))
}

fn validate_package_slot(path: &str, slot: &str, prefixes: &[&str]) -> Result<(), String> {
    let filename = package_basename(path)?;
    let upper = filename.to_ascii_uppercase();
    if !prefixes.iter().any(|prefix| upper.starts_with(prefix)) {
        return Err(format!(
            "The {slot} slot contains \"{filename}\", which is not a {slot} package."
        ));
    }
    Ok(())
}

fn package_build_token(path: &str, prefix: &str) -> Option<String> {
    let filename = package_basename(path).ok()?.to_ascii_uppercase();
    filename
        .strip_prefix(prefix)?
        .split('_')
        .next()
        .filter(|token| !token.is_empty())
        .map(str::to_string)
}

fn package_model_identity(path: &str) -> Option<String> {
    let filename = package_basename(path).ok()?.to_ascii_uppercase();
    filename.split('_').find_map(|token| {
        let bytes = token.as_bytes();
        if bytes.len() < 5
            || !bytes[0].is_ascii_alphabetic()
            || !bytes[1..4].iter().all(u8::is_ascii_digit)
            || !bytes[4].is_ascii_alphabetic()
        {
            return None;
        }
        let length = if bytes.get(5).is_some_and(u8::is_ascii_digit) {
            6
        } else {
            5
        };
        Some(token[..length].to_string())
    })
}

fn validate_package_selection(
    packages: &FolderPackages,
    csc_mode: &str,
    repartition: bool,
) -> Result<(), String> {
    if !matches!(csc_mode, "home" | "wipe") {
        return Err(format!("Unknown CSC mode: {csc_mode}"));
    }

    if let Some(path) = &packages.bl {
        validate_package_slot(path, "BL", &["BL_"])?;
    }
    if let Some(path) = &packages.ap {
        validate_package_slot(path, "AP", &["AP_"])?;
    }
    if let Some(path) = &packages.cp {
        validate_package_slot(path, "CP", &["CP_"])?;
    }
    if let Some(path) = &packages.userdata {
        validate_package_slot(path, "USERDATA", &["USERDATA_"])?;
    }
    if let Some(path) = &packages.csc {
        match csc_mode {
            "home" => validate_package_slot(path, "HOME_CSC", &["HOME_CSC_"])?,
            "wipe" => {
                let filename = package_basename(path)?;
                let upper = filename.to_ascii_uppercase();
                if !upper.starts_with("CSC_") || upper.starts_with("HOME_CSC_") {
                    return Err(format!(
                        "The CSC slot contains \"{filename}\", which is not a factory-reset CSC package."
                    ));
                }
            }
            _ => unreachable!(),
        }
    }

    let mut selected_paths = HashSet::new();
    for path in [
        packages.bl.as_deref(),
        packages.ap.as_deref(),
        packages.cp.as_deref(),
        packages.csc.as_deref(),
        packages.userdata.as_deref(),
    ]
    .into_iter()
    .flatten()
    {
        let normalized = path.trim().replace('/', "\\").to_ascii_lowercase();
        if !selected_paths.insert(normalized) {
            return Err(format!(
                "Firmware package \"{path}\" was selected in more than one slot."
            ));
        }
    }

    if let (Some(bl), Some(ap)) = (&packages.bl, &packages.ap) {
        let bl_build = package_build_token(bl, "BL_");
        let ap_build = package_build_token(ap, "AP_");
        if let (Some(bl_build), Some(ap_build)) = (bl_build, ap_build)
            && bl_build != ap_build
        {
            return Err(format!(
                "BL and AP packages belong to different firmware builds ({bl_build} vs {ap_build}). Select packages from one firmware release."
            ));
        }
    }

    let mut package_models: HashMap<String, String> = HashMap::new();
    for (slot, path) in [
        ("BL", packages.bl.as_deref()),
        ("AP", packages.ap.as_deref()),
        ("CP", packages.cp.as_deref()),
        ("CSC", packages.csc.as_deref()),
        ("USERDATA", packages.userdata.as_deref()),
    ] {
        if let Some(path) = path
            && let Some(model) = package_model_identity(path)
        {
            package_models.insert(slot.to_string(), model);
        }
    }
    if let Some(expected_model) = package_models.values().next() {
        let mismatched: Vec<String> = package_models
            .iter()
            .filter(|(_, model)| *model != expected_model)
            .map(|(slot, model)| format!("{slot}={model}"))
            .collect();
        if !mismatched.is_empty() {
            return Err(format!(
                "Selected packages target different device families (expected {expected_model}; {}). Select every package from one model and release.",
                mismatched.join(", ")
            ));
        }
    }

    if repartition {
        if csc_mode != "wipe" {
            return Err(
                "Repartition requires factory-reset CSC mode; HOME_CSC cannot be used.".to_string(),
            );
        }
        if !packages.has_core_set() {
            return Err(
                "Repartition requires a complete BL, AP, CP, and factory-reset CSC package set."
                    .to_string(),
            );
        }
    }

    Ok(())
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

fn select_zip_packages(names: &[String], csc_mode: &str) -> Result<FolderPackages, String> {
    let mut bl = Vec::new();
    let mut ap = Vec::new();
    let mut cp = Vec::new();
    let mut home_csc = Vec::new();
    let mut csc = Vec::new();
    let mut userdata = Vec::new();

    for name in names {
        if !is_tar_package_name(name) {
            continue;
        }
        let upper_name = name.to_ascii_uppercase();
        let path = PathBuf::from(name);
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
                    "HOME_CSC package was not found in the ZIP. Use wipe mode to select CSC instead."
                        .to_string(),
                );
            }
            home_csc
        }
        "wipe" => {
            if csc.is_none() && home_csc.is_some() {
                return Err(
                    "CSC package was not found in the ZIP. Use HOME_CSC mode instead.".to_string(),
                );
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

struct ZipFlashPlan {
    sources: Vec<TarSource>,
    packages: FolderPackages,
    non_stored: Vec<String>,
}

fn scan_zip_flash_plan(zip_path: &str, csc_mode: &str) -> Result<ZipFlashPlan, String> {
    let file = File::open(zip_path)
        .map_err(|error| format!("Failed to open firmware ZIP \"{zip_path}\": {error}"))?;
    let entries =
        samloader_fus::list_firmware_zip_entries(&file).map_err(|error| error.to_string())?;

    let names: Vec<String> = entries.iter().map(|entry| entry.name.clone()).collect();
    let packages = select_zip_packages(&names, csc_mode)?;
    if packages.is_empty() {
        return Err(format!(
            "The firmware ZIP \"{zip_path}\" does not contain any BL/AP/CP/CSC/USERDATA packages."
        ));
    }

    let file_len = file
        .metadata()
        .map_err(|error| format!("Failed to inspect firmware ZIP \"{zip_path}\": {error}"))?
        .len();

    let mut member_names = Vec::new();
    packages.append_to(&mut member_names);

    let file = Arc::new(file);
    let mut sources = Vec::new();
    let mut non_stored = Vec::new();
    for member in &member_names {
        let entry = entries
            .iter()
            .find(|entry| &entry.name == member)
            .expect("selected members always come from the entry list");
        if !entry.is_stored {
            non_stored.push(member.clone());
            continue;
        }
        if entry.size == 0 {
            return Err(format!("ZIP member \"{member}\" is empty."));
        }
        // Fail closed if the central directory declares a byte range that runs
        // off the end of the archive; mapping it would fault out of bounds
        // (SIGBUS on non-Windows) during MD5 verification or the TAR walk.
        match entry.data_start.checked_add(entry.size) {
            Some(end) if end <= file_len => {}
            _ => {
                return Err(format!(
                    "ZIP member \"{member}\" declares a byte range beyond the end of the archive."
                ));
            }
        }
        sources.push(TarSource {
            display_name: format!("{member} (in {zip_path})"),
            file: Arc::clone(&file),
            base_offset: entry.data_start,
            size: entry.size,
            verify_md5: member.to_lowercase().ends_with(".md5"),
        });
    }
    Ok(ZipFlashPlan {
        sources,
        packages,
        non_stored,
    })
}

fn verify_reviewed_zip(zip_path: &str, reviewed: &PackageIdentity) -> Result<(), String> {
    let current = file_identity(zip_path)?;
    if current != *reviewed {
        return Err(
            "The firmware ZIP changed after it was reviewed. Scan the ZIP again and review the updated package list before flashing."
                .to_string(),
        );
    }
    Ok(())
}

fn file_identity(path: &str) -> Result<PackageIdentity, String> {
    let canonical = fs::canonicalize(path)
        .map_err(|error| format!("Failed to resolve firmware package \"{path}\": {error}"))?;
    let metadata = fs::metadata(&canonical)
        .map_err(|error| format!("Failed to inspect firmware package \"{path}\": {error}"))?;
    if !metadata.is_file() || metadata.len() == 0 {
        return Err(format!(
            "Firmware package \"{path}\" is no longer a non-empty file. Scan the folder again."
        ));
    }
    let modified = metadata.modified().map_err(|error| {
        format!("Failed to read the modification time for firmware package \"{path}\": {error}")
    })?;
    let modified = modified
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|_| format!("Firmware package \"{path}\" has an invalid modification time."))?;
    let modified_unix_nanos = (u128::from(modified.as_secs()) * 1_000_000_000
        + u128::from(modified.subsec_nanos()))
    .to_string();
    Ok(PackageIdentity {
        path: path.to_string(),
        canonical_path: canonical.to_string_lossy().to_string(),
        size: metadata.len(),
        modified_unix_nanos,
    })
}

fn package_identities(packages: &FolderPackages) -> Result<Vec<PackageIdentity>, String> {
    let mut paths = Vec::new();
    packages.append_to(&mut paths);
    paths.into_iter().map(|path| file_identity(&path)).collect()
}

fn verify_reviewed_folder(
    selected: &FolderPackages,
    reviewed: &[PackageIdentity],
) -> Result<(), String> {
    let current = package_identities(selected)?;
    if current != reviewed {
        return Err(
            "The firmware folder changed after it was reviewed. Scan the folder again and review the updated package list before flashing."
                .to_string(),
        );
    }
    Ok(())
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

fn validate_download_list_entry(is_file: bool, size: u64, package: &str) -> Result<(), String> {
    if !is_file || size == 0 {
        return Err(format!(
            "download-list.txt in \"{package}\" is not a non-empty file."
        ));
    }
    if size > MAX_DOWNLOAD_LIST_SIZE {
        return Err(format!(
            "download-list.txt in \"{package}\" is unexpectedly large ({size} bytes)."
        ));
    }
    Ok(())
}

/// One TAR archive to scan, addressed as a byte range of an underlying file.
/// A standalone `.tar`/`.tar.md5` file is `base_offset == 0, size == file length`;
/// a member of a firmware ZIP uses the member's raw data range.
struct TarSource {
    display_name: String,
    file: Arc<File>,
    base_offset: u64,
    size: u64,
    verify_md5: bool,
}

fn tar_sources_from_paths(packages: &[String]) -> Result<Vec<TarSource>, String> {
    let mut sources = Vec::new();
    for pkg in packages {
        let package_path = Path::new(pkg);
        let filename = package_path
            .file_name()
            .map(|name| name.to_string_lossy())
            .unwrap_or_default();
        if !is_tar_package_name(&filename) {
            return Err(format!(
                "Unsupported firmware package \"{pkg}\". Select a .tar or .tar.md5 package."
            ));
        }
        let metadata = package_path
            .metadata()
            .map_err(|e| format!("Failed to inspect package file \"{pkg}\": {e}"))?;
        if !metadata.is_file() {
            return Err(format!("Firmware package \"{pkg}\" is not a file."));
        }
        if metadata.len() == 0 {
            return Err(format!("Firmware package \"{pkg}\" is empty."));
        }
        let file =
            File::open(pkg).map_err(|e| format!("Failed to open package file \"{pkg}\": {e}"))?;
        sources.push(TarSource {
            display_name: pkg.clone(),
            file: Arc::new(file),
            base_offset: 0,
            size: metadata.len(),
            verify_md5: pkg.to_lowercase().ends_with(".md5"),
        });
    }
    Ok(sources)
}

fn scan_tar_packages(
    packages: &[String],
    skip_md5: bool,
    app: &AppHandle,
) -> Result<(Vec<IndexedEntry>, Option<Vec<u8>>), String> {
    let sources = tar_sources_from_paths(packages)?;
    scan_tar_sources(&sources, skip_md5, app)
}

fn scan_tar_sources(
    sources: &[TarSource],
    skip_md5: bool,
    app: &AppHandle,
) -> Result<(Vec<IndexedEntry>, Option<Vec<u8>>), String> {
    // Map each source's byte range once; the region serves both MD5-footer
    // verification and TAR header walking. Payload entries get their own
    // per-entry mappings so FirmwareInfo can own them, exactly as before.
    let mut regions = Vec::new();
    for source in sources {
        let region_size = usize::try_from(source.size).map_err(|_| {
            format!(
                "Firmware package \"{}\" is too large for this platform.",
                source.display_name
            )
        })?;
        let region = unsafe {
            MmapOptions::new()
                .offset(source.base_offset)
                .len(region_size)
                .map(source.file.as_ref())
        }
        .map_err(|e| {
            format!(
                "Failed to memory map package \"{}\": {e}",
                source.display_name
            )
        })?;
        regions.push(region);
    }

    if !skip_md5 {
        for (source, region) in sources.iter().zip(&regions) {
            if source.verify_md5 {
                emit_flash(
                    app,
                    "verifying-md5",
                    None,
                    0.0,
                    Some(format!("Verifying {}", source.display_name)),
                );
                verify_md5_footer(Cursor::new(&region[..])).map_err(|e| {
                    format!(
                        "MD5 verification failed for \"{}\": {e}",
                        source.display_name
                    )
                })?;
            }
        }
    }

    let mut archives_download_lists: Vec<HashSet<String>> = Vec::new();
    let mut all_packages_entries: Vec<Vec<IndexedEntry>> = Vec::new();

    for (source, region) in sources.iter().zip(&regions) {
        let pkg = &source.display_name;
        let source_file = Arc::clone(&source.file);
        let mut archive = Archive::new(Cursor::new(&region[..]));
        let entries = archive
            .entries_with_seek()
            .map_err(|e| format!("Failed to read archive entries for \"{pkg}\": {e}"))?;

        let mut package_entries = Vec::new();

        for entry_res in entries {
            let entry =
                entry_res.map_err(|e| format!("Corrupted archive entry in \"{pkg}\": {e}"))?;
            let entry_type = entry.header().entry_type();
            let is_file = entry_type.is_file();
            let entry_path = entry
                .path()
                .map_err(|error| format!("Invalid archive entry path in \"{pkg}\": {error}"))?
                .to_string_lossy()
                .to_string();

            let offset = source.base_offset + entry.raw_file_position();
            let size = entry.size();
            let mapping_size = usize::try_from(size).map_err(|_| {
                format!(
                    "Archive entry \"{entry_path}\" in \"{pkg}\" is too large for this platform."
                )
            })?;
            let (normalized_name, is_lz4) = normalize_basename(&entry_path);

            if normalized_name.eq_ignore_ascii_case("download-list.txt") {
                validate_download_list_entry(is_file, size, pkg)?;
                let mut reader = entry;
                let mut content = String::new();
                reader.read_to_string(&mut content).map_err(|error| {
                    format!("Failed to read download-list.txt in \"{pkg}\": {error}")
                })?;
                let mut download_list = HashSet::new();
                for line in content
                    .lines()
                    .map(str::trim)
                    .filter(|line| !line.is_empty())
                {
                    let (name, _) = normalize_basename(line);
                    if name.is_empty() {
                        return Err(format!(
                            "download-list.txt in \"{pkg}\" contains an invalid entry: {line:?}."
                        ));
                    }
                    download_list.insert(name.to_ascii_lowercase());
                }
                if download_list.is_empty() {
                    return Err(format!(
                        "download-list.txt in \"{pkg}\" contains no payload entries."
                    ));
                }
                archives_download_lists.push(download_list);
            } else {
                if !validate_payload_entry(is_file, entry_type.is_dir(), size, &entry_path)
                    .map_err(|error| format!("Invalid archive entry in \"{pkg}\": {error}"))?
                {
                    continue;
                }
                let mmap = unsafe {
                    MmapOptions::new()
                        .offset(offset)
                        .len(mapping_size)
                        .map(source.file.as_ref())
                }
                .map_err(|e| {
                    format!("Failed to memory map entry \"{entry_path}\" in package \"{pkg}\": {e}")
                })?;

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
            if entry.normalized_name.to_ascii_lowercase().ends_with(".pit") {
                if pit_entry.is_some() {
                    return Err(
                        "More than one PIT file was found in the selected firmware packages. Select one coherent firmware set."
                            .to_string(),
                    );
                }
                pit_entry = Some(entry);
            } else if let Some(allowlist) = download_allowlist {
                if allowlist.contains(&entry.normalized_name.to_ascii_lowercase()) {
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

fn pit_target_key(entry: &PitEntry) -> (u32, u32, u32) {
    (
        entry.binary_type as u32,
        entry.device_type as u32,
        entry.identifier,
    )
}

type PitLayoutGroup = (u32, u32);

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

fn validate_pit_layout(pit_data: &PitData, label: &str) -> Result<(), String> {
    if pit_data.entries.is_empty() {
        return Err(format!("The {label} PIT contains no partition entries."));
    }

    let mut names = HashSet::new();
    let mut flashable_count = 0_usize;
    for entry in &pit_data.entries {
        let partition_name = entry.partition_name.to_string_lossy().trim().to_string();
        if partition_name.is_empty() {
            return Err(format!("The {label} PIT contains an unnamed partition."));
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
            // Reserved/key entries may legitimately have zero-sized payload
            // mappings such as UL_KEYS/ul_key.bin. USERDATA may be
            // grow-to-fill.
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
            entry
                .block_size_or_offset
                .checked_add(entry.block_count)
                .ok_or_else(|| {
                    format!(
                        "The {label} PIT partition {partition_name} overflows its 32-bit block address space."
                    )
                })?;
        }

        if !flash_filename.is_empty()
            && (entry.block_count > 0
                || is_growable_zero_partition(&partition_name)
                || is_allowed_zero_sized_payload_mapping(&partition_name, flash_filename))
        {
            flashable_count += 1;
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

fn create_firmware_info<'a>(
    mmap: Mmap,
    source_size: u64,
    is_lz4_suffix: bool,
    pit_entry: &'a PitEntry,
    skip_size_check: bool,
    file_display_name: &str,
) -> Result<FirmwareInfo<'a>, String> {
    let lz4_frame_header = if is_lz4_payload(&mmap, is_lz4_suffix) {
        let cursor = std::io::Cursor::new(&mmap);
        let header = Lz4FrameHeader::parse(cursor)
            .map_err(|e| format!("Failed to parse LZ4 header for {file_display_name}: {e}"))?;
        if header.content_size == 0 {
            return Err(format!(
                "The LZ4 package entry {file_display_name} reports an empty payload."
            ));
        }
        Some(header)
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

fn with_odin_session<T, F>(odin_manager: &mut OdinManager, operation: F) -> Result<T, String>
where
    F: FnOnce(&mut OdinManager) -> Result<T, String>,
{
    odin_manager.begin_session().map_err(|e| e.to_string())?;
    let operation_result =
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| operation(odin_manager)));
    let cleanup_result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        odin_manager.end_session().map_err(|e| e.to_string())
    }));

    match operation_result {
        Err(payload) => {
            // Preserve the original panic after making the best-effort cleanup
            // attempt. The outer worker boundary will turn it into a terminal UI
            // error and release the active-operation guard.
            std::panic::resume_unwind(payload)
        }
        Ok(Err(error)) => {
            let cleanup_message = match cleanup_result {
                Ok(Ok(())) => None,
                Ok(Err(cleanup_error)) => Some(cleanup_error),
                Err(payload) => Some(format!(
                    "session cleanup panicked: {}",
                    panic_message(payload)
                )),
            };
            if let Some(cleanup_error) = cleanup_message {
                Err(format!(
                    "{error} The device session also could not be closed cleanly: {cleanup_error}"
                ))
            } else {
                Err(error)
            }
        }
        Ok(Ok(value)) => match cleanup_result {
            Ok(Ok(())) => Ok(value),
            Ok(Err(error)) => Err(error),
            Err(payload) => std::panic::resume_unwind(payload),
        },
    }
}

fn run_flash(app: AppHandle, req: FlashRequest) -> Result<(), String> {
    let usb_backend = backend_from_str(&req.backend)?;
    let zip_source = req.zip.clone();
    // Computed once from the reviewed ZIP below and reused for source scanning,
    // so the reviewed plan (non_stored-empty, packages == req.packages) is the
    // exact one that gets flashed — no second scan, no drop window.
    let mut zip_plan_sources: Option<Vec<TarSource>> = None;
    let selected_packages = if let Some(zip_path) = zip_source.as_deref() {
        if req.folder.is_some() {
            return Err(
                "Choose either a firmware ZIP or an extracted folder, not both.".to_string(),
            );
        }
        let Some(reviewed) = req.reviewed_zip.as_ref() else {
            return Err("Review the firmware ZIP before flashing. Scan the ZIP again.".to_string());
        };
        verify_reviewed_zip(zip_path, reviewed)?;
        let plan = scan_zip_flash_plan(zip_path, &req.csc_mode)?;
        if !plan.non_stored.is_empty() {
            return Err(format!(
                "{} is compressed inside the ZIP archive and cannot be flashed in place. Extract the ZIP and use Firmware folder mode instead.",
                plan.non_stored.join(", ")
            ));
        }
        if plan.packages != req.packages {
            return Err(
                "The firmware ZIP contents changed after review. Scan the ZIP again and review the updated package list before flashing."
                    .to_string(),
            );
        }
        zip_plan_sources = Some(plan.sources);
        plan.packages
    } else if let Some(folder) = req.folder.as_deref() {
        if !req.packages.is_empty() {
            return Err(
                "Choose either a firmware folder or individual package slots, not both."
                    .to_string(),
            );
        }
        let reviewed = req.reviewed_packages.as_deref().ok_or_else(|| {
            "The firmware folder has not been reviewed. Scan it again before flashing.".to_string()
        })?;
        let selected = scan_package_folder_impl(folder, &req.csc_mode)?;
        verify_reviewed_folder(&selected, reviewed)?;
        selected
    } else {
        if req.reviewed_packages.is_some() {
            return Err(
                "Reviewed folder metadata was supplied without a firmware folder.".to_string(),
            );
        }
        req.packages.clone()
    };
    validate_package_selection(&selected_packages, &req.csc_mode, req.repartition)?;

    let mut packages = Vec::new();
    selected_packages.append_to(&mut packages);

    if packages.is_empty() {
        return Err("No firmware packages were selected.".to_string());
    }
    if !req.repartition && req.pit.is_some() {
        return Err(
            "A PIT file may only be selected when the explicit repartition option is enabled."
                .to_string(),
        );
    }

    let mut pit_file_bytes = None;
    if let Some(pit_path) = &req.pit {
        let mut f = File::open(pit_path)
            .map_err(|e| format!("Failed to open explicit PIT file \"{pit_path}\": {e}"))?;
        let mut buffer = Vec::new();
        f.read_to_end(&mut buffer)
            .map_err(|e| format!("Failed to read PIT file \"{pit_path}\": {e}"))?;
        if buffer.is_empty() {
            return Err(format!("The PIT file \"{pit_path}\" is empty."));
        }
        pit_file_bytes = Some(buffer);
    }

    let (resolved_entries, packaged_pit) = if let Some(sources) = zip_plan_sources {
        scan_tar_sources(&sources, req.skip_md5, &app)?
    } else {
        scan_tar_packages(&packages, req.skip_md5, &app)?
    };
    if resolved_entries.is_empty() {
        return Err(
            "The selected packages do not contain any non-empty flashable files.".to_string(),
        );
    }
    if pit_file_bytes.is_some() && packaged_pit.is_some() {
        return Err(
            "Both an explicit PIT and a packaged PIT were selected. Remove one to avoid an ambiguous repartition target."
                .to_string(),
        );
    }
    if pit_file_bytes.is_none() {
        pit_file_bytes = packaged_pit;
    }

    if req.repartition && pit_file_bytes.is_none() {
        return Err(
            "Repartition requires a PIT file in the packages or an explicit PIT path.".to_string(),
        );
    }
    let selected_pit = if req.repartition {
        let pit_bytes = pit_file_bytes
            .as_deref()
            .ok_or_else(|| "Repartition requires a valid PIT file.".to_string())?;
        let pit = PitData::new(pit_bytes)
            .map_err(|_| "The selected firmware contains an invalid PIT file.".to_string())?;
        validate_pit_layout(&pit, "selected")?;
        Some(pit)
    } else {
        None
    };

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
    validate_pit_layout(&pit_data, "device")?;

    if let Some(selected_pit) = selected_pit.as_ref() {
        validate_repartition_compatibility(&pit_data, selected_pit)?;
    }

    let mapping_pit = selected_pit.as_ref().unwrap_or(&pit_data);

    let mut partition_infos = Vec::new();
    let mut unmatched_entries = Vec::new();
    let mut mapped_targets: HashMap<(u32, u32, u32), String> = HashMap::new();
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
            let target = pit_target_key(pit_entry);
            if let Some(previous) = mapped_targets.get(&target) {
                if previous == &original_name {
                    continue;
                }
                return Err(format!(
                    "Package entries \"{previous}\" and \"{original_name}\" both target partition {}. Remove the duplicate or select one coherent firmware set.",
                    pit_entry.partition_name
                ));
            }
            mapped_targets.insert(target, original_name.clone());
            let payload = if let Some(payload) = first_mapping.take() {
                payload
            } else {
                unsafe {
                    MmapOptions::new()
                        .offset(source_offset)
                        .len(source_size)
                        .map(source_file.as_ref())
                }
                .map_err(|error| {
                    format!(
                        "Failed to create another mapping for package entry \"{original_name}\": {error}"
                    )
                })?
            };
            let size = payload.len() as u64;
            let info = create_firmware_info(
                payload,
                size,
                is_lz4,
                pit_entry,
                req.skip_size_check,
                &original_name,
            )?;
            partition_infos.push(info);
        }
    }

    if !unmatched_entries.is_empty() {
        unmatched_entries.sort();
        return Err(format!(
            "The following package entries do not exist in the {} PIT: {}. Flashing was stopped before any partition data was written.",
            if req.repartition {
                "selected"
            } else {
                "device"
            },
            unmatched_entries.join(", ")
        ));
    }

    if partition_infos.is_empty() {
        return Err("No package entries matched partitions in the device PIT.".to_string());
    }

    validate_firmware_plan(&partition_infos)?;

    // Upload only after the current device PIT, replacement PIT, package
    // mappings, duplicate targets, and partition sizes have all passed. From
    // this point onward the operation intentionally has no cancellation path.
    if req.repartition {
        emit_flash(
            &app,
            "pit",
            None,
            0.0,
            Some("Uploading validated PIT".to_string()),
        );
        let pit_bytes = pit_file_bytes
            .as_ref()
            .ok_or_else(|| "Repartition requires a valid PIT file.".to_string())?;
        odin_manager
            .send_pit_data(pit_bytes)
            .map_err(|e| e.to_string())?;
    }

    let total_bytes: u64 = partition_infos
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
    for info in partition_infos {
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

    Ok(())
}

enum DownloadFailure {
    Cancelled,
    Failed(String),
}

fn remove_partial(path: &Path) -> Result<(), String> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(format!(
            "Could not remove the incomplete download \"{}\": {error}",
            path.display()
        )),
    }
}

fn install_completed_download(
    part_path: &Path,
    output_path: &Path,
    overwrite: bool,
) -> Result<(), String> {
    if output_path.is_dir() {
        return Err(format!(
            "The download completed, but \"{}\" is now a folder and cannot be replaced.",
            output_path.display()
        ));
    }
    if path_exists_or_symlink(output_path) && !overwrite {
        return Err(format!(
            "\"{}\" was created while the download was running. Choose another name or allow overwrite.",
            output_path.display()
        ));
    }

    let initial_install = if overwrite {
        fs::rename(part_path, output_path)
    } else {
        move_file_no_replace(part_path, output_path)
    };
    match initial_install {
        Ok(()) => Ok(()),
        Err(first_error) if path_exists_or_symlink(output_path) && overwrite => {
            let mut backup_path = {
                let mut name = output_path.as_os_str().to_os_string();
                name.push(".samloader-backup");
                PathBuf::from(name)
            };
            for suffix in 1..=1000 {
                if !path_exists_or_symlink(&backup_path) {
                    break;
                }
                let mut name = output_path.as_os_str().to_os_string();
                name.push(format!(".samloader-backup-{suffix}"));
                backup_path = PathBuf::from(name);
            }
            if path_exists_or_symlink(&backup_path) {
                return Err(format!(
                    "The download completed, but a safe backup filename could not be reserved beside \"{}\".",
                    output_path.display()
                ));
            }

            fs::rename(output_path, &backup_path).map_err(|backup_error| {
                format!(
                    "The download completed, but the existing file \"{}\" could not be backed up for replacement: {backup_error}",
                    output_path.display()
                )
            })?;
            if let Err(rename_error) = fs::rename(part_path, output_path) {
                return match fs::rename(&backup_path, output_path) {
                    Ok(()) => Err(format!(
                        "The download completed, but its temporary file could not replace \"{}\": {rename_error} (initial replace error: {first_error}). The original file was restored.",
                        output_path.display()
                    )),
                    Err(restore_error) => Err(format!(
                        "The download completed, but replacement failed ({rename_error}) and the original could not be restored ({restore_error}). The original remains at \"{}\".",
                        backup_path.display()
                    )),
                };
            }
            let _ = fs::remove_file(backup_path);
            Ok(())
        }
        Err(error) => Err(format!(
            "The download completed, but its temporary file could not be moved to \"{}\": {error}",
            output_path.display()
        )),
    }
}

fn write_atomic_file(output_path: &Path, bytes: &[u8], overwrite: bool) -> Result<(), String> {
    let parent = output_path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    if !parent.is_dir() {
        return Err(format!(
            "The destination folder \"{}\" does not exist.",
            parent.display()
        ));
    }
    if output_path.is_dir() {
        return Err(format!(
            "The destination \"{}\" is a folder.",
            output_path.display()
        ));
    }
    if path_exists_or_symlink(output_path) && !overwrite {
        return Err(format!(
            "The destination \"{}\" already exists.",
            output_path.display()
        ));
    }

    let mut temporary = None;
    for suffix in 0..=1000_u32 {
        let mut name = output_path.as_os_str().to_os_string();
        name.push(format!(
            ".samloader-write-{}-{suffix}.tmp",
            std::process::id()
        ));
        let path = PathBuf::from(name);
        match File::options().write(true).create_new(true).open(&path) {
            Ok(file) => {
                temporary = Some((path, file));
                break;
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => {
                return Err(format!(
                    "Failed to reserve a temporary file beside \"{}\": {error}",
                    output_path.display()
                ));
            }
        }
    }
    let (temporary_path, mut file) = temporary.ok_or_else(|| {
        format!(
            "Could not reserve a safe temporary filename beside \"{}\".",
            output_path.display()
        )
    })?;

    let write_result = file.write_all(bytes).and_then(|()| file.sync_all());
    drop(file);
    if let Err(error) = write_result {
        let _ = fs::remove_file(&temporary_path);
        return Err(format!(
            "Failed to write the temporary PIT snapshot \"{}\": {error}",
            temporary_path.display()
        ));
    }

    let install_result = install_completed_download(&temporary_path, output_path, overwrite)
        .map_err(|error| format!("Failed to save the PIT snapshot safely: {error}"));
    if install_result.is_err() {
        let _ = fs::remove_file(&temporary_path);
    }
    install_result
}

fn run_download(
    app: AppHandle,
    req: DownloadRequest,
    control: Arc<DownloadControl>,
) -> Result<(), DownloadFailure> {
    let resolved = resolve_download(&req).map_err(DownloadFailure::Failed)?;
    let preflight = &resolved.preflight;
    control.set_identity(
        preflight.filename.clone(),
        preflight.path.clone(),
        preflight.size,
    );

    if control.cancelled.load(Ordering::Acquire) {
        return Err(DownloadFailure::Cancelled);
    }
    if !preflight.enough_space {
        return Err(DownloadFailure::Failed(format!(
            "Not enough free space to download {} bytes. Only {} bytes are available in the destination.",
            preflight.size, preflight.available_space
        )));
    }
    if preflight.part_conflict {
        return Err(DownloadFailure::Failed(format!(
            "The temporary file \"{}\" already exists. To avoid deleting another running process's data, confirm no download is using it, remove it manually, and try again.",
            preflight.part_path
        )));
    }
    if preflight.conflict && !req.overwrite {
        return Err(DownloadFailure::Failed(format!(
            "\"{}\" already exists. Choose another output name or confirm replacement.",
            preflight.path
        )));
    }
    if path_exists_or_symlink(&resolved.part_path) {
        return Err(DownloadFailure::Failed(
            "The temporary download file appeared during preflight. Another process may own it, so samloader left it untouched. Choose another name or retry after it is no longer in use."
                .to_string()
        ));
    }
    if path_exists_or_symlink(&resolved.output_path) && !req.overwrite {
        return Err(DownloadFailure::Failed(
            "The completed output file appeared during preflight. Choose another name or confirm replacement."
                .to_string(),
        ));
    }

    let output_parent = resolved
        .output_path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let available_now = available_space(output_parent).map_err(|error| {
        DownloadFailure::Failed(format!(
            "Could not recheck free space in \"{}\": {error}",
            output_parent.display()
        ))
    })?;
    if available_now < preflight.size {
        return Err(DownloadFailure::Failed(format!(
            "Not enough free space after preparing the destination. {} bytes are required, but only {available_now} bytes are free.",
            preflight.size
        )));
    }

    emit_download(
        &app,
        &control,
        "started",
        Some(format!(
            "Downloading and decrypting {}",
            preflight.source_filename
        )),
    );

    let progress = TauriDownloadProgress {
        app: app.clone(),
        control: Arc::clone(&control),
        total: preflight.size,
        filename: preflight.filename.clone(),
        output_path: preflight.path.clone(),
        started: Instant::now(),
        last_emit_ms: AtomicU64::new(0),
        verbose: req.verbose,
    };

    if let Err(error) = resolved
        .client
        .download(&resolved.part_path, req.threads, &progress)
    {
        if control.cancelled.load(Ordering::Acquire) {
            return Err(DownloadFailure::Cancelled);
        }
        return Err(DownloadFailure::Failed(format!(
            "The firmware download failed. Check your connection and try again: {error}"
        )));
    }

    if control.cancelled.load(Ordering::Acquire) {
        let _ = remove_partial(&resolved.part_path);
        return Err(DownloadFailure::Cancelled);
    }

    if let Err(error) =
        install_completed_download(&resolved.part_path, &resolved.output_path, req.overwrite)
    {
        return Err(DownloadFailure::Failed(format!(
            "{error} The completed decrypted file remains at \"{}\" so it is not lost.",
            resolved.part_path.display()
        )));
    }

    control.position.store(preflight.size, Ordering::Release);
    Ok(())
}

fn pit_entries_from_bytes(bytes: &[u8]) -> Result<Vec<PitEntryView>, String> {
    let pit_data = PitData::new(bytes).map_err(|_| "Failed to unpack PIT data.".to_string())?;
    validate_pit_layout(&pit_data, "device")?;
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
        default_download_dir: default_download_dir()
            .unwrap_or_else(|_| PathBuf::from("."))
            .to_string_lossy()
            .to_string(),
    }
}

#[tauri::command]
fn scan_firmware_folder(folder: String, csc_mode: String) -> Result<FolderScanResult, String> {
    let packages = scan_package_folder_impl(&folder, &csc_mode)?;
    validate_package_selection(&packages, &csc_mode, false)?;
    let identities = package_identities(&packages)?;
    Ok(FolderScanResult {
        packages,
        identities,
    })
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct ZipScanResult {
    packages: FolderPackages,
    identity: PackageIdentity,
    non_stored: Vec<String>,
}

#[tauri::command]
fn scan_firmware_zip(path: String, csc_mode: String) -> Result<ZipScanResult, String> {
    let plan = scan_zip_flash_plan(&path, &csc_mode)?;
    validate_package_selection(&plan.packages, &csc_mode, false)?;
    let identity = file_identity(&path)?;
    Ok(ZipScanResult {
        packages: plan.packages,
        identity,
        non_stored: plan.non_stored,
    })
}

async fn run_blocking<T, F>(task: F) -> Result<T, String>
where
    T: Send + 'static,
    F: FnOnce() -> Result<T, String> + Send + 'static,
{
    let result = tauri::async_runtime::spawn_blocking(move || {
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(task))
    })
    .await
    .map_err(|e| format!("The background operation could not be completed: {e}"))?;
    match result {
        Ok(result) => result,
        Err(payload) => Err(format!(
            "An unexpected internal error occurred: {}",
            panic_message(payload)
        )),
    }
}

async fn run_exclusive_blocking<T, F>(
    state: &GuiState,
    operation: ActiveOperation,
    task: F,
) -> Result<T, String>
where
    T: Send + 'static,
    F: FnOnce() -> Result<T, String> + Send + 'static,
{
    let guard = reserve_operation(&state.active_operation, operation)?;
    run_blocking(move || {
        // Moving the guard into the blocking task keeps the exclusion active
        // even if the async command future is dropped while USB I/O is running.
        let _guard = guard;
        task()
    })
    .await
}

#[tauri::command]
async fn check_updates(model: String, region: String) -> Result<UpdateInfo, String> {
    run_blocking(move || {
        let model = normalize_model(&model)?;
        let region = normalize_region(&region)?;
        let history = match FusClient::new() {
            Ok(client) => client
                .try_fetch_history(&model, &region)
                .map_err(|error| error.to_string()),
            Err(error) => Err(error.to_string()),
        };
        let info = match history {
            Ok(info) => info,
            Err(history_error) => try_fetch_version_xml(&model, &region).map_err(|version_error| {
                format!(
                    "No firmware history could be found for {model} / {region}. Verify the model and sales CSC. History request: {history_error}. Version request: {version_error}"
                )
            })?,
        };
        Ok(UpdateInfo {
            latest: info.latest,
            previous: info.previous,
            beta: info.beta,
        })
    })
    .await
}

#[tauri::command]
async fn detect_android_device(state: State<'_, GuiState>) -> Result<AndroidDeviceInfo, String> {
    run_exclusive_blocking(
        &state,
        ActiveOperation::Device("Android device detection"),
        move || Ok(detect_android_device_impl()),
    )
    .await
}

#[tauri::command]
async fn prepare_download(req: DownloadRequest) -> Result<DownloadPreflight, String> {
    run_blocking(move || resolve_download(&req).map(|resolved| resolved.preflight)).await
}

#[tauri::command]
async fn detect_download_device(
    state: State<'_, GuiState>,
    backend: String,
    wait: bool,
) -> Result<DeviceStatus, String> {
    let pit_snapshot = Arc::clone(&state.pit_snapshot);
    let result = run_exclusive_blocking(
        &state,
        ActiveOperation::Device("device detection"),
        move || {
            let backend = backend_from_str(&backend)?;
            let connected =
                detect_device_present_checked(backend, wait).map_err(|e| e.to_string())?;
            Ok(DeviceStatus {
                connected,
                label: if connected {
                    "Download mode device".to_string()
                } else {
                    "No download-mode device".to_string()
                },
            })
        },
    )
    .await;
    if !matches!(&result, Ok(status) if status.connected) {
        *pit_snapshot
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = None;
    }
    result
}

#[tauri::command]
async fn reboot_to_download(state: State<'_, GuiState>, backend: String) -> Result<(), String> {
    *state
        .pit_snapshot
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner()) = None;
    run_exclusive_blocking(
        &state,
        ActiveOperation::Device("a device reboot"),
        move || {
            let backend = backend_from_str(&backend)?;
            reboot_download(backend).map_err(|e| e.to_string())
        },
    )
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
    state: State<'_, GuiState>,
    backend: String,
    wait: bool,
    verbose: bool,
) -> Result<DevicePitRead, String> {
    let pit_snapshot = Arc::clone(&state.pit_snapshot);
    let next_pit_snapshot = Arc::clone(&state.next_pit_snapshot);
    *pit_snapshot
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner()) = None;
    run_exclusive_blocking(
        &state,
        ActiveOperation::Device("a device PIT read"),
        move || {
            let backend = backend_from_str(&backend)?;
            let usb = create_backend(backend, verbose, wait).map_err(|e| e.to_string())?;
            let mut odin_manager = OdinManager::new(usb, verbose);
            odin_manager.init().map_err(|e| e.to_string())?;
            let bytes = with_odin_session(&mut odin_manager, |manager| {
                manager.download_pit_file().map_err(|e| e.to_string())
            })?;
            let rows = pit_entries_from_bytes(&bytes)?;
            let id = next_pit_snapshot.fetch_add(1, Ordering::Relaxed) + 1;
            *pit_snapshot
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(PitSnapshot { id, bytes });
            Ok(DevicePitRead {
                snapshot_id: id.to_string(),
                rows,
            })
        },
    )
    .await
}

#[tauri::command]
async fn dump_device_pit(
    state: State<'_, GuiState>,
    snapshot_id: String,
    output: String,
) -> Result<(), String> {
    let expected_id = snapshot_id.parse::<u64>().map_err(|_| {
        "The PIT snapshot identifier is invalid. Read the device PIT again.".to_string()
    })?;
    let bytes = pit_snapshot_bytes(&state.pit_snapshot, expected_id)?;
    pit_entries_from_bytes(&bytes)?;
    run_blocking(move || write_atomic_file(Path::new(&output), &bytes, true)).await
}

fn pit_snapshot_bytes(
    snapshot: &Mutex<Option<PitSnapshot>>,
    expected_id: u64,
) -> Result<Vec<u8>, String> {
    let snapshot = snapshot
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let snapshot = snapshot.as_ref().ok_or_else(|| {
        "The PIT snapshot is no longer current. Read the device PIT again before saving it."
            .to_string()
    })?;
    if snapshot.id != expected_id {
        return Err(
            "The PIT snapshot changed. Review the current PIT again before saving it.".to_string(),
        );
    }
    Ok(snapshot.bytes.clone())
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
fn start_download(
    app: AppHandle,
    state: State<'_, GuiState>,
    req: DownloadRequest,
) -> Result<(), String> {
    let control = Arc::new(DownloadControl::new());
    let operation_guard = reserve_operation(
        &state.active_operation,
        ActiveOperation::Download(Arc::clone(&control)),
    )?;

    let spawn_result = thread::Builder::new()
        .name("firmware-download".to_string())
        .spawn(move || {
            let _operation_guard = operation_guard;
            log::info!("starting guarded firmware download operation");
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                run_download(app.clone(), req, Arc::clone(&control))
            }));
            let terminal = match result {
                Ok(Ok(())) => ("done", "Download and decryption complete.".to_string()),
                Ok(Err(DownloadFailure::Cancelled)) => (
                    "cancelled",
                    "Download cancelled. The incomplete file was removed.".to_string(),
                ),
                Ok(Err(DownloadFailure::Failed(error))) => ("error", error),
                Err(payload) => {
                    let (identity, _, _) = control.snapshot();
                    let staging_hint =
                        identity
                            .output_path
                            .map_or_else(String::new, |output_path| {
                                format!(
                                    " Review the staging path \"{}\" before retrying.",
                                    part_path_for(Path::new(&output_path)).display()
                                )
                            });
                    (
                        "error",
                        format!(
                            "An unexpected internal download error occurred: {}{}",
                            panic_message(payload),
                            staging_hint
                        ),
                    )
                }
            };

            match terminal.0 {
                "done" => log::info!("firmware download and integrity verification completed"),
                "cancelled" => log::warn!("firmware download was cancelled"),
                _ => log::error!("firmware download failed: {}", terminal.1),
            }

            emit_download(&app, &control, terminal.0, Some(terminal.1));
        });
    if let Err(error) = spawn_result {
        return Err(format!("Could not start the download worker: {error}"));
    }
    Ok(())
}

#[tauri::command]
fn pause_download(app: AppHandle, state: State<'_, GuiState>) -> Result<(), String> {
    let control = {
        let active = state
            .active_operation
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        match active.as_ref() {
            Some(ActiveOperation::Download(control)) => Arc::clone(control),
            Some(ActiveOperation::Flash) => {
                return Err(
                    "A flash operation is active; there is no download to pause.".to_string(),
                );
            }
            Some(ActiveOperation::Device(label)) => {
                return Err(format!("{label} is active; there is no download to pause."));
            }
            None => return Err("There is no active download to pause.".to_string()),
        }
    };
    if !control.pause() {
        return Err("The download is already paused or is being cancelled.".to_string());
    }
    emit_download(
        &app,
        &control,
        "paused",
        Some("Download paused.".to_string()),
    );
    Ok(())
}

#[tauri::command]
fn resume_download(app: AppHandle, state: State<'_, GuiState>) -> Result<(), String> {
    let control = {
        let active = state
            .active_operation
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        match active.as_ref() {
            Some(ActiveOperation::Download(control)) => Arc::clone(control),
            Some(ActiveOperation::Flash) => {
                return Err(
                    "A flash operation is active; there is no download to resume.".to_string(),
                );
            }
            Some(ActiveOperation::Device(label)) => {
                return Err(format!(
                    "{label} is active; there is no download to resume."
                ));
            }
            None => return Err("There is no active download to resume.".to_string()),
        }
    };
    if !control.resume() {
        return Err("The download is not paused.".to_string());
    }
    emit_download(
        &app,
        &control,
        "resumed",
        Some("Download resumed.".to_string()),
    );
    Ok(())
}

#[tauri::command]
fn cancel_download(app: AppHandle, state: State<'_, GuiState>) -> Result<(), String> {
    let control = {
        let active = state
            .active_operation
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        match active.as_ref() {
            Some(ActiveOperation::Download(control)) => Arc::clone(control),
            Some(ActiveOperation::Flash) => {
                return Err(
                    "A flash operation is active; there is no download to cancel.".to_string(),
                );
            }
            Some(ActiveOperation::Device(label)) => {
                return Err(format!(
                    "{label} is active; there is no download to cancel."
                ));
            }
            None => return Err("There is no active download to cancel.".to_string()),
        }
    };
    if !control.cancel() {
        return Err("Download cancellation was already requested.".to_string());
    }
    emit_download(
        &app,
        &control,
        "canceling",
        Some("Cancelling download…".to_string()),
    );
    Ok(())
}

#[tauri::command]
fn start_flash(
    app: AppHandle,
    state: State<'_, GuiState>,
    req: FlashRequest,
) -> Result<(), String> {
    let operation_guard = reserve_operation(&state.active_operation, ActiveOperation::Flash)?;

    let spawn_result = thread::Builder::new()
        .name("firmware-flash".to_string())
        .spawn(move || {
            let _operation_guard = operation_guard;
            log::info!("starting guarded firmware flash operation");
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                run_flash(app.clone(), req)
            }));
            let terminal = match result {
                Ok(Ok(())) => ("done", "Flash complete".to_string(), 100.0),
                Ok(Err(error)) => ("error", error, 0.0),
                Err(payload) => (
                    "error",
                    format!(
                        "An unexpected internal flash error occurred: {}",
                        panic_message(payload)
                    ),
                    0.0,
                ),
            };
            match terminal.0 {
                "done" => log::info!("firmware flash completed successfully"),
                _ => log::error!("firmware flash failed: {}", terminal.1),
            }
            emit_flash(&app, terminal.0, None, terminal.2, Some(terminal.1));
        });
    if let Err(error) = spawn_result {
        return Err(format!("Could not start the flash worker: {error}"));
    }
    Ok(())
}

fn main() {
    tauri::Builder::default()
        // This must be the first plugin so a second process cannot race an
        // active download file or USB session in the first process.
        .plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| {
            if let Some(window) = app.get_webview_window("main") {
                let _ = window.show();
                let _ = window.unminimize();
                let _ = window.set_focus();
            }
        }))
        .manage(GuiState::default())
        .plugin(
            tauri_plugin_log::Builder::new()
                .level(log::LevelFilter::Info)
                .max_file_size(1_000_000)
                .build(),
        )
        .plugin(tauri_plugin_dialog::init())
        .on_window_event(|window, event| {
            if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                let state = window.state::<GuiState>();
                let active_label = state
                    .active_operation
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .as_ref()
                    .and_then(|operation| match operation {
                        ActiveOperation::Download(_) | ActiveOperation::Flash => {
                            Some(operation.label())
                        }
                        ActiveOperation::Device(_) => None,
                    });
                if let Some(label) = active_label {
                    api.prevent_close();
                    let _ = window.emit(
                        "close-blocked",
                        format!(
                            "The app must remain open while {label} is active. Finish or safely stop the operation first."
                        ),
                    );
                }
            }
        })
        .invoke_handler(tauri::generate_handler![
            app_info,
            scan_firmware_folder,
            scan_firmware_zip,
            check_updates,
            detect_android_device,
            prepare_download,
            detect_download_device,
            reboot_to_download,
            read_pit_file,
            read_device_pit,
            dump_device_pit,
            verify_md5_files,
            start_download,
            pause_download,
            resume_download,
            cancel_download,
            start_flash
        ])
        .run(tauri::generate_context!())
        .expect("error while running samloader desktop UI");
}

#[cfg(test)]
mod tests {
    use super::*;

    fn adb_row(serial: &str, state: &str) -> AdbDeviceRow {
        AdbDeviceRow {
            serial: serial.to_string(),
            state: state.to_string(),
            model_hint: None,
        }
    }

    fn temp_test_dir(label: &str) -> PathBuf {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!(
            "samloader-gui-{label}-{}-{nonce}",
            std::process::id()
        ))
    }

    #[test]
    fn parses_adb_devices_and_ignores_daemon_noise() {
        let output = "* daemon started successfully\n\
                      List of devices attached\n\
                      R58M123456A device product:b0qxxx model:SM_S908B transport_id:1\n\
                      emulator-5554 offline transport_id:2\n";
        let rows = parse_adb_devices(output);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].serial, "R58M123456A");
        assert_eq!(rows[0].state, "device");
        assert_eq!(rows[0].model_hint.as_deref(), Some("SM_S908B"));
        assert_eq!(rows[1].state, "offline");
    }

    #[test]
    fn parses_android_properties_without_accepting_empty_values() {
        let properties = parse_android_properties(
            "[ro.product.model]: [SM-S931U1]\n\
             [ro.csc.sales_code]: [XAA]\n\
             [ro.boot.carrierid]: []\n",
        );
        assert_eq!(
            properties.get("ro.product.model").map(String::as_str),
            Some("SM-S931U1")
        );
        assert_eq!(
            properties.get("ro.csc.sales_code").map(String::as_str),
            Some("XAA")
        );
        assert!(!properties.contains_key("ro.boot.carrierid"));
    }

    #[test]
    fn selects_consumer_friendly_adb_statuses() {
        assert_eq!(device_status_from_rows(&[]).status, "not_found");
        assert_eq!(
            device_status_from_rows(&[adb_row("one", "device"), adb_row("two", "device")]).status,
            "multiple"
        );
        assert_eq!(
            device_status_from_rows(&[adb_row("one", "unauthorized")]).status,
            "unauthorized"
        );
        assert_eq!(
            device_status_from_rows(&[adb_row("one", "offline")]).status,
            "offline"
        );
        let connected = device_status_from_rows(&[adb_row("one", "device")]);
        assert_eq!(connected.status, "connected");
        assert!(connected.connected);
        assert_eq!(connected.serial.as_deref(), Some("one"));
    }

    #[test]
    fn normalizes_and_validates_model_and_region() {
        assert_eq!(normalize_model(" sm-s931u1 ").unwrap(), "SM-S931U1");
        assert_eq!(normalize_region(" xaa ").unwrap(), "XAA");
        assert!(normalize_model("SM S931U1").is_err());
        assert!(normalize_model("../SM-S931U1").is_err());
        assert!(normalize_region("X/AA").is_err());
        assert!(normalize_region("").is_err());
        assert_eq!(normalize_detected_region("eux").as_deref(), Some("EUX"));
        assert!(normalize_detected_region("OXM").is_none());
    }

    #[test]
    fn strips_only_the_encrypted_suffix_and_server_directories() {
        assert_eq!(
            decrypted_filename("folder/G998BXX.zip.enc4").unwrap(),
            "G998BXX.zip"
        );
        assert_eq!(
            decrypted_filename("folder\\G998BXX.zip.enc4").unwrap(),
            "G998BXX.zip"
        );
        assert_eq!(decrypted_filename("firmware.ENC4").unwrap(), "firmware");
        assert_eq!(decrypted_filename("firmware.zip").unwrap(), "firmware.zip");
    }

    #[test]
    fn custom_filename_cannot_escape_selected_output_folder() {
        let request = DownloadRequest {
            model: "SM-S931U1".to_string(),
            region: "XAA".to_string(),
            version: None,
            out_dir: Some("downloads".to_string()),
            out_file: Some("../outside.zip".to_string()),
            threads: 8,
            verbose: false,
            overwrite: false,
        };
        assert!(output_path_for_request(&request, "firmware.zip.enc4").is_err());
        assert_eq!(
            part_path_for(Path::new("firmware.zip")),
            PathBuf::from("firmware.zip.part")
        );
    }

    #[test]
    fn empty_output_folder_uses_the_consumer_download_directory() {
        let request = DownloadRequest {
            model: "SM-S931U1".to_string(),
            region: "XAA".to_string(),
            version: None,
            out_dir: None,
            out_file: None,
            threads: 8,
            verbose: false,
            overwrite: false,
        };
        let output = output_path_for_request(&request, "firmware.zip.enc4").unwrap();
        let downloads = default_download_dir().unwrap();
        assert_eq!(output.parent(), Some(downloads.as_path()));
        assert_eq!(
            output.file_name().and_then(|name| name.to_str()),
            Some("firmware.zip")
        );
    }

    #[cfg(windows)]
    #[test]
    fn rejects_reserved_windows_filenames() {
        assert!(validate_output_filename("NUL.zip").is_err());
        assert!(validate_output_filename("COM1").is_err());
        assert!(validate_output_filename("firmware.zip.").is_err());
    }

    #[test]
    fn download_request_defaults_preserve_consumer_settings() {
        let request: DownloadRequest = serde_json::from_value(serde_json::json!({
            "model": "SM-S931U1",
            "region": "XAA"
        }))
        .unwrap();
        assert_eq!(request.threads, 8);
        assert!(!request.verbose);
        assert!(!request.overwrite);
    }

    #[test]
    fn completed_download_replaces_existing_file_without_losing_new_data() {
        let directory = temp_test_dir("install-test");
        fs::create_dir(&directory).unwrap();
        let output = directory.join("firmware.zip");
        let partial = part_path_for(&output);
        fs::write(&output, b"old firmware").unwrap();
        fs::write(&partial, b"new firmware").unwrap();

        install_completed_download(&partial, &output, true).unwrap();

        assert_eq!(fs::read(&output).unwrap(), b"new firmware");
        assert!(!partial.exists());
        assert!(!directory.join("firmware.zip.samloader-backup").exists());
        fs::remove_file(output).unwrap();
        fs::remove_dir(directory).unwrap();
    }

    #[test]
    fn completed_download_does_not_replace_without_permission() {
        let directory = temp_test_dir("conflict-test");
        fs::create_dir(&directory).unwrap();
        let output = directory.join("firmware.zip");
        let partial = part_path_for(&output);
        fs::write(&output, b"keep me").unwrap();
        fs::write(&partial, b"new firmware").unwrap();

        assert!(install_completed_download(&partial, &output, false).is_err());
        assert_eq!(fs::read(&output).unwrap(), b"keep me");
        assert_eq!(fs::read(&partial).unwrap(), b"new firmware");
        fs::remove_file(output).unwrap();
        fs::remove_file(partial).unwrap();
        fs::remove_dir(directory).unwrap();
    }

    #[test]
    fn no_replace_primitive_preserves_a_racing_destination() {
        let directory = temp_test_dir("no-replace-primitive");
        fs::create_dir_all(&directory).unwrap();
        let source = directory.join("verified.part");
        let destination = directory.join("firmware.zip");
        fs::write(&source, b"new firmware").unwrap();
        fs::write(&destination, b"existing firmware").unwrap();

        assert!(move_file_no_replace(&source, &destination).is_err());
        assert_eq!(fs::read(&source).unwrap(), b"new firmware");
        assert_eq!(fs::read(&destination).unwrap(), b"existing firmware");
        fs::remove_dir_all(directory).unwrap();
    }

    fn coherent_packages() -> FolderPackages {
        FolderPackages {
            bl: Some("BL_S931BXXU1AYF1_release.tar.md5".to_string()),
            ap: Some("AP_S931BXXU1AYF1_release.tar.md5".to_string()),
            cp: Some("CP_S931BXXU1AYE9_release.tar.md5".to_string()),
            csc: Some("CSC_OXM_S931BOXM1AYF1_release.tar.md5".to_string()),
            userdata: None,
        }
    }

    #[test]
    fn validates_package_slots_builds_and_device_family() {
        let packages = coherent_packages();
        assert!(validate_package_selection(&packages, "wipe", false).is_ok());

        let mut mixed_build = packages.clone();
        mixed_build.ap = Some("AP_S931BXXU2ZZZ9_release.tar.md5".to_string());
        assert!(validate_package_selection(&mixed_build, "wipe", false).is_err());

        let mut mixed_model = packages.clone();
        mixed_model.cp = Some("CP_S938BXXU1AYE9_release.tar.md5".to_string());
        assert!(validate_package_selection(&mixed_model, "wipe", false).is_err());

        let mut wrong_slot = packages;
        wrong_slot.csc = Some("HOME_CSC_OXM_S931BOXM1AYF1_release.tar.md5".to_string());
        assert!(validate_package_selection(&wrong_slot, "wipe", false).is_err());
    }

    #[test]
    fn repartition_requires_complete_factory_reset_package_set() {
        let packages = coherent_packages();
        assert!(validate_package_selection(&packages, "wipe", true).is_ok());
        assert!(validate_package_selection(&packages, "home", true).is_err());

        let mut incomplete = packages;
        incomplete.cp = None;
        assert!(validate_package_selection(&incomplete, "wipe", true).is_err());
    }

    #[test]
    fn validates_pit_uniqueness_and_repartition_identity() {
        let bytes = include_bytes!("../../../test-data/Q7MQ_EUR_OPENX.pit");
        let current = PitData::new(bytes).unwrap();
        let candidate = PitData::new(bytes).unwrap();
        validate_pit_layout(&current, "device").unwrap();
        validate_repartition_compatibility(&current, &candidate).unwrap();
        assert!(is_allowed_zero_sized_payload_mapping(
            "UL_KEYS",
            "ul_key.bin"
        ));
        assert!(!is_allowed_zero_sized_payload_mapping("BOOT_A", "boot.img"));
        let duplicated_payload_targets = find_pit_entries_by_filename(&current, "dspso.bin");
        assert_eq!(duplicated_payload_targets.len(), 2);
        assert_eq!(
            duplicated_payload_targets[0]
                .partition_name
                .to_string_lossy(),
            "DSP_A"
        );
        assert_eq!(
            duplicated_payload_targets[1]
                .partition_name
                .to_string_lossy(),
            "DSP_B"
        );

        let mut duplicate_target = PitData::new(bytes).unwrap();
        let first_key = pit_target_key(&duplicate_target.entries[0]);
        let first_binary_type = duplicate_target.entries[0].binary_type;
        let first_device_type = duplicate_target.entries[0].device_type;
        duplicate_target.entries[1].binary_type = first_binary_type;
        duplicate_target.entries[1].device_type = first_device_type;
        duplicate_target.entries[1].identifier = first_key.2;
        validate_pit_layout(&duplicate_target, "selected").unwrap();

        let mut incompatible = PitData::new(bytes).unwrap();
        incompatible.lu_count = incompatible.lu_count.saturating_add(1);
        assert!(validate_repartition_compatibility(&current, &incompatible).is_err());

        let storage_entry_index = current
            .entries
            .iter()
            .position(|entry| {
                matches!(entry.device_type, DeviceType::MMC | DeviceType::UFS)
                    && entry.block_count > 0
                    && !is_pit_metadata_partition(&entry.partition_name.to_string_lossy())
            })
            .unwrap();

        let mut overlap = PitData::new(bytes).unwrap();
        let first_group = pit_layout_group(&overlap.entries[storage_entry_index]);
        let overlapping_index = overlap
            .entries
            .iter()
            .enumerate()
            .find(|(index, entry)| {
                *index != storage_entry_index
                    && pit_layout_group(entry) == first_group
                    && entry.block_count > 0
                    && !is_pit_metadata_partition(&entry.partition_name.to_string_lossy())
            })
            .map(|(index, _)| index)
            .unwrap();
        overlap.entries[overlapping_index].block_size_or_offset =
            overlap.entries[storage_entry_index].block_size_or_offset;
        validate_pit_layout(&overlap, "selected").unwrap();

        let mut overflowing = PitData::new(bytes).unwrap();
        overflowing.entries[storage_entry_index].block_size_or_offset = u32::MAX;
        overflowing.entries[storage_entry_index].block_count = 2;
        assert!(
            validate_pit_layout(&overflowing, "selected")
                .unwrap_err()
                .contains("overflows its 32-bit block address space")
        );

        let mut zero_sized = PitData::new(bytes).unwrap();
        zero_sized.entries[storage_entry_index].block_count = 0;
        assert!(
            validate_pit_layout(&zero_sized, "selected")
                .unwrap_err()
                .contains("zero-sized partition")
        );

        let reserved_index = current
            .entries
            .iter()
            .position(|entry| entry.flash_filename.to_string_lossy().trim().is_empty())
            .unwrap();
        let mut reserved_zero = PitData::new(bytes).unwrap();
        reserved_zero.entries[reserved_index].block_count = 0;
        validate_pit_layout(&reserved_zero, "device").unwrap();

        let mut invalid_unit = PitData::new(bytes).unwrap();
        invalid_unit.entries[storage_entry_index].file_offset = u32::from(invalid_unit.lu_count);
        assert!(
            validate_pit_layout(&invalid_unit, "selected")
                .unwrap_err()
                .contains("invalid UFS logical unit")
        );

        let mut changed_target = PitData::new(bytes).unwrap();
        changed_target.entries[storage_entry_index].identifier = u32::MAX;
        assert!(
            validate_repartition_compatibility(&current, &changed_target)
                .unwrap_err()
                .contains("changes the hardware target")
        );

        let mut sparse = PitData::new(bytes).unwrap();
        sparse.entries.truncate(8);
        assert!(validate_repartition_compatibility(&current, &sparse).is_err());

        let current_group_limit = current
            .entries
            .iter()
            .filter(|entry| pit_layout_group(entry) == first_group)
            .map(|entry| {
                entry
                    .block_size_or_offset
                    .checked_add(entry.block_count)
                    .unwrap()
            })
            .max()
            .unwrap();
        let mut beyond_storage = PitData::new(bytes).unwrap();
        beyond_storage.entries[storage_entry_index].block_size_or_offset = current_group_limit;
        beyond_storage.entries[storage_entry_index].block_count = 1;
        assert!(
            validate_repartition_compatibility(&current, &beyond_storage)
                .unwrap_err()
                .contains("known storage boundary")
        );
    }

    #[test]
    fn active_operation_guard_excludes_and_releases_operations() {
        let active = Arc::new(Mutex::new(None));
        let guard = reserve_operation(&active, ActiveOperation::Device("a PIT read")).unwrap();
        assert!(reserve_operation(&active, ActiveOperation::Flash).is_err());
        let unwind = std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
            let _guard = guard;
            panic!("simulated device worker panic");
        }));
        assert!(unwind.is_err());

        let next_guard = reserve_operation(&active, ActiveOperation::Flash).unwrap();
        drop(next_guard);
        assert!(
            active
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .is_none()
        );
    }

    #[test]
    fn empty_or_non_file_download_lists_fail_closed() {
        assert!(validate_download_list_entry(true, 0, "firmware.tar").is_err());
        assert!(validate_download_list_entry(false, 10, "firmware.tar").is_err());
        assert!(validate_download_list_entry(true, 10, "firmware.tar").is_ok());
    }

    #[test]
    fn reviewed_folder_rejects_mutated_or_added_packages() {
        let directory = temp_test_dir("folder-review");
        fs::create_dir_all(&directory).unwrap();
        let first = directory.join("BL_S931BXXU1TEST.tar");
        fs::write(&first, b"reviewed").unwrap();

        let folder = directory.to_string_lossy();
        let packages = scan_package_folder_impl(&folder, "home").unwrap();
        let reviewed = package_identities(&packages).unwrap();
        verify_reviewed_folder(&packages, &reviewed).unwrap();

        fs::write(&first, b"changed package bytes").unwrap();
        let rescanned = scan_package_folder_impl(&folder, "home").unwrap();
        assert!(verify_reviewed_folder(&rescanned, &reviewed).is_err());

        fs::write(directory.join("BL_S931BXXU1OTHER.tar"), b"other").unwrap();
        assert!(scan_package_folder_impl(&folder, "home").is_err());
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn pit_dump_uses_the_exact_reviewed_snapshot_and_writes_atomically() {
        let reviewed = include_bytes!("../../../test-data/Q7MQ_EUR_OPENX.pit").to_vec();
        let replacement_device_bytes = b"a different device PIT".to_vec();
        let snapshot = Mutex::new(Some(PitSnapshot {
            id: 7,
            bytes: reviewed.clone(),
        }));

        let bytes = pit_snapshot_bytes(&snapshot, 7).unwrap();
        assert_eq!(bytes, reviewed);
        assert_ne!(bytes, replacement_device_bytes);
        assert!(pit_snapshot_bytes(&snapshot, 8).is_err());
        assert!(pit_entries_from_bytes(&replacement_device_bytes).is_err());

        let directory = temp_test_dir("pit-snapshot");
        fs::create_dir_all(&directory).unwrap();
        let output = directory.join("backup.pit");
        fs::write(&output, b"existing backup").unwrap();
        assert!(write_atomic_file(&output, &bytes, false).is_err());
        assert_eq!(fs::read(&output).unwrap(), b"existing backup");
        write_atomic_file(&output, &bytes, true).unwrap();
        assert_eq!(fs::read(&output).unwrap(), reviewed);
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

    fn tar_bytes(entries: &[(&str, &[u8])]) -> Vec<u8> {
        let mut builder = tar::Builder::new(Vec::new());
        for (name, bytes) in entries {
            let mut header = tar::Header::new_gnu();
            header.set_size(bytes.len() as u64);
            header.set_mode(0o644);
            header.set_cksum();
            builder.append_data(&mut header, name, *bytes).unwrap();
        }
        builder.into_inner().unwrap()
    }

    fn tar_md5_bytes(member_name: &str, entries: &[(&str, &[u8])]) -> Vec<u8> {
        let mut bytes = tar_bytes(entries);
        let mut hasher = fast_md5::Md5::new();
        hasher.update(&bytes);
        let digest = hasher.finalize();
        let hex: String = digest.iter().map(|b| format!("{b:02x}")).collect();
        bytes.extend_from_slice(format!("{hex}  {member_name}\n").as_bytes());
        bytes
    }

    fn write_firmware_zip(path: &Path, members: &[(&str, &[u8], zip::CompressionMethod)]) {
        let file = File::create(path).unwrap();
        let mut writer = zip::ZipWriter::new(file);
        for (name, bytes, method) in members {
            let options = zip::write::SimpleFileOptions::default().compression_method(*method);
            writer.start_file(*name, options).unwrap();
            std::io::Write::write_all(&mut writer, bytes).unwrap();
        }
        writer.finish().unwrap();
    }

    #[test]
    fn zip_scan_reports_members_identity_and_compression() {
        let directory = temp_test_dir("gui-zip-scan");
        fs::create_dir_all(&directory).unwrap();
        let zip_path = directory.join("firmware.zip");
        let ap = tar_md5_bytes("AP_S931B_t.tar.md5", &[("persist.img", b"p")]);
        let csc = tar_md5_bytes("HOME_CSC_S931B_t.tar.md5", &[("cache.img", b"c")]);
        write_firmware_zip(
            &zip_path,
            &[
                ("AP_S931B_t.tar.md5", &ap, zip::CompressionMethod::Stored),
                (
                    "HOME_CSC_S931B_t.tar.md5",
                    &csc,
                    zip::CompressionMethod::Deflated,
                ),
            ],
        );
        let result =
            scan_firmware_zip(zip_path.to_string_lossy().to_string(), "home".to_string()).unwrap();
        assert_eq!(result.packages.ap.as_deref(), Some("AP_S931B_t.tar.md5"));
        assert_eq!(
            result.non_stored,
            vec!["HOME_CSC_S931B_t.tar.md5".to_string()]
        );
        assert_eq!(
            result.identity.size,
            std::fs::metadata(&zip_path).unwrap().len()
        );
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn zip_member_range_beyond_eof_fails_closed() {
        let directory = temp_test_dir("gui-zip-oob");
        fs::create_dir_all(&directory).unwrap();
        let zip_path = directory.join("firmware.zip");
        let ap = tar_md5_bytes("AP_S931B_t.tar.md5", &[("persist.img", b"p")]);
        write_firmware_zip(
            &zip_path,
            &[("AP_S931B_t.tar.md5", &ap, zip::CompressionMethod::Stored)],
        );

        // Inflate the member's declared size in the central directory so its byte
        // range runs past EOF. A plain end-truncation would instead destroy the
        // central directory, so the ZIP would never parse far enough to reach the
        // range check; this keeps the archive listable so the bounds check itself
        // is what must reject it rather than mmapping out of bounds later.
        let mut bytes = fs::read(&zip_path).unwrap();
        let cd = bytes
            .windows(4)
            .position(|w| w == [0x50, 0x4b, 0x01, 0x02])
            .expect("central directory header present");
        let huge = u32::MAX.to_le_bytes();
        bytes[cd + 20..cd + 24].copy_from_slice(&huge); // compressed size
        bytes[cd + 24..cd + 28].copy_from_slice(&huge); // uncompressed size
        fs::write(&zip_path, &bytes).unwrap();

        let Err(error) = scan_zip_flash_plan(zip_path.to_str().unwrap(), "home") else {
            panic!("expected the scan to fail closed on an out-of-bounds member range");
        };
        assert!(error.contains("beyond the end of the archive"));
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn reviewed_zip_identity_mismatch_fails_closed() {
        let directory = temp_test_dir("gui-zip-toctou");
        fs::create_dir_all(&directory).unwrap();
        let zip_path = directory.join("firmware.zip");
        let ap = tar_md5_bytes("AP_t.tar.md5", &[("persist.img", b"p")]);
        write_firmware_zip(
            &zip_path,
            &[("AP_t.tar.md5", &ap, zip::CompressionMethod::Stored)],
        );
        let zip_str = zip_path.to_string_lossy().to_string();
        let reviewed = file_identity(&zip_str).unwrap();

        // Mutate the file after review.
        let mut bytes = std::fs::read(&zip_path).unwrap();
        bytes.push(0);
        std::fs::write(&zip_path, bytes).unwrap();

        assert!(verify_reviewed_zip(&zip_str, &reviewed).is_err());
        std::fs::remove_dir_all(directory).unwrap();
    }
}
