# Heimdall

Heimdall is a cross-platform open-source tool suite used to flash
firmware (aka ROMs) onto Samsung mobile devices.

## Supported Platforms

Heimdall should work on Linux, macOS, and Windows.

## How does Heimdall work?

Heimdall connects to a mobile device over USB and interacts with
low-level software running on the device, known as Loke. Loke and
Heimdall communicate via the custom Samsung-developed protocol
typically referred to as the 'Odin protocol'.

USB communication in Heimdall is handled by the popular open-source
USB library, [libusb](https://libusb.info).

## Free & Open Source

This project is a derivative work of [~grimler/Heimdall](https://git.sr.ht/~grimler/Heimdall).
The original C++ code is licensed under the MIT License.
The Rust rewrite and all new contributions are licensed under the Apache License, Version 2.0.
