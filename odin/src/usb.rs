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

use crate::error::OdinError;
use nusb::MaybeFuture;
use rusb::{Context, DeviceHandle, UsbContext};
use std::io::{Read, Write};
use std::time::Duration;

macro_rules! print_warning {
    ($verbose:expr, $($arg:tt)*) => {
        if $verbose {
            eprint!("WARNING: ");
            eprintln!($($arg)*);
        }
    };
}

macro_rules! print_verbose {
    ($verbose:expr, $($arg:tt)*) => {
        if $verbose {
            eprintln!($($arg)*);
        }
    };
}

pub(crate) const VID_SAMSUNG: u16 = 0x04E8;
const PID_GALAXY_S: u16 = 0x6601;
const PID_GALAXY_S2: u16 = 0x685D;
const PID_DROID_CHARGE: u16 = 0x68C3;

const SUPPORTED_DEVICES: &[(u16, u16)] = &[
    (VID_SAMSUNG, PID_GALAXY_S),
    (VID_SAMSUNG, PID_GALAXY_S2),
    (VID_SAMSUNG, PID_DROID_CHARGE),
];

const USB_CLASS_CDC_DATA: u8 = 0x0A;

/// Trait representing a duplex data transport layer for USB or serial communication.
pub trait UsbTransfer {
    /// Resets the transport connection state and buffers.
    fn reset(&mut self);
    /// Sends a buffer of data across the transport with a timeout.
    fn send_data(&mut self, data: &[u8], timeout: i32, retry: bool) -> bool;
    /// Receives data from the transport into a buffer, returning the number of bytes read.
    fn receive_data(&mut self, data: &mut [u8], timeout: i32, retry: bool) -> i32;
}

/// Trait representing a backend factory and device locator for USB or serial transports.
pub trait UsbBackend: Sized + UsbTransfer {
    /// The associated device type identifier.
    type UsbDevice;

    /// Instantiates a new backend session wrapper for a given device.
    fn new(device: Self::UsbDevice, verbose: bool) -> Result<Self, OdinError>;
    /// Searches for a matching connected device based on a predicate.
    fn find_device<F>(wait: bool, predicate: F) -> Result<Self::UsbDevice, OdinError>
    where
        F: FnMut(u16, u16) -> bool;

    /// Searches for a connected device in Download Mode.
    fn find_download_device(wait: bool) -> Result<Self::UsbDevice, OdinError> {
        Self::find_device(wait, |vid, pid| SUPPORTED_DEVICES.contains(&(vid, pid)))
    }
}

/// USB communication backend using `libusb` / `rusb`.
pub struct RusbBackend {
    verbose: bool,
    handle: DeviceHandle<Context>,

    interface_index: i32,
    in_endpoint: u8,
    out_endpoint: u8,
}

impl RusbBackend {
    fn print_device_info(device: &rusb::Device<Context>, handle: &DeviceHandle<Context>) {
        if let Ok(descriptor) = device.device_descriptor() {
            if let Ok(languages) = handle.read_languages(Duration::from_secs(1))
                && !languages.is_empty()
            {
                if let Ok(manufacturer) = handle.read_manufacturer_string_ascii(&descriptor) {
                    eprintln!("      Manufacturer: \"{}\"", manufacturer);
                }
                if let Ok(product) = handle.read_product_string_ascii(&descriptor) {
                    eprintln!("           Product: \"{}\"", product);
                }
                if let Ok(serial) = handle.read_serial_number_string_ascii(&descriptor) {
                    eprintln!("         Serial No: \"{}\"", serial);
                }
            }

            eprintln!("\n            length: {}", descriptor.length());
            eprintln!("      device class: {}", descriptor.class_code());
            eprintln!(
                "               S/N: {}",
                descriptor.serial_number_string_index().unwrap_or(0)
            );
            eprintln!(
                "           VID:PID: {:04X}:{:04X}",
                descriptor.vendor_id(),
                descriptor.product_id()
            );

            let version = descriptor.device_version();
            let bcd = (version.0 as u16) << 8 | (version.1 as u16) << 4 | (version.2 as u16);
            eprintln!("         bcdDevice: {:04X}", bcd);

            eprintln!(
                "   iMan:iProd:iSer: {}:{}:{}",
                descriptor.manufacturer_string_index().unwrap_or(0),
                descriptor.product_string_index().unwrap_or(0),
                descriptor.serial_number_string_index().unwrap_or(0)
            );
            eprintln!("          nb confs: {}", descriptor.num_configurations());
        }
    }
}

