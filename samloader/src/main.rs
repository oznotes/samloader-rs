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

#[cfg(not(any(feature = "rusb", feature = "nusb", feature = "serialport")))]
compile_error!("At least one USB backend must be enabled!");

mod actions;
mod download;
mod flash;

use clap::{Arg, ArgAction, Command};
use samloader_fus::{FusClient, fetch_version_xml};
use samloader_odin::UsbBackendOption;
use std::fs;
use std::path::{Path, PathBuf};

pub(crate) struct PartitionArg {
    pub(crate) name: Option<String>,
    pub(crate) filename: String,
}

#[macro_export]
macro_rules! print_error {
    ($($arg:tt)*) => {
        eprint!("ERROR: ");
        eprintln!($($arg)*);
    };
}

const ENABLED_BACKENDS: &[&str] = &[
    #[cfg(feature = "nusb")]
    UsbBackendOption::Nusb.to_str(),
    #[cfg(feature = "rusb")]
    UsbBackendOption::Libusb.to_str(),
    #[cfg(feature = "serialport")]
    UsbBackendOption::Vcom.to_str(),
    #[cfg(any(feature = "mock", debug_assertions))]
    UsbBackendOption::Mock.to_str(),
];

// On Windows, the default USB backend is set to VCOM rather than libusb/nusb.
// This is because libusb/nusb requires replacing the standard device driver
// with a WinUSB/generic driver (e.g., using a utility like Zadig).
// This manual driver swap breaks compatibility with the official
// Samsung USB drivers, creating a poor user experience.
// In contrast, the VCOM (Virtual COM Port/usbser.sys) implementation
// on Windows works out-of-the-box, requires no special driver
// modifications, and performs extremely fast.
cfg_if::cfg_if! {
    if #[cfg(all(target_os = "windows", feature = "serialport"))] {
        const DEFAULT_BACKEND: &str = UsbBackendOption::Vcom.to_str();
    } else {
        const DEFAULT_BACKEND: &str = ENABLED_BACKENDS.first().unwrap();
    }
}

// =============================================================================
// CLI String Constants
// =============================================================================

// --- Global Options ---
const VERBOSE_HELP: &str = "Enable verbose output";
const WAIT_HELP: &str = "Waits until a compatible device is connected";
const USB_BACKEND_HELP: &str = "The USB backend to use";

// --- Common Subcommand Options ---
const MODEL_HELP: &str = "The model name (e.g. SM-S931U1)";
const REGION_HELP: &str = "Region CSC code (e.g. XAA)";

// --- Download Command (`download`) ---
const VERSION_HELP: &str =
    "Firmware version string (e.g. PDA/CSC/MODEM). If omitted, downloads the latest";
const THREADS_HELP: &str = "Number of parallel connections";
const OUT_DIR_HELP: &str = "Output directory";
const OUT_FILE_HELP: &str = "Output file path";

// --- Check Update Command (`check-update`) ---
const ALL_HELP: &str =
    "List all available firmware versions, with previous and beta sorted from new to old";

// --- Detect Command (`detect`) ---
const DETECT_ABOUT: &str = "Indicates whether or not a download mode device can be detected";
const DETECT_HELP: &str = r#"Indicates whether or not a download mode device can be detected

Returns instantly per default, or waits until device is found
when --wait argument is used"#;

// --- Dump PIT Command (`dump-pit`) ---
const DUMP_PIT_ABOUT: &str = "Dumps the connected device's PIT file to the specified output file";
const DUMP_PIT_HELP: &str = r#"Dumps the connected device's PIT file to the specified
output file"#;
const DUMP_PIT_OUTPUT_HELP: &str = "Output file path for the dumped PIT file";

// --- Print PIT Command (`print-pit`) ---
const PRINT_PIT_ABOUT: &str = "Prints the contents of a PIT file in a human readable format";
const PRINT_PIT_HELP: &str = r#"Prints the contents of a PIT file in a human readable format. If
a filename is not provided then Heimdall retrieves the PIT file from the
connected device"#;
const PRINT_PIT_FILE_HELP: &str = "The PIT file to print. If not provided, Heimdall retrieves \
                                   the PIT file from the connected device";

// --- Flash Command (`flash`) ---
const FLASH_ABOUT: &str = "Flashes one or more firmware files to your phone";
const FLASH_HELP: &str = r#"Flashes one or more firmware files to your phone. Partition names
(or identifiers) can be obtained by executing the print-pit action.

