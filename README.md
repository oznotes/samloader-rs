# samloader

An all-in-one Samsung firmware discovery, download, verification, and flash
tool with a command-line interface and a Windows desktop application.

> [!WARNING]
> Firmware flashing writes device partitions. Confirm the exact device, model,
> CSC, and package set before starting. Regular `CSC` packages factory-reset the
> device; `HOME_CSC` is the data-preserving default. Never disconnect or close
> the application while a partition is being written.

```
Usage: samloader [OPTIONS] <COMMAND>

Commands:
  download         Download firmware
  check-update     Check available versions
  detect           Indicates whether or not a download mode device can be detected
  dump-pit         Dumps the connected device's PIT file to the specified output file
  print-pit        Prints the contents of a PIT file in a human readable format
  flash            Flashes one or more firmware files to your phone
  verify-md5       Verifies the MD5 checksum of one or more .tar.md5 files
  reboot-download  Boot a connected Samsung device into download mode
  help             Print this message or the help of the given subcommand(s)

Options:
      --verbose                    Enable verbose output
      --usb-backend <usb_backend>  The USB backend to use [default: vcom] [possible values: nusb, vcom]
  -h, --help                       Print help
  -V, --version                    Print version
```

## Features

- Combines both firmware downloading and flashing into a single utility.
- Downloads firmware using multiple parallel connections (default: 8) to bypass server-side connection speed throttling and maximize bandwidth usage.
- Decrypts firmware files on-the-fly, eliminating separate download and decryption steps.
- Supports flashing raw images and official package files across Linux, macOS, and Windows.
- Flashes official firmware ZIPs directly and auto-selects BL/AP/CP/CSC/USERDATA packages.
- Auto-selects the same package set from an extracted firmware folder.
- Processes and extracts official TAR firmware packages in-memory, avoiding slow disk write operations.
- Transmits raw LZ4-compressed data directly to supported devices to reduce USB transfer size and time.

## Flashing official firmware

### Firmware ZIP

Flash a complete official firmware ZIP without extracting it manually:

```bash
samloader flash --zip /path/to/firmware.zip
```

`HOME_CSC` is selected by default to preserve user data. Select regular `CSC`
only when a factory reset is intended:

```bash
samloader flash --zip /path/to/firmware.zip --csc-mode wipe
```

ZIP members stored without compression are read directly from the archive.
Compressed BL/AP/CP/CSC/USERDATA members are unpacked automatically into a
temporary `.samloader-flash-*` directory beside the ZIP, or into the operating
system temporary directory when the ZIP location is not writable or lacks
space. Both locations are checked before extraction, and the temporary
directory is removed after the flash finishes or exits with a handled error.

### Extracted firmware folder

An already-extracted package folder can also be flashed directly:

```bash
samloader flash --folder /path/to/firmware-folder
```

Folder flashing selects `HOME_CSC` by default to avoid a factory reset. To select the regular `CSC` package instead, use:

```bash
samloader flash --folder /path/to/firmware-folder --csc-mode wipe
```

## Install

### Windows desktop

The desktop interface can list known stable, historical, and beta firmware;
suggest or accept custom CSC regions; read the model and CSC from one authorized
ADB device; download with pause/resume/cancel and disk-space preflight; inspect
PIT data; verify `.tar.md5` packages; and flash from individual packages, an
official firmware ZIP, or an extracted firmware folder. The equivalent CLI
command remains visible for review and copying.

Use the signed installer for the normal consumer setup. The portable executable
requires the Microsoft Edge WebView2 runtime, which is normally present on
current Windows 10 and Windows 11 systems. Device identification requires
Android Platform Tools (`adb`) in `PATH`; Download Mode operations require an
appropriate Samsung USB/VCOM or WinUSB driver.

Every public Windows release includes:

- an Authenticode-signed installer and portable executable;
- an Authenticode-signed command-line executable;
- `SHA256SUMS.txt` for every executable; and
- project and third-party license notices.

Do not use a public binary if Windows reports a missing or invalid signature.

### Command line

If you have a working Rust toolchain installed, you can install with:

```bash
cargo install samloader
```

Prebuilt executables are available from this repository's [latest GitHub release](../../releases/latest).

See [RELEASE.md](RELEASE.md) for the automated and physical-device release
gates, and [SECURITY.md](SECURITY.md) for private vulnerability reporting.

## License & Acknowledgements

This project is licensed under the **Apache License, Version 2.0**.

`samloader` is built on top of and inspired by several incredible open-source projects:

- **Heimdall (Firmware Flashing):**
  The core flashing functionality is a derivative work of [~grimler/Heimdall](https://git.sr.ht/~grimler/Heimdall). This implementation began as a precise 1-to-1 conversion of the original C++ project into safe and idiomatic Rust. The original code is licensed under the **MIT License** (preserved in this repository), and copyright headers are preserved in the relevant source files.

- **Firmware Downloading / FUS:**
  The firmware downloading and decryption implementation was inspired from multiple places, including [samloader](https://github.com/samloader/samloader), [samfirm.js](https://github.com/jesec/samfirm.js/), [Bifrost](https://github.com/zacharee/Bifrost), and [asgard](https://github.com/ducthoe/asgard).
