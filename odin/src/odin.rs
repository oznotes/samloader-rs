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
use crate::packets;
use crate::packets::{InboundPacket, PitDataPacket, RequestPacket};
use crate::usb::UsbTransfer;
use samloader_pit::{BinaryType, PitEntry};
use std::time::Duration;

/// Driver and session manager coordinating the Samsung Odin/Loke flashing protocol.
pub struct OdinManager {
    verbose: bool,
    usb: Box<dyn UsbTransfer>,

    file_transfer_sequence_max_length: usize,
    file_transfer_packet_size: usize,
    file_transfer_sequence_timeout: u32,
    lz4_supported: bool,
    bootloader_protocol_version: u32,
    session_active: bool,
}

#[derive(PartialEq, Eq, Copy, Clone)]
enum EmptySendKind {
    None,
    Before,
    After,
    BeforeAndAfter,
}

const FILE_TRANSFER_SEQUENCE_MAX_LENGTH_DEFAULT: usize = 800;
const FILE_TRANSFER_PACKET_SIZE_DEFAULT: usize = 0x20000;
const FILE_TRANSFER_SEQUENCE_TIMEOUT_DEFAULT: u32 = 30000;

impl OdinManager {
    /// Creates a new `OdinManager` instance with a given transport and verbosity.
    pub fn new(usb: Box<dyn UsbTransfer>, verbose: bool) -> Self {
        Self {
            verbose,
            usb,

            file_transfer_sequence_max_length: FILE_TRANSFER_SEQUENCE_MAX_LENGTH_DEFAULT,
            file_transfer_packet_size: FILE_TRANSFER_PACKET_SIZE_DEFAULT,
            file_transfer_sequence_timeout: FILE_TRANSFER_SEQUENCE_TIMEOUT_DEFAULT,
            lz4_supported: false,
            bootloader_protocol_version: 0,
            session_active: false,
        }
    }

    /// Resets the connection transport and performs the "ODIN" / "LOKE" protocol handshake.
    pub fn init(&mut self) -> Result<(), OdinError> {
        println!("Initializing protocol...");

        self.usb.reset();

        self.send_string("ODIN", 1000)
            .map_err(|_| OdinError::HandshakeSendFailed)?;

        let response = self
            .receive_string(1000)
            .map_err(|_| OdinError::HandshakeReceiveFailed)?;

        if response == "LOKE" {
            println!("Protocol initialization successful.\n");
            Ok(())
        } else {
            Err(OdinError::HandshakeMismatch {
                expected: "LOKE".to_string(),
                received: response,
            })
        }
    }

    /// Begins an active flashing session, negotiating features such as packet size and LZ4 support.
    pub fn begin_session(&mut self) -> Result<(), OdinError> {
        println!("Beginning session...");

        // Once this request is sent the device may have entered a session even
        // when a later negotiation step fails. Track that possibility so Drop
        // can always make a best-effort attempt to close it.
        self.session_active = true;
        let packet = RequestPacket::begin_session();
        let session_response = self
            .request_and_response(&packet, EmptySendKind::After, 3000)
            .map_err(|_| OdinError::BeginSessionFailed)?;

        self.bootloader_protocol_version = if session_response == 0 {
            1
        } else {
            session_response >> 16
        };

        println!("\nSome devices may take up to 2 minutes to respond.\nPlease be patient!\n");
        std::thread::sleep(Duration::from_millis(3000));

        if self.bootloader_protocol_version >= 2 {
            self.lz4_supported = (session_response & 0x8000) != 0;
            self.file_transfer_sequence_timeout = 120000;
            self.file_transfer_packet_size = 0x100000;
            self.file_transfer_sequence_max_length = 30;

            let packet = RequestPacket::file_part_size(self.file_transfer_packet_size as u32);
            let value = self
                .request_and_response(&packet, EmptySendKind::After, 3000)
                .map_err(|_| OdinError::FilePartSizeSendFailed)?;

            if value != 0 {
                return Err(OdinError::UnexpectedFilePartSizeResponse(value));
            }
        }

        println!("Session begun.\n");
        Ok(())
    }

    /// Ends the active flashing session on the device.
    pub fn end_session(&mut self) -> Result<(), OdinError> {
        println!("Ending session...");

        // Clear this before I/O so a failed end packet is not sent repeatedly
        // from Drop. The packet below is the single best-effort cleanup attempt.
        self.session_active = false;
        let packet = RequestPacket::end_session();
        self.request_and_response(&packet, EmptySendKind::After, 3000)
            .map_err(|_| OdinError::EndSessionSendFailed)?;

        Ok(())
    }

