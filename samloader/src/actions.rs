// Copyright 2026 John "topjohnwu" Wu
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

use crate::{flash::validate_pit_layout, print_error};
use samloader_odin::{
    OdinManager, UsbBackendOption, create_backend, detect_device, verify_md5_footer,
};
use samloader_pit::PitData;
use std::ffi::OsString;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

static DUMP_TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

struct OwnedSiblingTemp {
    file: Option<File>,
    path: Option<PathBuf>,
}

impl OwnedSiblingTemp {
    fn create(destination: &Path) -> Result<Self, String> {
        let filename = destination.file_name().ok_or_else(|| {
            format!(
                "Output path does not name a file: {}",
                destination.display()
            )
        })?;
        let parent = destination
            .parent()
            .filter(|path| !path.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();

        for _ in 0..128 {
            let counter = DUMP_TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
            let mut temp_name = OsString::from(".");
            temp_name.push(filename);
            temp_name.push(format!(
                ".samloader-dump-{}-{nonce}-{counter}.tmp",
                std::process::id()
            ));
            let path = parent.join(temp_name);
            match OpenOptions::new().write(true).create_new(true).open(&path) {
                Ok(file) => {
                    return Ok(Self {
                        file: Some(file),
                        path: Some(path),
                    });
                }
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) => {
                    return Err(format!(
                        "Failed to create a staging file beside {}: {error}",
                        destination.display()
                    ));
                }
            }
        }

        Err(format!(
            "Failed to reserve a unique staging file beside {}",
            destination.display()
        ))
    }

    fn install(mut self, destination: &Path, contents: &[u8]) -> Result<(), String> {
        let path = self
            .path
            .as_ref()
            .expect("owned staging path must exist while installing");
        let file = self
            .file
            .as_mut()
            .expect("owned staging file must exist while installing");
        file.write_all(contents).map_err(|error| {
            format!(
                "Failed to write PIT data to staging file {}: {error}",
                path.display()
            )
        })?;
        file.sync_all().map_err(|error| {
            format!(
                "Failed to flush PIT staging file {}: {error}",
                path.display()
            )
        })?;
        drop(self.file.take());

        #[cfg(windows)]
        crate::fsutil::move_file_no_replace(path, destination).map_err(|error| {
            if fs::symlink_metadata(destination).is_ok() {
                format!(
                    "Output appeared while reading the device: {} (it was not overwritten)",
                    destination.display()
                )
            } else {
                format!(
                    "Failed to atomically move {} to {}: {error}",
                    path.display(),
                    destination.display()
                )
            }
        })?;

        #[cfg(not(windows))]
        {
            fs::hard_link(path, destination).map_err(|error| {
                if error.kind() == std::io::ErrorKind::AlreadyExists {
                    format!(
                        "Output appeared while reading the device: {} (it was not overwritten)",
                        destination.display()
                    )
                } else {
                    format!(
                        "Failed to atomically install {} at {}: {error}. Ensure the destination filesystem supports hard links",
                        path.display(),
                        destination.display()
                    )
                }
            })?;

            if let Err(error) = fs::remove_file(path) {
                let rollback = fs::remove_file(destination);
                return Err(match rollback {
                    Ok(()) => format!(
                        "Failed to remove PIT staging name {}: {error}; the output was rolled back",
                        path.display()
                    ),
                    Err(rollback_error) => format!(
                        "PIT data was installed at {}, but cleanup of {} failed ({error}) and rollback also failed ({rollback_error})",
                        destination.display(),
                        path.display()
                    ),
                });
            }
        }
        self.path = None;
        Ok(())
    }
}

impl Drop for OwnedSiblingTemp {
    fn drop(&mut self) {
        drop(self.file.take());
        if let Some(path) = self.path.take() {
            let _ = fs::remove_file(path);
        }
    }
}

fn ensure_dump_destination_absent(destination: &Path) -> Result<(), String> {
    match fs::symlink_metadata(destination) {
        Ok(_) => Err(format!(
            "Output already exists and will not be overwritten: {}",
            destination.display()
        )),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(format!(
            "Failed to inspect output path {}: {error}",
            destination.display()
        )),
    }
}

fn dump_pit_to_output<F>(destination: &Path, fetch: F) -> Result<Vec<String>, String>
where
    F: FnOnce() -> Result<(Vec<u8>, Vec<String>), String>,
{
    ensure_dump_destination_absent(destination)?;

    let (pit_buffer, cleanup_warnings) = fetch()?;
    let pit_data = PitData::new(&pit_buffer)
        .map_err(|error| format!("Failed to unpack device PIT file: {error}"))?;
    validate_pit_layout(&pit_data, "device")?;

    OwnedSiblingTemp::create(destination)?.install(destination, &pit_buffer)?;
    Ok(cleanup_warnings)
}

