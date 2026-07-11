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

use indicatif::{ProgressBar, ProgressStyle};
use samloader_fus::{DownloadProgress, FusClient, try_fetch_version_xml};
use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

const PROGRESS_TEMPLATE: &str =
    "[{elapsed_precise}] [{bar:40}] {bytes}/{total_bytes} ({bytes_per_sec}) [{eta_precise}]";

pub(crate) struct DownloadArgs {
    /// The model name (e.g. SM-S931U1)
    pub(crate) model: String,

    /// Region CSC code (e.g. XAA)
    pub(crate) region: String,

    /// Optional: firmware version. If None, downloads the latest.
    pub(crate) version: Option<String>,

    /// Number of parallel connections
    pub(crate) threads: u64,

    /// Optional: the output directory
    pub(crate) out_dir: Option<String>,

    /// Optional: the output file name
    pub(crate) out_file: Option<String>,

    /// Whether to enable verbose output
    pub(crate) verbose: bool,
}

struct ProgressWrapper<'a> {
    progress_bar: &'a ProgressBar,
    verbose: bool,
}

impl DownloadProgress for ProgressWrapper<'_> {
    fn inc(&self, bytes: u64) {
        self.progress_bar.inc(bytes);
    }

    fn position(&self) -> u64 {
        self.progress_bar.position()
    }

    fn println(&self, msg: String) {
        self.progress_bar.println(msg);
    }

    fn println_verbose(&self, msg: String) {
        if self.verbose {
            self.progress_bar.println(msg);
        }
    }
}

pub(crate) fn action_download(args: DownloadArgs) -> i32 {
    let mut client = match FusClient::new() {
        Ok(client) => client,
        Err(error) => {
            eprintln!("ERROR: Could not connect to Samsung FUS: {error}");
            return 1;
        }
    };

    let version = match &args.version {
        Some(v) => v.clone(),
        None => match client.try_fetch_history(&args.model, &args.region) {
            Ok(info) => info.latest,
            Err(history_error) => match try_fetch_version_xml(&args.model, &args.region) {
                Ok(info) => info.latest,
                Err(version_error) => {
                    eprintln!(
                        "ERROR: Could not resolve firmware for model {} and region {}. History request: {history_error}. Version request: {version_error}",
                        args.model, args.region
                    );
                    return 1;
                }
            },
        },
    };

    if let Err(error) = client.try_fetch_binary_info(&args.model, &args.region, &version) {
        eprintln!(
            "ERROR: Firmware metadata is unavailable for model {}, region {}, version {}: {error}",
            args.model, args.region, version
        );
        return 1;
    }

    println!("Firmware Version: {}", client.info.version);

    let default_name = match safe_default_filename(&client.info.filename) {
        Ok(name) => name,
        Err(error) => {
            eprintln!("ERROR: {error}");
            return 1;
        }
    };

    let final_out = match (args.out_file, args.out_dir) {
        (Some(_), Some(_)) => {
            eprintln!("ERROR: Choose either --out-file or --out-dir, not both.");
            return 1;
        }
        (Some(name), None) => PathBuf::from(name),
        (None, Some(dir)) => PathBuf::from(dir).join(default_name),
        _ => PathBuf::from(default_name),
    };
    let partial_out = partial_path(&final_out);

    if path_exists_or_symlink(&final_out) {
        eprintln!(
            "ERROR: Output already exists: {}. Choose another --out-file; existing firmware is never overwritten.",
            final_out.display()
        );
        return 1;
    }

    println!(
        "Downloading {} to {}",
        client.info.filename,
        final_out.display()
    );

    let style = ProgressStyle::with_template(PROGRESS_TEMPLATE)
        .unwrap_or_else(|_| ProgressStyle::default_bar());
    let progress = ProgressBar::new(client.info.size).with_style(style);
    progress.enable_steady_tick(Duration::from_secs(1));

    let wrapper = ProgressWrapper {
        progress_bar: &progress,
        verbose: args.verbose,
    };

    if let Err(e) = client.download(&partial_out, args.threads, &wrapper) {
        progress.abandon();
        eprintln!("\nERROR: Download failed: {e}");
        if path_exists_or_symlink(&partial_out) {
            eprintln!(
                "The partial path is still present at {}. It may belong to another running download; it was not removed.",
                partial_out.display()
            );
        }
        return 1;
    }

    if let Err(error) = install_completed_download(&partial_out, &final_out) {
        progress.abandon();
        eprintln!("\nERROR: Download completed but could not be installed: {error}");
        eprintln!(
            "The verified firmware remains at {}.",
            partial_out.display()
        );
        return 1;
    }
    progress.finish();
    println!("Verified firmware saved to {}", final_out.display());
    0
}