    /// Reboots the device normally out of Download Mode.
    pub fn reboot_device(&mut self) -> Result<(), OdinError> {
        println!("Rebooting device...");

        let packet = RequestPacket::reboot_device();
        use crate::packets::OutboundPacket;
        if self.verbose {
            eprintln!("Sending packet: {:#04X?}", packet);
        }

        // The device immediately reboots and drops the USB connection,
        // so the write may partially fail and a response will never arrive.
        // We do a fire-and-forget send with no retries and a short timeout.
        let _ = self.usb.send_data(&packet.pack(), 500, false);

        // This is required for some devices e.g. A55.
        self.receive_empty(100);

        Ok(())
    }

    /// Sends a raw string message over the transport connection.
    pub fn send_string(&mut self, s: &str, timeout: i32) -> Result<(), OdinError> {
        if self.verbose {
            eprintln!("Sending string: {:?}", s);
        }
        if !self.usb.send_data(s.as_bytes(), timeout, true) {
            return Err(OdinError::SendPacketFailed);
        }
        Ok(())
    }

    fn receive_empty(&mut self, timeout: i32) {
        let mut buffer = vec![0u8; 1];
        self.usb.receive_data(&mut buffer, timeout, false);
    }

    /// Receives a raw string message from the transport connection.
    pub fn receive_string(&mut self, timeout: i32) -> Result<String, OdinError> {
        let mut buffer = [0u8; 1024];
        let received_size = self.usb.receive_data(&mut buffer, timeout, true);

        if received_size < 0 {
            return Err(OdinError::ReceivePacketFailed);
        }

        let mut data = buffer.to_vec();
        data.truncate(received_size as usize);
        if self.verbose {
            eprintln!("Received string data ({} bytes): {:?}", received_size, data);
        }
        Ok(String::from_utf8_lossy(&data).into_owned())
    }

    fn send_packet(
        &mut self,
        packet: &(impl packets::OutboundPacket + std::fmt::Debug),
        empty_send_kind: EmptySendKind,
        timeout: i32,
    ) -> Result<(), ()> {
        if self.verbose {
            eprintln!("Sending packet: {:#04X?}", packet);
        }
        let packet_bytes = packet.pack();
        if empty_send_kind == EmptySendKind::Before
            || empty_send_kind == EmptySendKind::BeforeAndAfter
        {
            self.usb.send_data(&[], 100, false);
        }
        if !self.usb.send_data(&packet_bytes, timeout, true) {
            return Err(());
        }
        if empty_send_kind == EmptySendKind::After
            || empty_send_kind == EmptySendKind::BeforeAndAfter
        {
            self.usb.send_data(&[], 100, false);
        }
        Ok(())
    }

    fn receive_packet_with_size<T: InboundPacket + std::fmt::Debug>(
        &mut self,
        size: usize,
        timeout: i32,
    ) -> Result<T, OdinError> {
        let mut buffer = vec![0u8; size];
        let received_size = self.usb.receive_data(&mut buffer, timeout, true);

        if received_size < 0 {
            return Err(OdinError::ReceivePacketFailed);
        }

        buffer.truncate(received_size as usize);
        let parsed = T::unpack(&buffer).map_err(OdinError::ParseError)?;
        if self.verbose {
            eprintln!("Received packet: {:#04X?}", parsed);
        }
        Ok(parsed)
    }

    fn receive_packet<T: InboundPacket + std::fmt::Debug>(
        &mut self,
        timeout: i32,
    ) -> Result<T, OdinError> {
        self.receive_packet_with_size::<T>(T::SIZE, timeout)
    }

    fn request_and_response(
        &mut self,
        packet: &RequestPacket,
        empty_send_kind: EmptySendKind,
        timeout: i32,
    ) -> Result<u32, OdinError> {
        self.send_packet(packet, empty_send_kind, timeout)
            .map_err(|_| OdinError::SendPacketFailed)?;

        let response = self.receive_packet::<packets::Response>(timeout)?;
        let expected_type = packet.expected_response_type();

        if response.response_type != expected_type {
            return Err(OdinError::ResponseTypeMismatch {
                expected: expected_type,
                received: response.response_type,
            });
        }

        Ok(response.value)
    }