fn download_pit_from_device(
    usb_backend: UsbBackendOption,
    verbose: bool,
    reboot_device: bool,
    wait: bool,
) -> Result<(Vec<u8>, Vec<String>), String> {
    let usb = create_backend(usb_backend, verbose, wait).map_err(|error| error.to_string())?;
    let mut odin_manager = OdinManager::new(usb, verbose);
    odin_manager.init().map_err(|error| error.to_string())?;
    odin_manager
        .begin_session()
        .map_err(|error| error.to_string())?;

    let pit_result = odin_manager
        .download_pit_file()
        .map_err(|error| error.to_string());
    let mut cleanup_errors = Vec::new();
    if let Err(error) = odin_manager.end_session() {
        cleanup_errors.push(error.to_string());
    }
    if reboot_device && let Err(error) = odin_manager.reboot_device() {
        cleanup_errors.push(error.to_string());
    }

    match pit_result {
        Ok(pit_buffer) => Ok((pit_buffer, cleanup_errors)),
        Err(read_error) if cleanup_errors.is_empty() => Err(read_error),
        Err(read_error) => Err(format!(
            "{read_error}. Device cleanup also failed: {}",
            cleanup_errors.join("; ")
        )),
    }
}

pub(crate) fn action_detect(usb_backend: UsbBackendOption, wait: bool) -> i32 {
    let detected = detect_device(usb_backend, wait);
    if detected {
        println!("Device detected");
        0
    } else {
        eprintln!("ERROR: Failed to detect compatible download-mode device.");
        1
    }
}

pub(crate) fn action_dump_pit(
    usb_backend: UsbBackendOption,
    output: &str,
    verbose: bool,
    reboot_device: bool,
    wait: bool,
) -> i32 {
    if output.is_empty() {
        print_error!("Output file was not specified.");
        return 1;
    }

    let destination = Path::new(output);
    match dump_pit_to_output(destination, || {
        download_pit_from_device(usb_backend, verbose, reboot_device, wait)
    }) {
        Ok(cleanup_warnings) if cleanup_warnings.is_empty() => 0,
        Ok(cleanup_warnings) => {
            print_error!(
                "PIT backup was saved, but device cleanup failed: {}",
                cleanup_warnings.join("; ")
            );
            1
        }
        Err(error) => {
            print_error!("{}", error);
            1
        }
    }
}

pub(crate) fn action_print_pit(
    usb_backend: UsbBackendOption,
    file: &str,
    verbose: bool,
    reboot_device: bool,
    wait: bool,
) -> i32 {
    if !file.is_empty() {
        let mut f = match File::open(file) {
            Ok(f) => f,
            Err(_) => {
                print_error!("Failed to open file \"{}\"", file);
                return 1;
            }
        };

        let mut buffer = Vec::new();
        if f.read_to_end(&mut buffer).is_err() {
            print_error!("Failed to read file \"{}\"", file);
            return 1;
        }

        match PitData::new(&buffer) {
            Ok(pit_data) => {
                println!("{}", pit_data);
                0
            }
            Err(_) => {
                print_error!("Failed to unpack PIT file!");
                1
            }
        }
    } else {
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

        let mut success = true;
        let mut device_pit_data = None;

        match odin_manager.download_pit_file() {
            Ok(device_pit) => match PitData::new(&device_pit) {
                Ok(pit_data) => {
                    device_pit_data = Some(pit_data);
                }
                Err(_) => {
                    print_error!("Failed to unpack device's PIT file!");
                    success = false;
                }
            },
            Err(e) => {
                print_error!("{}", e);
                success = false;
            }
        }

        if let Some(pit_data) = device_pit_data {
            println!("{}", pit_data);
        }

        if let Err(e) = odin_manager.end_session() {
            print_error!("{}", e);
            success = false;
        }

        if reboot_device && let Err(e) = odin_manager.reboot_device() {
            print_error!("{}", e);
            success = false;
        }

        if success { 0 } else { 1 }
    }
}

pub(crate) fn action_reboot_download(usb_backend: UsbBackendOption, _verbose: bool) -> i32 {
    match samloader_odin::reboot_download(usb_backend) {
        Ok(()) => 0,
        Err(e) => {
            print_error!("{}", e);
            1
        }
    }
}

pub(crate) fn action_verify_md5(files: &[String]) -> i32 {
    let mut success = true;
    for file_path in files {
        println!("Verifying MD5 checksum for {}...", file_path);
        let file = match File::open(file_path) {
            Ok(f) => f,
            Err(e) => {
                print_error!("Failed to open file \"{}\": {}", file_path, e);
                success = false;
                continue;
            }
        };
        match verify_md5_footer(&file) {
            Ok(()) => {
                println!("MD5 verification successful!\n");
            }
            Err(e) => {
                print_error!("MD5 verification failed for \"{}\": {}", file_path, e);
                success = false;
            }
        }
    }
    if success { 0 } else { 1 }
}

