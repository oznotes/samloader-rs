// Copyright 2026 John "topjohnwu" Wu
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

#[cfg(windows)]
use std::os::windows::ffi::OsStrExt;
#[cfg(windows)]
use std::path::Path;

#[cfg(windows)]
#[link(name = "Kernel32")]
unsafe extern "system" {
    fn MoveFileExW(existing: *const u16, destination: *const u16, flags: u32) -> i32;
}

/// Atomically moves a file on Windows while refusing to replace an existing
/// destination. Unlike `std::fs::rename`, this deliberately omits
/// `MOVEFILE_REPLACE_EXISTING` and therefore works on FAT/exFAT without losing
/// no-clobber semantics.
#[cfg(windows)]
pub(crate) fn move_file_no_replace(existing: &Path, destination: &Path) -> std::io::Result<()> {
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
    // SAFETY: Both pointers refer to live, nul-terminated UTF-16 buffers for
    // the duration of the call. A zero flags value requests no replacement.
    let moved = unsafe { MoveFileExW(existing.as_ptr(), destination.as_ptr(), 0) };
    if moved == 0 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(())
    }
}