    /// Flashes/uploads raw PIT data to the device.
    pub fn send_pit_data(&mut self, pit_buffer: &[u8]) -> Result<(), OdinError> {
        let pit_buffer_size = pit_buffer.len() as u32;

        // Start file transfer
        let packet = RequestPacket::pit_file_flash();
        self.request_and_response(&packet, EmptySendKind::After, 3000)
            .map_err(|_| OdinError::PitFileTransferInitFailed)?;

        // Transfer file size
        let packet = RequestPacket::flash_part_pit_file(pit_buffer_size);
        self.request_and_response(&packet, EmptySendKind::After, 3000)
            .map_err(|_| OdinError::PitFilePartInfoSendFailed)?;

        // Flash pit file
        let packet = packets::FilePartPacket::new(pit_buffer, pit_buffer_size);
        self.send_packet(&packet, EmptySendKind::After, 3000)
            .map_err(|_| OdinError::SendPacketFailed)?;

        let response = self.receive_packet::<packets::Response>(3000)?;

        if response.response_type != packets::RESPONSE_TYPE_PIT_FILE {
            return Err(OdinError::ResponseTypeMismatch {
                expected: packets::RESPONSE_TYPE_PIT_FILE,
                received: response.response_type,
            });
        }

        // End pit file transfer
        let packet = RequestPacket::end_pit_file_transfer(pit_buffer_size);
        self.request_and_response(&packet, EmptySendKind::After, 3000)
            .map_err(|_| OdinError::PitFileTransferEndSendFailed)?;

        Ok(())
    }

    /// Downloads/dumps the active Partition Information Table (PIT) file from the device.
    pub fn download_pit_file(&mut self) -> Result<Vec<u8>, OdinError> {
        println!("Downloading device's PIT file...");

        let packet = RequestPacket::pit_file_dump();
        let file_size = self
            .request_and_response(&packet, EmptySendKind::After, 3000)
            .map_err(|_| OdinError::PitFileSizeReceiveFailed)? as usize;

        let transfer_count = file_size.div_ceil(PitDataPacket::SIZE);
        let mut buffer = Vec::with_capacity(file_size);

        for i in 0..transfer_count {
            let packet = RequestPacket::dump_part_pit_file(i as u32);
            self.send_packet(&packet, EmptySendKind::After, 3000)
                .map_err(|_| OdinError::PitFilePartRequestFailed(i as u32))?;

            let expected_size = std::cmp::min(file_size - buffer.len(), PitDataPacket::SIZE);

            let part = self
                .receive_packet_with_size::<PitDataPacket>(expected_size, 3000)
                .map_err(|_| OdinError::PitFilePartReceiveFailed(i as u32))?;
            buffer.extend_from_slice(&part.data);
        }

        // Receive empty packet after the last PIT transfer,
        // this is required for some older devices e.g. Tab S2 VE.
        self.receive_empty(100);

        // End file transfer
        let packet = RequestPacket::pit_file_end();
        self.request_and_response(&packet, EmptySendKind::After, 3000)
            .map_err(|_| OdinError::PitFileEndSendFailed)?;

        println!("PIT file download successful.\n");
        Ok(buffer)
    }

    /// Returns whether the negotiated device session supports flashing LZ4-compressed streams.
    pub fn is_lz4_supported(&self) -> bool {
        self.lz4_supported
    }

    /// Returns the negotiated bootloader protocol version of the connected device.
    pub fn bootloader_protocol_version(&self) -> u32 {
        self.bootloader_protocol_version
    }

    fn file_transfer_sequence_max_bytes(&self) -> usize {
        self.file_transfer_packet_size * self.file_transfer_sequence_max_length
    }

    fn send_raw_sequences<Iter, Bytes>(
        &mut self,
        sequences: Iter,
        pit_entry: &PitEntry,
    ) -> Result<(), OdinError>
    where
        Bytes: AsRef<[u8]>,
        Iter: Iterator<Item = Bytes>,
    {
        let mut progress = |_| {};
        self.send_raw_sequences_with_progress(sequences, pit_entry, &mut progress)
    }