impl UsbBackend for RusbBackend {
    type UsbDevice = rusb::Device<Context>;

    fn new(device: Self::UsbDevice, verbose: bool) -> Result<Self, OdinError> {
        let handle = match device.open() {
            Ok(h) => h,
            Err(e) => return Err(OdinError::DeviceAccess(e)),
        };

        if let Ok(config) = handle.active_configuration() {
            if config != 1 {
                let _ = handle.set_active_configuration(1);
            }
        } else {
            let _ = handle.set_active_configuration(1);
        }

        if verbose {
            Self::print_device_info(&device, &handle);
        }

        let config_descriptor = device
            .config_descriptor(0)
            .map_err(|_| OdinError::ConfigDescriptorRetrieval)?;

        let mut interface_index = -1;
        let mut alt_setting_index = -1;
        let mut in_endpoint = 0;
        let mut out_endpoint = 0;

        for interface in config_descriptor.interfaces() {
            for setting in interface.descriptors() {
                let mut in_endpoint_address = None;
                let mut out_endpoint_address = None;

                for endpoint in setting.endpoint_descriptors() {
                    if endpoint.direction() == rusb::Direction::In {
                        in_endpoint_address = Some(endpoint.address());
                    } else {
                        out_endpoint_address = Some(endpoint.address());
                    }
                }

                if interface_index < 0
                    && setting.num_endpoints() == 2
                    && setting.class_code() == USB_CLASS_CDC_DATA
                    && let (Some(in_addr), Some(out_addr)) =
                        (in_endpoint_address, out_endpoint_address)
                {
                    interface_index = interface.number() as i32;
                    alt_setting_index = setting.setting_number() as i32;
                    in_endpoint = in_addr;
                    out_endpoint = out_addr;
                }
            }
        }

        if interface_index < 0 {
            return Err(OdinError::InterfaceConfigurationNotFound);
        }

        print_verbose!(verbose, "Claiming interface...");
        if handle.claim_interface(interface_index as u8).is_err() {
            #[cfg(target_os = "linux")]
            {
                print_verbose!(verbose, "Attempt failed. Detaching driver...");
                let _ = handle.detach_kernel_driver(interface_index as u8);
                print_verbose!(verbose, "Claiming interface again...");
                if handle.claim_interface(interface_index as u8).is_err() {
                    return Err(OdinError::InterfaceClaimFailed);
                }
            }
            #[cfg(not(target_os = "linux"))]
            {
                return Err(OdinError::InterfaceClaimFailed);
            }
        }

        if alt_setting_index != 0 {
            print_verbose!(verbose, "Setting up interface...");
            handle
                .set_alternate_setting(interface_index as u8, alt_setting_index as u8)
                .map_err(|_| OdinError::InterfaceSetupFailed)?;
        }

        Ok(Self {
            verbose,
            handle,

            interface_index,
            in_endpoint,
            out_endpoint,
        })
    }

    fn find_device<F>(wait: bool, mut predicate: F) -> Result<Self::UsbDevice, OdinError>
    where
        F: FnMut(u16, u16) -> bool,
    {
        let context = Context::new().map_err(|e| {
            OdinError::SerialError(format!("Failed to create libusb context: {}", e))
        })?;

        let mut print_wait = false;
        loop {
            if let Ok(devices) = context.devices() {
                for device in devices.iter() {
                    if let Ok(descriptor) = device.device_descriptor()
                        && predicate(descriptor.vendor_id(), descriptor.product_id())
                    {
                        return Ok(device);
                    }
                }
            }

            if wait && !print_wait {
                println!("Waiting for device...");
                print_wait = true;
                std::thread::sleep(Duration::from_secs(1));
            } else {
                break;
            }
        }

        Err(OdinError::DeviceNotFound)
    }
}

impl UsbTransfer for RusbBackend {
    fn reset(&mut self) {
        if let Err(e) = self.handle.reset() {
            print_warning!(self.verbose, "Failed to reset device! Result: {}", e);
        }
    }

    fn send_data(&mut self, data: &[u8], timeout: i32, retry: bool) -> bool {
        let max_attempts = if retry { 6 } else { 1 };
        for attempt in 0..max_attempts {
            if attempt > 0 {
                print_verbose!(self.verbose, " Retrying...");
                std::thread::sleep(Duration::from_millis(250 * attempt));
            }

            let result = self.handle.write_bulk(
                self.out_endpoint,
                data,
                Duration::from_millis(timeout as u64),
            );

            if let Ok(transferred) = result {
                return transferred == data.len();
            };

            if retry {
                print_warning!(
                    self.verbose,
                    "libusb error {} whilst sending bulk transfer.",
                    result.as_ref().unwrap_err()
                );
            }
        }

        false
    }

