#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use fs2::available_space;
use memmap2::{Mmap, MmapOptions};
use samloader_fus::{DownloadProgress, FusClient, try_fetch_version_xml};
use samloader_odin::{
    FirmwareFile, FirmwareInfo, FirmwareLz4File, Lz4FrameHeader, OdinManager, UsbBackendOption,
    create_backend, detect_device, reboot_download, verify_md5_footer,
};
use samloader_pit::{PitData, PitEntry};
use serde::{Deserialize, Serialize};
use std::any::Any;
use std::collections::{HashMap, HashSet};
use std::fs::{self, File};
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant};
use tar::Archive;
use tauri::{AppHandle, Emitter, State};

#[cfg(windows)]
use std::os::windows::process::CommandExt;

#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

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
}

#[derive(Default)]
struct GuiState {
    active_operation: Arc<Mutex<Option<ActiveOperation>>>,
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
            if !entry.header().entry_type().is_file() || entry.size() == 0 {
                continue;
            }
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
        if buffer.is_empty() {
            return Err(format!("The PIT file \"{pit_path}\" is empty."));
        }
        pit_file_bytes = Some(buffer);
    }

    let (resolved_entries, local_pit) = scan_tar_packages(&packages, req.skip_md5, &app)?;
    if resolved_entries.is_empty() {
        return Err(
            "The selected packages do not contain any non-empty flashable files.".to_string(),
        );
    }
    if pit_file_bytes.is_none() {
        pit_file_bytes = local_pit;
    }

    if req.repartition && pit_file_bytes.is_none() {
        return Err(
            "Repartition requires a PIT file in the packages or an explicit PIT path.".to_string(),
        );
    }
    if req.repartition {
        let Some(pit_bytes) = pit_file_bytes.as_deref() else {
            return Err("Repartition requires a valid PIT file.".to_string());
        };
        PitData::new(pit_bytes)
            .map_err(|_| "The selected firmware contains an invalid PIT file.".to_string())?;
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
        let pit_bytes = pit_file_bytes
            .as_ref()
            .ok_or_else(|| "Repartition requires a valid PIT file.".to_string())?;
        odin_manager
            .send_pit_data(pit_bytes)
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

    match fs::rename(part_path, output_path) {
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
    if (preflight.conflict || preflight.part_conflict) && !req.overwrite {
        let conflict_path = if preflight.conflict {
            &preflight.path
        } else {
            &preflight.part_path
        };
        return Err(DownloadFailure::Failed(format!(
            "\"{conflict_path}\" already exists. Choose another output name or confirm overwrite."
        )));
    }
    if req.overwrite {
        remove_partial(&resolved.part_path).map_err(DownloadFailure::Failed)?;
    } else if path_exists_or_symlink(&resolved.output_path)
        || path_exists_or_symlink(&resolved.part_path)
    {
        return Err(DownloadFailure::Failed(
            "The output or temporary file was created during preflight. Choose another name or confirm overwrite."
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
        let cleanup_error = remove_partial(&resolved.part_path).err();
        if control.cancelled.load(Ordering::Acquire) {
            return Err(DownloadFailure::Cancelled);
        }
        let mut message =
            format!("The firmware download failed. Check your connection and try again: {error}");
        if let Some(cleanup_error) = cleanup_error {
            message.push_str(&format!(" {cleanup_error}"));
        }
        return Err(DownloadFailure::Failed(message));
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
fn scan_firmware_folder(folder: String, csc_mode: String) -> Result<FolderPackages, String> {
    scan_package_folder_impl(&folder, &csc_mode)
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

#[tauri::command]
async fn check_updates(model: String, region: String) -> Result<UpdateInfo, String> {
    run_blocking(move || {
        let model = normalize_model(&model)?;
        let region = normalize_region(&region)?;
        let info = FusClient::new()
            .map_err(|e| format!("Could not connect to Samsung's firmware service: {e}"))?
            .try_fetch_history(&model, &region)
            .or_else(|_| try_fetch_version_xml(&model, &region))
            .map_err(|e| {
                format!(
                    "No firmware history could be found for {model} / {region}. Verify the model and sales CSC: {e}"
                )
            })?;
        Ok(UpdateInfo {
            latest: info.latest,
            previous: info.previous,
            beta: info.beta,
        })
    })
    .await
}

#[tauri::command]
async fn detect_android_device() -> Result<AndroidDeviceInfo, String> {
    run_blocking(move || Ok(detect_android_device_impl())).await
}

#[tauri::command]
async fn prepare_download(req: DownloadRequest) -> Result<DownloadPreflight, String> {
    run_blocking(move || resolve_download(&req).map(|resolved| resolved.preflight)).await
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
fn start_download(
    app: AppHandle,
    state: State<'_, GuiState>,
    req: DownloadRequest,
) -> Result<(), String> {
    let control = Arc::new(DownloadControl::new());
    let active_operation = Arc::clone(&state.active_operation);
    {
        let mut active = active_operation
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(operation) = active.as_ref() {
            let operation_name = match operation {
                ActiveOperation::Download(_) => "a firmware download",
                ActiveOperation::Flash => "a flash operation",
            };
            return Err(format!(
                "Cannot start another download while {operation_name} is active."
            ));
        }
        *active = Some(ActiveOperation::Download(Arc::clone(&control)));
    }

    let worker_active_operation = Arc::clone(&active_operation);
    let spawn_result = thread::Builder::new()
        .name("firmware-download".to_string())
        .spawn(move || {
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
                    if let Some(output_path) = identity.output_path {
                        let _ = remove_partial(&part_path_for(Path::new(&output_path)));
                    }
                    (
                        "error",
                        format!(
                            "An unexpected internal download error occurred: {}",
                            panic_message(payload)
                        ),
                    )
                }
            };

            {
                let mut active = worker_active_operation
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                *active = None;
            }
            emit_download(&app, &control, terminal.0, Some(terminal.1));
        });
    if let Err(error) = spawn_result {
        let mut active = active_operation
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        *active = None;
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
    let active_operation = Arc::clone(&state.active_operation);
    {
        let mut active = active_operation
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(operation) = active.as_ref() {
            let operation_name = match operation {
                ActiveOperation::Download(_) => "a firmware download",
                ActiveOperation::Flash => "another flash operation",
            };
            return Err(format!(
                "Cannot start flashing while {operation_name} is active."
            ));
        }
        *active = Some(ActiveOperation::Flash);
    }

    let worker_active_operation = Arc::clone(&active_operation);
    let spawn_result = thread::Builder::new()
        .name("firmware-flash".to_string())
        .spawn(move || {
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
            {
                let mut active = worker_active_operation
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                *active = None;
            }
            emit_flash(&app, terminal.0, None, terminal.2, Some(terminal.1));
        });
    if let Err(error) = spawn_result {
        let mut active = active_operation
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        *active = None;
        return Err(format!("Could not start the flash worker: {error}"));
    }
    Ok(())
}

fn main() {
    tauri::Builder::default()
        .manage(GuiState::default())
        .plugin(tauri_plugin_dialog::init())
        .invoke_handler(tauri::generate_handler![
            app_info,
            scan_firmware_folder,
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
}