    fn send_raw_sequences_with_progress<Iter, Bytes, F>(
        &mut self,
        sequences: Iter,
        pit_entry: &PitEntry,
        progress: &mut F,
    ) -> Result<(), OdinError>
    where
        Bytes: AsRef<[u8]>,
        Iter: Iterator<Item = Bytes>,
        F: FnMut(u64),
    {
        let packet = RequestPacket::file_transfer_flash();
        self.request_and_response(&packet, EmptySendKind::After, 3000)
            .map_err(|_| OdinError::FileTransferInitFailed)?;

        let mut sequences = sequences.peekable();
        while let Some(sequence_data) = sequences.next() {
            let sequence_data = sequence_data.as_ref();
            let start_packet = RequestPacket::flash_part_file_transfer(sequence_data.len() as u32);

            let is_last_sequence = sequences.peek().is_none();
            let end_packet = match pit_entry.binary_type {
                BinaryType::ApplicationProcessor => RequestPacket::end_phone_file_transfer(
                    sequence_data.len() as u32,
                    pit_entry,
                    is_last_sequence,
                ),
                BinaryType::CommunicationProcessor => RequestPacket::end_modem_file_transfer(
                    sequence_data.len() as u32,
                    pit_entry,
                    is_last_sequence,
                ),
            };

            self.send_one_sequence(
                &start_packet,
                &end_packet,
                sequence_data,
                sequence_data.len() as u64,
                progress,
            )?;
        }

        Ok(())
    }

    /// Flashes an uncompressed partition firmware file payload to the device.
    pub fn send_file(&mut self, info: &crate::firmware::FirmwareFile) -> Result<(), OdinError> {
        let sequences = info.sequences(self.file_transfer_sequence_max_bytes());
        self.send_raw_sequences(sequences, info.pit_entry)
    }

    /// Flashes an uncompressed partition firmware file payload to the device,
    /// calling `progress` with transferred byte deltas after each acknowledged file part.
    pub fn send_file_with_progress<F>(
        &mut self,
        info: &crate::firmware::FirmwareFile,
        progress: &mut F,
    ) -> Result<(), OdinError>
    where
        F: FnMut(u64),
    {
        let sequences = info.sequences(self.file_transfer_sequence_max_bytes());
        self.send_raw_sequences_with_progress(sequences, info.pit_entry, progress)
    }

    /// Flashes an LZ4-compressed partition firmware file payload to the device,
    /// decompressing on-the-fly if needed.
    pub fn send_lz4_file(
        &mut self,
        info: &crate::firmware::FirmwareLz4File,
    ) -> Result<(), OdinError> {
        let mut progress = |_| {};
        self.send_lz4_file_with_progress(info, &mut progress)
    }

    /// Flashes an LZ4-compressed partition firmware file payload to the device,
    /// calling `progress` with decompressed byte deltas after acknowledged file parts.
    pub fn send_lz4_file_with_progress<F>(
        &mut self,
        info: &crate::firmware::FirmwareLz4File,
        progress: &mut F,
    ) -> Result<(), OdinError>
    where
        F: FnMut(u64),
    {
        info.validate()
            .map_err(|error| OdinError::ParseError(format!("LZ4 preflight failed: {error}")))?;

        if !self.lz4_supported || info.header.block_max_size != 1024 * 1024 {
            let sequences = info.decompressed_sequences(self.file_transfer_sequence_max_bytes());
            return self.send_raw_sequences_with_progress(sequences, info.pit_entry, progress);
        }

        let packet = RequestPacket::lz4_file_transfer_flash();
        self.request_and_response(&packet, EmptySendKind::After, 3000)
            .map_err(|_| OdinError::FileTransferInitFailed)?;

        let sequences = info.sequences(self.file_transfer_sequence_max_bytes());

        let mut sequences = sequences.peekable();
        while let Some((decompressed_size, sequence_data)) = sequences.next() {
            let start_packet =
                RequestPacket::flash_lz4_part_file_transfer(sequence_data.len() as u32);

            let is_last_sequence = sequences.peek().is_none();
            let end_packet = match info.pit_entry.binary_type {
                BinaryType::ApplicationProcessor => RequestPacket::end_lz4_phone_file_transfer(
                    decompressed_size as u32,
                    info.pit_entry,
                    is_last_sequence,
                ),
                BinaryType::CommunicationProcessor => RequestPacket::end_lz4_modem_file_transfer(
                    decompressed_size as u32,
                    info.pit_entry,
                    is_last_sequence,
                ),
            };

            self.send_one_sequence(
                &start_packet,
                &end_packet,
                sequence_data,
                decompressed_size as u64,
                progress,
            )?;
        }

        Ok(())
    }