    fn receive_data(&mut self, data: &mut [u8], timeout: i32, retry: bool) -> i32 {
        let max_attempts = if retry { 6 } else { 1 };

        for attempt in 0..max_attempts {
            if attempt > 0 {
                print_verbose!(self.verbose, " Retrying...");
                std::thread::sleep(Duration::from_millis(250 * attempt));
            }

            let result = self.handle.read_bulk(
                self.in_endpoint,
                data,
                Duration::from_millis(timeout as u64),
            );

            if let Ok(transferred) = result {
                return transferred as i32;
            };

            if retry {
                print_warning!(
                    self.verbose,
                    "libusb error {} whilst receiving bulk transfer.",
                    result.as_ref().unwrap_err()
                );
            }
        }
        -1
    }
}

impl Drop for RusbBackend {
    fn drop(&mut self) {
        print_verbose!(self.verbose, "Releasing device interface...");
        let _ = self.handle.release_interface(self.interface_index as u8);

        #[cfg(target_os = "linux")]
        {
            let _ = self.handle.attach_kernel_driver(self.interface_index as u8);
        }
    }
}

/// Serial VCOM communication backend using virtual serial ports.
pub struct SerialBackend {
    verbose: bool,
    port: Box<dyn serialport::SerialPort>,
}

impl SerialBackend {
    fn print_device_info(info: &serialport::UsbPortInfo) {
        if let Some(ref manufacturer) = info.manufacturer {
            eprintln!("      Manufacturer: \"{}\"", manufacturer);
        }
        if let Some(ref product) = info.product {
            eprintln!("           Product: \"{}\"", product);
        }
        if let Some(ref serial) = info.serial_number {
            eprintln!("         Serial No: \"{}\"", serial);
        }
        eprintln!("           VID:PID: {:04X}:{:04X}", info.vid, info.pid);
    }
}

impl UsbBackend for SerialBackend {
    type UsbDevice = serialport::SerialPortInfo;

    fn new(device: Self::UsbDevice, verbose: bool) -> Result<Self, OdinError> {
        let info = match &device.port_type {
            serialport::SerialPortType::UsbPort(info) => info,
            _ => return Err(OdinError::DeviceNotFound),
        };

        if verbose {
            Self::print_device_info(info);
        }

        let port = serialport::new(&device.port_name, 115_200)
            .timeout(Duration::from_millis(1000))
            .open()
            .map_err(|e| {
                OdinError::SerialError(format!("Failed to open port {}: {}", device.port_name, e))
            })?;

        Ok(Self { verbose, port })
    }

    fn find_device<F>(wait: bool, mut predicate: F) -> Result<Self::UsbDevice, OdinError>
    where
        F: FnMut(u16, u16) -> bool,
    {
        let mut print_wait = false;
        loop {
            if let Ok(ports) = serialport::available_ports() {
                for port in ports {
                    if let serialport::SerialPortType::UsbPort(ref info) = port.port_type
                        && predicate(info.vid, info.pid)
                    {
                        return Ok(port);
                    }
                }
            }

            if wait && !print_wait {
                println!("Waiting for device...");
                print_wait = true;
                std::thread::sleep(Duration::from_secs(1));
            } else {
                break;
            }
        }

        Err(OdinError::DeviceNotFound)
    }
}

impl UsbTransfer for SerialBackend {
    fn reset(&mut self) {
        if let Err(e) = self.port.clear(serialport::ClearBuffer::All) {
            print_warning!(self.verbose, "Failed to reset device! Result: {}", e);
        }
    }