fn safe_default_filename(server_name: &str) -> Result<&str, String> {
    let decrypted = if server_name.to_ascii_lowercase().ends_with(".enc4") {
        &server_name[..server_name.len() - ".enc4".len()]
    } else {
        server_name
    };
    if decrypted.is_empty()
        || decrypted == "."
        || decrypted == ".."
        || decrypted.contains(['/', '\\'])
        || Path::new(decrypted)
            .file_name()
            .and_then(|name| name.to_str())
            != Some(decrypted)
    {
        return Err(format!(
            "Samsung returned an unsafe firmware filename: {server_name:?}"
        ));
    }
    Ok(decrypted)
}

fn partial_path(final_path: &Path) -> PathBuf {
    let mut value: OsString = final_path.as_os_str().to_os_string();
    value.push(".part");
    PathBuf::from(value)
}

fn path_exists_or_symlink(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok()
}

/// Installs a completed sibling staging file without replacing an existing
/// destination.
fn install_completed_download(partial: &Path, final_path: &Path) -> Result<(), String> {
    #[cfg(windows)]
    {
        crate::fsutil::move_file_no_replace(partial, final_path).map_err(|error| {
            if path_exists_or_symlink(final_path) {
                format!(
                    "output appeared while downloading: {} (it was not overwritten)",
                    final_path.display()
                )
            } else {
                format!(
                    "could not atomically move {} to {}: {error}",
                    partial.display(),
                    final_path.display()
                )
            }
        })
    }

    #[cfg(not(windows))]
    {
        fs::hard_link(partial, final_path).map_err(|error| {
        if error.kind() == std::io::ErrorKind::AlreadyExists {
            format!(
                "output appeared while downloading: {} (it was not overwritten)",
                final_path.display()
            )
        } else {
            format!(
                "could not atomically link {} to {}: {error}. Ensure both paths are on a filesystem that supports hard links",
                partial.display(),
                final_path.display()
            )
        }
        })?;

        if let Err(error) = fs::remove_file(partial) {
            let rollback = fs::remove_file(final_path);
            return Err(match rollback {
                Ok(()) => format!(
                    "could not remove staging name {}: {error}",
                    partial.display()
                ),
                Err(rollback_error) => format!(
                    "firmware was installed at {}, but cleanup of {} failed ({error}) and rollback also failed ({rollback_error})",
                    final_path.display(),
                    partial.display()
                ),
            });
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_directory(test_name: &str) -> PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "samloader-cli-{test_name}-{}-{nonce}",
            std::process::id()
        ));
        fs::create_dir(&path).unwrap();
        path
    }

    #[test]
    fn server_filename_cannot_escape_output_directory() {
        assert_eq!(
            safe_default_filename("firmware.zip.enc4").unwrap(),
            "firmware.zip"
        );
        assert_eq!(
            safe_default_filename("firmware.zip.ENC4").unwrap(),
            "firmware.zip"
        );
        for name in [
            "../firmware.zip.enc4",
            "folder/file.enc4",
            "folder\\file.enc4",
            "..",
        ] {
            assert!(safe_default_filename(name).is_err(), "{name}");
        }
    }

    #[test]
    fn partial_path_is_a_sibling_suffix() {
        assert_eq!(
            partial_path(Path::new("output/firmware.zip")),
            PathBuf::from("output/firmware.zip.part")
        );
    }

    #[test]
    fn completed_download_is_installed_without_overwrite() {
        let directory = temp_directory("install");
        let partial = directory.join("firmware.zip.part");
        let output = directory.join("firmware.zip");
        fs::write(&partial, b"verified").unwrap();

        install_completed_download(&partial, &output).unwrap();

        assert!(!partial.exists());
        assert_eq!(fs::read(&output).unwrap(), b"verified");
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn atomic_install_preserves_destination_that_appeared() {
        let directory = temp_directory("noclobber");
        let partial = directory.join("firmware.zip.part");
        let output = directory.join("firmware.zip");
        fs::write(&partial, b"new").unwrap();
        fs::write(&output, b"existing").unwrap();

        assert!(install_completed_download(&partial, &output).is_err());

        assert_eq!(fs::read(&partial).unwrap(), b"new");
        assert_eq!(fs::read(&output).unwrap(), b"existing");
        fs::remove_dir_all(directory).unwrap();
    }
}