#[cfg(target_os = "linux")]
const UDEV_RULES_PATH: &str = "/etc/udev/rules.d/60-samloader.rules";

#[cfg(target_os = "linux")]
pub(crate) fn action_fix_usb() -> i32 {
    if unsafe { libc::geteuid() != 0 } {
        eprintln!("ERROR: This command must be run as root (e.g., using sudo).");
        return 1;
    }

    println!("Writing udev rule to {}...", UDEV_RULES_PATH);
    let rule = "SUBSYSTEM==\"usb\", ATTR{idVendor}==\"04e8\", TAG+=\"uaccess\"\n";
    if let Err(e) = std::fs::write(UDEV_RULES_PATH, rule) {
        eprintln!("ERROR: Failed to write udev rules file: {}", e);
        return 1;
    }

    println!("Reloading and triggering udev rules...");
    let status = std::process::Command::new("udevadm")
        .arg("control")
        .arg("--reload-rules")
        .status();

    match status {
        Ok(s) if s.success() => {}
        _ => {
            eprintln!("ERROR: Failed to reload udev rules via udevadm.");
            return 1;
        }
    }

    let status = std::process::Command::new("udevadm")
        .arg("trigger")
        .status();

    match status {
        Ok(s) if s.success() => {}
        _ => {
            eprintln!("ERROR: Failed to trigger udev rules via udevadm.");
            return 1;
        }
    }

    println!("SUCCESS: USB udev rules successfully configured and reloaded.");
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_directory(test_name: &str) -> PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let directory = std::env::temp_dir().join(format!(
            "samloader-dump-pit-{test_name}-{}-{nonce}",
            std::process::id()
        ));
        fs::create_dir(&directory).unwrap();
        directory
    }

    fn assert_no_staging_files(directory: &Path) {
        let staging_files: Vec<_> = fs::read_dir(directory)
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .filter(|name| name.to_string_lossy().contains(".samloader-dump-"))
            .collect();
        assert!(
            staging_files.is_empty(),
            "orphaned PIT staging files: {staging_files:?}"
        );
    }

    #[test]
    fn device_read_failure_preserves_output_that_appears() {
        let directory = temp_directory("read-failure");
        let output = directory.join("device.pit");

        let result = dump_pit_to_output(&output, || {
            fs::write(&output, b"existing backup").unwrap();
            Err("simulated device read failure".to_string())
        });

        assert!(result.unwrap_err().contains("device read failure"));
        assert_eq!(fs::read(&output).unwrap(), b"existing backup");
        assert_no_staging_files(&directory);
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn invalid_pit_layout_is_never_installed() {
        let directory = temp_directory("invalid-layout");
        let output = directory.join("device.pit");
        let mut invalid_pit = include_bytes!("../../test-data/Q7MQ_EUR_OPENX.pit").to_vec();
        invalid_pit[4..8].copy_from_slice(&0_u32.to_le_bytes());

        let result = dump_pit_to_output(&output, || Ok((invalid_pit, Vec::new())));

        assert!(result.unwrap_err().contains("no partition entries"));
        assert!(!output.exists());
        assert_no_staging_files(&directory);
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn valid_pit_is_installed_without_overwriting_a_racing_output() {
        let directory = temp_directory("racing-output");
        let output = directory.join("device.pit");
        let valid_pit = include_bytes!("../../test-data/Q7MQ_EUR_OPENX.pit").to_vec();

        let result = dump_pit_to_output(&output, || {
            fs::write(&output, b"existing backup").unwrap();
            Ok((valid_pit, Vec::new()))
        });

        assert!(result.unwrap_err().contains("not overwritten"));
        assert_eq!(fs::read(&output).unwrap(), b"existing backup");
        assert_no_staging_files(&directory);
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn valid_pit_is_atomically_installed() {
        let directory = temp_directory("success");
        let output = directory.join("device.pit");
        let valid_pit = include_bytes!("../../test-data/Q7MQ_EUR_OPENX.pit").to_vec();

        dump_pit_to_output(&output, || Ok((valid_pit.clone(), Vec::new()))).unwrap();

        assert_eq!(fs::read(&output).unwrap(), valid_pit);
        assert_no_staging_files(&directory);
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn valid_pit_is_saved_even_when_device_cleanup_reports_a_warning() {
        let directory = temp_directory("cleanup-warning");
        let output = directory.join("device.pit");
        let valid_pit = include_bytes!("../../test-data/Q7MQ_EUR_OPENX.pit").to_vec();

        let warnings = dump_pit_to_output(&output, || {
            Ok((
                valid_pit.clone(),
                vec!["simulated reboot failure".to_string()],
            ))
        })
        .unwrap();

        assert_eq!(warnings, ["simulated reboot failure"]);
        assert_eq!(fs::read(&output).unwrap(), valid_pit);
        assert_no_staging_files(&directory);
        fs::remove_dir_all(directory).unwrap();
    }
}