Example explicit flashing: samloader flash -p RECOVERY recovery.img
Example auto-matching: samloader flash -f boot.img"#;

const NO_REBOOT_HELP: &str = "Disables automatic reboot after flashing";
const REPARTITION_HELP: &str = "Repartition the device. WARNING: It's strongly recommended \
                                you specify all files at your disposal";
const SKIP_SIZE_CHECK_HELP: &str = "Do not verify that files fit in the specified partition";
const PIT_HELP: &str = "The PIT file to use for repartitioning or flashing";
const SKIP_MD5_HELP: &str = "Skip MD5 checksum verification";
const FOLDER_HELP: &str = "Folder containing BL/AP/CP/CSC/USERDATA tar package files";
const CSC_MODE_HELP: &str = "CSC package to auto-select from --folder";
const BL_HELP: &str = "BL tar package file";
const AP_HELP: &str = "AP tar package file";
const CP_HELP: &str = "CP tar package file";
const CSC_HELP: &str = "CSC/HOME_CSC tar package file";
const USERDATA_HELP: &str = "USERDATA tar package file";
const PARTITION_HELP: &str = "Explicit partition name/identifier and file to flash";
const FILE_HELP: &str = "Automatic partition name matching file to flash";

// --- Verify MD5 Command (`verify-md5`) ---
const VERIFY_MD5_ABOUT: &str = "Verifies the MD5 checksum of one or more .tar.md5 files";
const VERIFY_MD5_FILE_HELP: &str = "The .tar.md5 files to verify";

trait FusOptionExt {
    fn fus_options(self) -> Self;
}

impl FusOptionExt for Command {
    fn fus_options(self) -> Self {
        self.arg(
            Arg::new("model")
                .short('m')
                .long("model")
                .required(true)
                .help(MODEL_HELP),
        )
        .arg(
            Arg::new("region")
                .short('r')
                .long("region")
                .required(true)
                .help(REGION_HELP),
        )
    }
}

trait OdinOptionExt {
    fn odin_options(self) -> Self;
}

impl OdinOptionExt for Command {
    fn odin_options(self) -> Self {
        self.arg(
            Arg::new("wait")
                .long("wait")
                .action(ArgAction::SetTrue)
                .help(WAIT_HELP),
        )
        .arg(
            Arg::new("no-reboot")
                .long("no-reboot")
                .action(ArgAction::SetTrue)
                .help(NO_REBOOT_HELP),
        )
    }
}

#[derive(Debug)]
struct FolderPackages {
    bl: Option<String>,
    ap: Option<String>,
    cp: Option<String>,
    csc: Option<String>,
    userdata: Option<String>,
}