    fn send_data(&mut self, data: &[u8], timeout: i32, retry: bool) -> bool {
        if let Err(e) = self.port.set_timeout(Duration::from_millis(timeout as u64)) {
            print_warning!(self.verbose, "Failed to set serial port timeout: {}", e);
        }

        let mut written = 0;
        let max_attempts = if retry { 6 } else { 1 };

        for attempt in 0..max_attempts {
            if attempt > 0 {
                print_verbose!(
                    self.verbose,
                    " Retrying (written {}/{})...",
                    written,
                    data.len()
                );
                std::thread::sleep(Duration::from_millis(250 * attempt));
            }

            while written < data.len() {
                match self.port.write(&data[written..]) {
                    Ok(0) => {
                        if retry {
                            print_warning!(self.verbose, "Serial write returned 0 bytes written");
                        }
                        break;
                    }
                    Ok(n) => {
                        written += n;
                    }
                    Err(ref e) if e.kind() == std::io::ErrorKind::Interrupted => {
                        continue;
                    }
                    Err(ref e) if e.kind() == std::io::ErrorKind::TimedOut => {
                        if retry {
                            print_warning!(self.verbose, "Serial write timed out: {}", e);
                        }
                        break;
                    }
                    Err(e) => {
                        if retry {
                            print_warning!(
                                self.verbose,
                                "Serial error whilst sending transfer: {}",
                                e
                            );
                        }
                        break;
                    }
                }
            }

            if written == data.len() {
                let _ = self.port.flush();
                return true;
            }
        }

        false
    }

    fn receive_data(&mut self, data: &mut [u8], timeout: i32, retry: bool) -> i32 {
        if let Err(e) = self.port.set_timeout(Duration::from_millis(timeout as u64)) {
            print_warning!(self.verbose, "Failed to set serial port timeout: {}", e);
        }

        let max_attempts = if retry { 6 } else { 1 };

        for attempt in 0..max_attempts {
            if attempt > 0 {
                print_verbose!(self.verbose, " Retrying...");
                std::thread::sleep(Duration::from_millis(250 * attempt));
            }

            match self.port.read(data) {
                Ok(bytes_read) => {
                    return bytes_read as i32;
                }
                Err(ref e) if e.kind() == std::io::ErrorKind::TimedOut => {
                    // Timed out, retry if allowed
                }
                Err(e) => {
                    if retry {
                        print_warning!(
                            self.verbose,
                            "Serial error whilst receiving transfer: {}",
                            e
                        );
                    }
                }
            }
        }
        -1
    }
}

/// USB communication backend using `nusb`.
pub struct NusbBackend {
    verbose: bool,
    device: nusb::Device,
    _interface: nusb::Interface,
    ep_in: nusb::Endpoint<nusb::transfer::Bulk, nusb::transfer::In>,
    ep_out: nusb::Endpoint<nusb::transfer::Bulk, nusb::transfer::Out>,
}

impl NusbBackend {
    fn print_device_info(device: &nusb::DeviceInfo, _handle: &nusb::Device) {
        if let Some(manufacturer) = device.manufacturer_string() {
            eprintln!("      Manufacturer: \"{}\"", manufacturer);
        }
        if let Some(product) = device.product_string() {
            eprintln!("           Product: \"{}\"", product);
        }
        if let Some(serial) = device.serial_number() {
            eprintln!("         Serial No: \"{}\"", serial);
        }

        eprintln!("\n      device class: {}", device.class());
        eprintln!(
            "           VID:PID: {:04X}:{:04X}",
            device.vendor_id(),
            device.product_id()
        );
    }
}

impl UsbBackend for NusbBackend {
    type UsbDevice = nusb::DeviceInfo;

    fn new(device: Self::UsbDevice, verbose: bool) -> Result<Self, OdinError> {
        let handle = device
            .open()
            .wait()
            .map_err(|e| OdinError::SerialError(format!("Failed to open device: {}", e)))?;

        // Traverse configuration and select the class USB_CLASS_CDC_DATA interface with 2 endpoints
        let mut interface_index = -1;
        let mut alt_setting_index = -1;
        let mut in_endpoint = 0;
        let mut out_endpoint = 0;

        if let Ok(config) = handle.active_configuration() {
            for interface in config.interface_alt_settings() {
                let mut in_endpoint_address = None;
                let mut out_endpoint_address = None;

                for endpoint in interface.endpoints() {
                    let addr = endpoint.address();
                    if addr & 0x80 != 0 {
                        in_endpoint_address = Some(addr);
                    } else {
                        out_endpoint_address = Some(addr);
                    }
                }

                if interface_index < 0
                    && interface.endpoints().count() == 2
                    && interface.class() == USB_CLASS_CDC_DATA
                    && let (Some(in_addr), Some(out_addr)) =
                        (in_endpoint_address, out_endpoint_address)
                {
                    interface_index = interface.interface_number() as i32;
                    alt_setting_index = interface.alternate_setting() as i32;
                    in_endpoint = in_addr;
                    out_endpoint = out_addr;
                }
            }
        }

        if interface_index < 0 {
            return Err(OdinError::InterfaceConfigurationNotFound);
        }

        if verbose {
            Self::print_device_info(&device, &handle);
        }

        let interface = handle
            .detach_and_claim_interface(interface_index as u8)
            .wait()
            .map_err(|_| OdinError::InterfaceClaimFailed)?;

        if alt_setting_index != 0 {
            interface
                .set_alt_setting(alt_setting_index as u8)
                .wait()
                .map_err(|_| OdinError::InterfaceSetupFailed)?;
        }

        let ep_in = interface
            .endpoint::<nusb::transfer::Bulk, nusb::transfer::In>(in_endpoint)
            .map_err(|e| OdinError::SerialError(format!("Failed to open IN endpoint: {}", e)))?;
        let ep_out = interface
            .endpoint::<nusb::transfer::Bulk, nusb::transfer::Out>(out_endpoint)
            .map_err(|e| OdinError::SerialError(format!("Failed to open OUT endpoint: {}", e)))?;

        Ok(Self {
            verbose,
            device: handle,
            _interface: interface,
            ep_in,
            ep_out,
        })
    }