    fn send_one_sequence(
        &mut self,
        start_packet: &RequestPacket,
        end_packet: &RequestPacket,
        sequence_data: &[u8],
        progress_total_bytes: u64,
        progress: &mut impl FnMut(u64),
    ) -> Result<(), OdinError> {
        self.request_and_response(start_packet, EmptySendKind::BeforeAndAfter, 3000)
            .map_err(|_| OdinError::FileTransferSequenceBeginFailed)?;

        let mut sequence_wire_bytes = 0_u64;
        let mut sequence_progress_bytes = 0_u64;

        for (file_part_index, file_buffer) in sequence_data
            .chunks(self.file_transfer_packet_size)
            .enumerate()
        {
            let mut success = false;
            for retry in 0..5 {
                if retry > 0 {
                    println!("\nRetrying...");
                }

                let packet = packets::FilePartPacket::new(
                    file_buffer,
                    self.file_transfer_packet_size as u32,
                );

                let empty_send_kind = if file_part_index == 0 {
                    EmptySendKind::None
                } else {
                    EmptySendKind::Before
                };

                if self.send_packet(&packet, empty_send_kind, 3000).is_err() {
                    continue;
                }

                match self
                    .receive_packet::<packets::Response>(self.file_transfer_sequence_timeout as i32)
                {
                    Ok(response)
                        if response.response_type == packets::RESPONSE_TYPE_SEND_FILE_PART =>
                    {
                        if response.value as usize == file_part_index {
                            success = true;
                            sequence_wire_bytes += file_buffer.len() as u64;
                            let progress_bytes = if sequence_data.is_empty() {
                                progress_total_bytes
                            } else {
                                progress_total_bytes.saturating_mul(sequence_wire_bytes)
                                    / sequence_data.len() as u64
                            };
                            let delta = progress_bytes.saturating_sub(sequence_progress_bytes);
                            if delta > 0 {
                                progress(delta);
                                sequence_progress_bytes = progress_bytes;
                            }
                            break;
                        } else if retry == 0 {
                            return Err(OdinError::FilePartIndexMismatch {
                                expected: file_part_index,
                                received: response.value,
                            });
                        }
                    }
                    _ => {}
                }
            }

            if !success {
                return Err(OdinError::FilePartResponseReceiveFailed);
            }
        }

        let remaining = progress_total_bytes.saturating_sub(sequence_progress_bytes);
        if remaining > 0 {
            progress(remaining);
        }

        self.request_and_response(
            end_packet,
            EmptySendKind::BeforeAndAfter,
            self.file_transfer_sequence_timeout as i32,
        )
        .map_err(|_| OdinError::FileTransferSequenceEndFailed)?;

        Ok(())
    }

    /// Sets the total expected session bytes to be flashed, allowing the device
    /// to update its progress indicator.
    pub fn set_total_bytes(&mut self, total_bytes: u64) -> Result<(), OdinError> {
        let packet = RequestPacket::total_bytes(total_bytes);
        let value = self
            .request_and_response(&packet, EmptySendKind::After, 3000)
            .map_err(|_| OdinError::TotalBytesSendFailed)?;

        if value != 0 {
            return Err(OdinError::UnexpectedTotalBytesResponse(value));
        }

        Ok(())
    }
}

impl Drop for OdinManager {
    fn drop(&mut self) {
        if self.session_active {
            let _ = self.end_session();
        }
    }
}