impl FolderPackages {
    fn append_to(self, packages: &mut Vec<String>) {
        if let Some(bl) = self.bl {
            packages.push(bl);
        }
        if let Some(ap) = self.ap {
            packages.push(ap);
        }
        if let Some(cp) = self.cp {
            packages.push(cp);
        }
        if let Some(csc) = self.csc {
            packages.push(csc);
        }
        if let Some(userdata) = self.userdata {
            packages.push(userdata);
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

fn scan_package_folder(folder: &str, csc_mode: &str) -> Result<FolderPackages, String> {
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
                    "HOME_CSC package was not found. Use --csc-mode wipe to select CSC instead."
                        .to_string(),
                );
            }
            home_csc
        }
        "wipe" => {
            if csc.is_none() && home_csc.is_some() {
                return Err(
                    "CSC package was not found. Omit --csc-mode wipe to select HOME_CSC instead."
                        .to_string(),
                );
            }
            csc
        }
        _ => unreachable!("csc-mode is validated by clap"),
    };

    Ok(FolderPackages {
        bl: select_package("BL", bl)?,
        ap: select_package("AP", ap)?,
        cp: select_package("CP", cp)?,
        csc: selected_csc,
        userdata: select_package("USERDATA", userdata)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::File;
    use std::io::Write;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_firmware_dir(test_name: &str) -> PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "samloader-rs-{test_name}-{}-{nonce}",
            std::process::id()
        ));
        fs::create_dir(&dir).unwrap();
        dir
    }

    fn write_package(dir: &Path, name: &str) {
        let mut file = File::create(dir.join(name)).unwrap();
        file.write_all(b"test").unwrap();
    }

    #[test]
    fn folder_mode_defaults_to_home_csc() {
        let dir = temp_firmware_dir("home-csc");
        write_package(&dir, "AP_test.tar.md5");
        write_package(&dir, "CSC_test.tar.md5");
        write_package(&dir, "HOME_CSC_test.tar.md5");

        let selection = scan_package_folder(dir.to_str().unwrap(), "home").unwrap();

        assert!(selection.ap.unwrap().ends_with("AP_test.tar.md5"));
        assert!(selection.csc.unwrap().ends_with("HOME_CSC_test.tar.md5"));
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn folder_mode_wipe_selects_regular_csc() {
        let dir = temp_firmware_dir("wipe-csc");
        write_package(&dir, "CSC_test.tar.md5");
        write_package(&dir, "HOME_CSC_test.tar.md5");

        let selection = scan_package_folder(dir.to_str().unwrap(), "wipe").unwrap();

        assert!(selection.csc.unwrap().ends_with("CSC_test.tar.md5"));
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn folder_mode_home_rejects_regular_csc_fallback() {
        let dir = temp_firmware_dir("home-rejects-csc");
        write_package(&dir, "CSC_test.tar.md5");

        let err = scan_package_folder(dir.to_str().unwrap(), "home").unwrap_err();

        assert!(err.contains("--csc-mode wipe"));
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn folder_mode_prefers_md5_package_over_tar_package() {
        let dir = temp_firmware_dir("prefer-md5");
        write_package(&dir, "BL_test.tar");
        write_package(&dir, "BL_test.tar.md5");

        let selection = scan_package_folder(dir.to_str().unwrap(), "home").unwrap();

        assert!(selection.bl.unwrap().ends_with("BL_test.tar.md5"));
        fs::remove_dir_all(dir).unwrap();
    }
}

fn main() {
    #[allow(unused_mut)]
    let mut cmd = Command::new("samloader")
        .about("Download and flash firmware for Samsung devices")
        .version(env!("CARGO_PKG_VERSION"))
        .subcommand_required(true)
        .arg_required_else_help(true)
        .arg(
            Arg::new("verbose")
                .long("verbose")
                .action(ArgAction::SetTrue)
                .global(true)
                .help(VERBOSE_HELP),
        )
        .arg(
            Arg::new("usb_backend")
                .long("usb-backend")
                .global(true)
                .default_value(DEFAULT_BACKEND)
                .value_parser(ENABLED_BACKENDS.to_vec())
                .help(USB_BACKEND_HELP),
        )
        .subcommand(
            Command::new("download")
                .about("Download firmware")
                .fus_options()
                .arg(
                    Arg::new("version")
                        .short('v')
                        .long("version")
                        .help(VERSION_HELP),
                )
                .arg(
                    Arg::new("threads")
                        .short('j')
                        .long("threads")
                        .default_value("8")
                        .value_parser(clap::value_parser!(u64))
                        .help(THREADS_HELP),
                )
                .arg(
                    Arg::new("out_dir")
                        .short('d')
                        .long("out-dir")
                        .help(OUT_DIR_HELP),
                )
                .arg(
                    Arg::new("out_file")
                        .short('o')
                        .long("out-file")
                        .help(OUT_FILE_HELP),
                ),
        )
        .subcommand(
            Command::new("check-update")
                .about("Check available versions")
                .fus_options()
                .arg(
                    Arg::new("all")
                        .short('a')
                        .long("all")
                        .help(ALL_HELP)
                        .action(ArgAction::SetTrue),
                ),
        )
        .subcommand(
            Command::new("detect")
                .about(DETECT_ABOUT)
                .long_about(DETECT_HELP)
                .arg(
                    Arg::new("wait")
                        .long("wait")
                        .action(ArgAction::SetTrue)
                        .help(WAIT_HELP),
                ),
        )
        .subcommand(
            Command::new("dump-pit")
                .about(DUMP_PIT_ABOUT)
                .long_about(DUMP_PIT_HELP)
                .odin_options()
                .arg(
                    Arg::new("output")
                        .long("output")
                        .required(true)
                        .num_args(1)
                        .help(DUMP_PIT_OUTPUT_HELP),
                ),
        )
        .subcommand(
            Command::new("print-pit")
                .about(PRINT_PIT_ABOUT)
                .long_about(PRINT_PIT_HELP)
                .odin_options()
                .arg(
                    Arg::new("file")
                        .short('f')
                        .long("file")
                        .num_args(1)
                        .help(PRINT_PIT_FILE_HELP),
                ),
        )
        .subcommand(
            Command::new("flash")
                .about(FLASH_ABOUT)
                .long_about(FLASH_HELP)
                .odin_options()
                .arg(
                    Arg::new("repartition")
                        .long("repartition")
                        .action(ArgAction::SetTrue)
                        .help(REPARTITION_HELP),
                )
                .arg(
                    Arg::new("skip-size-check")
                        .long("skip-size-check")
                        .action(ArgAction::SetTrue)
                        .help(SKIP_SIZE_CHECK_HELP),
                )
                .arg(
                    Arg::new("skip-md5")
                        .long("skip-md5")
                        .action(ArgAction::SetTrue)
                        .help(SKIP_MD5_HELP),
                )
                .arg(Arg::new("pit").long("pit").num_args(1).help(PIT_HELP))
                .arg(
                    Arg::new("folder")
                        .long("folder")
                        .num_args(1)
                        .help(FOLDER_HELP),
                )
                .arg(
                    Arg::new("csc-mode")
                        .long("csc-mode")
                        .num_args(1)
                        .default_value("home")
                        .value_parser(["home", "wipe"])
                        .help(CSC_MODE_HELP),
                )
                .arg(
                    Arg::new("bl")
                        .short('b')
                        .long("BL")
                        .num_args(1)
                        .help(BL_HELP),
                )
                .arg(
                    Arg::new("ap")
                        .short('a')
                        .long("AP")
                        .num_args(1)
                        .help(AP_HELP),
                )
                .arg(
                    Arg::new("cp")
                        .short('c')
                        .long("CP")
                        .num_args(1)
                        .help(CP_HELP),
                )
                .arg(
                    Arg::new("csc")
                        .short('s')
                        .long("CSC")
                        .num_args(1)
                        .help(CSC_HELP),
                )
                .arg(
                    Arg::new("userdata")
                        .short('u')
                        .long("USERDATA")
                        .num_args(1)
                        .help(USERDATA_HELP),
                )
                .arg(
                    Arg::new("partition")
                        .short('p')
                        .long("partition")
                        .action(ArgAction::Append)
                        .num_args(2)
                        .value_names(["PARTITION", "FILE"])
                        .help(PARTITION_HELP),
                )
                .arg(
                    Arg::new("file")
                        .short('f')
                        .long("file")
                        .action(ArgAction::Append)
                        .num_args(1)
                        .value_name("FILE")
                        .help(FILE_HELP),
                ),
        )
        .subcommand(
            Command::new("verify-md5").about(VERIFY_MD5_ABOUT).arg(
                Arg::new("file")
                    .required(true)
                    .num_args(1..)
                    .help(VERIFY_MD5_FILE_HELP),
            ),
        )
        .subcommand(
            Command::new("reboot-download")
                .about("Boot a connected Samsung device into download mode"),
        );

    #[cfg(target_os = "linux")]
    {
        cmd = cmd.subcommand(
            Command::new("fix-usb")
                .about("Add udev rules to fix USB device access permissions on Linux"),
        );
    }

    let matches = cmd.get_matches();

    let verbose = matches.get_flag("verbose");
    let usb_backend_str = matches.get_one::<String>("usb_backend").unwrap().as_str();
    let usb_backend = UsbBackendOption::try_from(usb_backend_str).expect("Invalid USB backend");

    let result = match matches.subcommand() {
        Some(("download", sub_m)) => {
            let model = sub_m.get_one::<String>("model").cloned().unwrap();
            let region = sub_m.get_one::<String>("region").cloned().unwrap();
            let version = sub_m.get_one::<String>("version").cloned();
            let threads = *sub_m.get_one::<u64>("threads").unwrap();
            let out_dir = sub_m.get_one::<String>("out_dir").cloned();
            let out_file = sub_m.get_one::<String>("out_file").cloned();
            let args = download::DownloadArgs {
                model,
                region,
                version,
                threads,
                out_dir,
                out_file,
                verbose,
            };
            download::action_download(args);
            0
        }
        Some(("check-update", sub_m)) => {
            let model = sub_m.get_one::<String>("model").cloned().unwrap();
            let region = sub_m.get_one::<String>("region").cloned().unwrap();
            let show_all = sub_m.get_flag("all");

            let info = FusClient::new()
                .and_then(|client| client.fetch_history(&model, &region))
                .or_else(|_| fetch_version_xml(&model, &region))
                .expect("Failed to fetch version info");

            if show_all {
                println!("Latest Stable Version:");
                println!("{}", info.latest);

                if !info.previous.is_empty() {
                    println!();
                    println!("Previous Stable Versions (sorted from new to old):");
                    for v in &info.previous {
                        println!("{}", v);
                    }
                }

                if !info.beta.is_empty() {
                    println!();
                    println!("Beta Versions (sorted from new to old):");
                    for v in &info.beta {
                        println!("{}", v);
                    }
                }
            } else {
                println!("{}", info.latest);
            }
            0
        }
        Some(("detect", sub_matches)) => {
            actions::action_detect(usb_backend, sub_matches.get_flag("wait"))
        }
        Some(("dump-pit", sub_matches)) => actions::action_dump_pit(
            usb_backend,
            sub_matches.get_one::<String>("output").unwrap(),
            verbose,
            !sub_matches.get_flag("no-reboot"),
            sub_matches.get_flag("wait"),
        ),
        Some(("print-pit", sub_matches)) => actions::action_print_pit(
            usb_backend,
            sub_matches
                .get_one::<String>("file")
                .map(|s| s.as_str())
                .unwrap_or(""),
            verbose,
            !sub_matches.get_flag("no-reboot"),
            sub_matches.get_flag("wait"),
        ),
        Some(("flash", sub_matches)) => {
            let mut packages = Vec::new();
            if let Some(folder) = sub_matches.get_one::<String>("folder") {
                let csc_mode = sub_matches
                    .get_one::<String>("csc-mode")
                    .map(|s| s.as_str())
                    .unwrap_or("home");
                let folder_packages = match scan_package_folder(folder, csc_mode) {
                    Ok(packages) => packages,
                    Err(e) => {
                        print_error!("{}", e);
                        std::process::exit(1);
                    }
                };
                folder_packages.append_to(&mut packages);
            }

            if let Some(bl) = sub_matches.get_one::<String>("bl") {
                packages.push(bl.clone());
            }
            if let Some(ap) = sub_matches.get_one::<String>("ap") {
                packages.push(ap.clone());
            }
            if let Some(cp) = sub_matches.get_one::<String>("cp") {
                packages.push(cp.clone());
            }
            if let Some(csc) = sub_matches.get_one::<String>("csc") {
                packages.push(csc.clone());
            }
            if let Some(userdata) = sub_matches.get_one::<String>("userdata") {
                packages.push(userdata.clone());
            }

            let mut partitions = Vec::new();
            if let Some(args) = sub_matches.get_many::<String>("partition") {
                let args_vec: Vec<&String> = args.collect();
                let mut i = 0;
                while i < args_vec.len() {
                    if i + 1 < args_vec.len() {
                        partitions.push(PartitionArg {
                            name: Some(args_vec[i].to_uppercase()),
                            filename: args_vec[i + 1].clone(),
                        });
                        i += 2;
                    }
                }
            }
            if let Some(files) = sub_matches.get_many::<String>("file") {
                for file in files {
                    partitions.push(PartitionArg {
                        name: None,
                        filename: file.clone(),
                    });
                }
            }

            if packages.is_empty() && partitions.is_empty() {
                print_error!("No packages, files, or partitions specified for flashing.");
                std::process::exit(1);
            }

            let result = flash::action_flash(
                usb_backend,
                sub_matches.get_flag("repartition"),
                verbose,
                !sub_matches.get_flag("no-reboot"),
                sub_matches.get_flag("wait"),
                sub_matches.get_flag("skip-size-check"),
                sub_matches.get_flag("skip-md5"),
                sub_matches.get_one::<String>("pit").map(|s| s.as_str()),
                &packages,
                &partitions,
            );
            std::process::exit(result);
        }
        Some(("verify-md5", sub_matches)) => {
            let files: Vec<String> = sub_matches
                .get_many::<String>("file")
                .unwrap()
                .cloned()
                .collect();
            actions::action_verify_md5(&files)
        }
        Some(("reboot-download", _sub_matches)) => {
            actions::action_reboot_download(usb_backend, verbose)
        }
        #[cfg(target_os = "linux")]
        Some(("fix-usb", _sub_matches)) => actions::action_fix_usb(),
        _ => unreachable!(),
    };

    std::process::exit(result);
}