    fn find_device<F>(wait: bool, mut predicate: F) -> Result<Self::UsbDevice, OdinError>
    where
        F: FnMut(u16, u16) -> bool,
    {
        let mut print_wait = false;
        loop {
            if let Ok(devices) = nusb::list_devices().wait() {
                for device in devices {
                    if predicate(device.vendor_id(), device.product_id()) {
                        return Ok(device);
                    }
                }
            }
            if wait && !print_wait {
                println!("Waiting for device...");
                print_wait = true;
                std::thread::sleep(Duration::from_secs(1));
            } else {
                break;
            }
        }
        Err(OdinError::DeviceNotFound)
    }
}

impl UsbTransfer for NusbBackend {
    fn reset(&mut self) {
        if let Err(e) = self.device.reset().wait() {
            print_warning!(self.verbose, "Failed to reset device! Result: {}", e);
        }
    }

    fn send_data(&mut self, data: &[u8], timeout: i32, retry: bool) -> bool {
        let max_attempts = if retry { 6 } else { 1 };
        for attempt in 0..max_attempts {
            if attempt > 0 {
                std::thread::sleep(Duration::from_millis(250 * attempt));
            }
            let buf = nusb::transfer::Buffer::from(data.to_vec());
            let completion = self
                .ep_out
                .transfer_blocking(buf, Duration::from_millis(timeout as u64));
            if completion.status.is_ok() {
                return completion.actual_len == data.len();
            }
        }
        false
    }

    fn receive_data(&mut self, data: &mut [u8], timeout: i32, retry: bool) -> i32 {
        let max_attempts = if retry { 6 } else { 1 };
        for attempt in 0..max_attempts {
            if attempt > 0 {
                std::thread::sleep(Duration::from_millis(250 * attempt));
            }
            let max_packet_size = self.ep_in.max_packet_size();
            let mut requested_len = data.len();
            if !requested_len.is_multiple_of(max_packet_size) {
                requested_len = ((requested_len / max_packet_size) + 1) * max_packet_size;
            }
            let buf = nusb::transfer::Buffer::new(requested_len);
            let completion = self
                .ep_in
                .transfer_blocking(buf, Duration::from_millis(timeout as u64));
            if let Ok(()) = completion.status {
                let to_copy = std::cmp::min(completion.actual_len, data.len());
                data[..to_copy].copy_from_slice(&completion.buffer[..to_copy]);
                return to_copy as i32;
            }
        }
        -1
    }
}

/// Creates and initializes the requested USB/VCOM communication backend interface.
pub fn create_backend(
    usb_backend: &str,
    verbose: bool,
    wait: bool,
) -> Result<Box<dyn UsbTransfer>, OdinError> {
    match usb_backend {
        "vcom" => {
            let device = SerialBackend::find_download_device(wait)?;
            let backend = SerialBackend::new(device, verbose)?;
            Ok(Box::new(backend))
        }
        "nusb" => {
            let device = NusbBackend::find_download_device(wait)?;
            let backend = NusbBackend::new(device, verbose)?;
            Ok(Box::new(backend))
        }
        _ => {
            let device = RusbBackend::find_download_device(wait)?;
            let backend = RusbBackend::new(device, verbose)?;
            Ok(Box::new(backend))
        }
    }
}