/// Triggers a reboot of the connected Samsung device into Download Mode via the
/// specified backend protocol.
pub fn reboot_download(usb_backend: crate::usb::UsbBackendOption) -> Result<(), OdinError> {
    use crate::usb::{UsbTransfer, VID_SAMSUNG};

    let mut backend: Box<dyn UsbTransfer> = match usb_backend {
        #[cfg(feature = "serialport")]
        crate::usb::UsbBackendOption::Vcom => {
            use crate::usb::{SerialBackend, UsbBackend};
            let device = SerialBackend::find_device(false, |vid, _| vid == VID_SAMSUNG)?;
            Ok::<Box<dyn UsbTransfer>, OdinError>(Box::new(SerialBackend::new(device, false)?))
        }
        #[cfg(feature = "nusb")]
        crate::usb::UsbBackendOption::Nusb => {
            use crate::usb::{NusbBackend, UsbBackend};
            let device = NusbBackend::find_device(false, |vid, _| vid == VID_SAMSUNG)?;
            Ok::<Box<dyn UsbTransfer>, OdinError>(Box::new(NusbBackend::new(device, false)?))
        }
        #[cfg(feature = "rusb")]
        crate::usb::UsbBackendOption::Libusb => {
            use crate::usb::{RusbBackend, UsbBackend};
            let device = RusbBackend::find_device(false, |vid, _| vid == VID_SAMSUNG)?;
            Ok::<Box<dyn UsbTransfer>, OdinError>(Box::new(RusbBackend::new(device, false)?))
        }
        #[cfg(any(feature = "mock", debug_assertions, test))]
        crate::usb::UsbBackendOption::Mock => {
            use crate::usb::MockBackend;
            Ok::<Box<dyn UsbTransfer>, OdinError>(Box::new(MockBackend::new(false)))
        }
    }?;

    let cmd: &[u8] = b"AT+SUDDLMOD=0,0\r";

    if !backend.send_data(cmd, 1000, false) {
        return Err(OdinError::SerialError("Failed to send data".to_string()));
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::firmware::{FirmwareLz4File, Lz4FrameHeader};
    use crate::usb::{MockBackend, UsbTransfer};
    use lz4_flex::frame::{BlockSize, FrameEncoder, FrameInfo};
    use memmap2::MmapOptions;
    use samloader_pit::PitData;
    use std::fs::{OpenOptions, remove_file};
    use std::io::Write;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    struct CountingBackend {
        send_count: Arc<AtomicUsize>,
    }

    impl UsbTransfer for CountingBackend {
        fn reset(&mut self) {}

        fn send_data(&mut self, _data: &[u8], _timeout: i32, _retry: bool) -> bool {
            self.send_count.fetch_add(1, Ordering::Relaxed);
            false
        }

        fn receive_data(&mut self, _data: &mut [u8], _timeout: i32, _retry: bool) -> i32 {
            0
        }
    }

    fn truncated_lz4_frame() -> Vec<u8> {
        let payload = vec![0x5a; 4096];
        let frame_info = FrameInfo::new()
            .block_size(BlockSize::Max1MB)
            .content_size(Some(payload.len() as u64));
        let mut encoder = FrameEncoder::with_frame_info(frame_info, Vec::new());
        encoder.write_all(&payload).unwrap();
        let mut frame = encoder.finish().unwrap();
        frame.pop();
        frame
    }

    fn map_test_frame(frame: &[u8], label: &str) -> (std::path::PathBuf, memmap2::Mmap) {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "samloader-odin-lz4-{label}-{}-{nonce}.bin",
            std::process::id()
        ));
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(&path)
            .unwrap();
        file.write_all(frame).unwrap();
        file.flush().unwrap();
        let mmap = unsafe { MmapOptions::new().map(&file).unwrap() };
        (path, mmap)
    }

    #[test]
    fn test_odin_mock_session_and_pit_download() {
        let backend = Box::new(MockBackend::new(true));
        let mut manager = OdinManager::new(backend, true);

        // handshake
        assert!(manager.init().is_ok());

        // session begin
        assert!(manager.begin_session().is_ok());
        assert!(manager.session_active);
        assert_eq!(manager.bootloader_protocol_version(), 2);
        assert!(manager.is_lz4_supported());

        // PIT dump
        let pit_bytes = manager.download_pit_file().unwrap();
        // Q7MQ_EUR_OPENX.pit is exactly 18492 bytes including cryptographic signature
        assert_eq!(pit_bytes.len(), 18492);

        // end session
        assert!(manager.end_session().is_ok());
        assert!(!manager.session_active);
    }

    #[test]
    fn malformed_lz4_is_rejected_before_writes_in_both_transfer_modes() {
        let frame = truncated_lz4_frame();
        let header = Lz4FrameHeader::parse(&frame[..]).unwrap();
        let pit = PitData::new(include_bytes!("../../test-data/Q7MQ_EUR_OPENX.pit")).unwrap();

        for lz4_supported in [false, true] {
            let (path, mmap) = map_test_frame(
                &frame,
                if lz4_supported {
                    "compressed"
                } else {
                    "fallback"
                },
            );
            let send_count = Arc::new(AtomicUsize::new(0));
            let backend = Box::new(CountingBackend {
                send_count: Arc::clone(&send_count),
            });
            let mut manager = OdinManager::new(backend, false);
            manager.lz4_supported = lz4_supported;
            let firmware = FirmwareLz4File {
                pit_entry: &pit.entries[0],
                file: mmap,
                header: Lz4FrameHeader {
                    content_size: header.content_size,
                    block_max_size: header.block_max_size,
                },
            };

            let error = manager.send_lz4_file(&firmware).unwrap_err();
            assert!(matches!(error, OdinError::ParseError(_)));
            assert_eq!(
                send_count.load(Ordering::Relaxed),
                0,
                "invalid LZ4 data must be rejected before transport writes"
            );

            drop(firmware);
            remove_file(path).unwrap();
        }
    }
}
