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

//! Error types for samloader-fus.

use thiserror::Error;

/// Error types for samloader-fus.
#[derive(Debug, Error)]
pub enum Error {
    /// An I/O error occurred during file initialization, memory mapping, flushing, or truncating.
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    /// A network error occurred while communicating with the FUS server.
    #[error("Network error: {0}")]
    Network(#[from] reqwest::Error),

    /// The download server returned an HTTP status that did not satisfy the
    /// request. `retryable` distinguishes transient server throttling/outages
    /// from permanent authorization, range, and client errors.
    #[error("Firmware server returned HTTP {status}: {message}")]
    HttpStatus {
        /// Numeric HTTP status code returned by the server.
        status: u16,
        /// Whether retrying this response can reasonably succeed.
        retryable: bool,
        /// Human-readable status and request context.
        message: String,
    },

    /// The FUS server returned a successful HTTP response that did not contain
    /// the expected firmware metadata.
    #[error("Invalid FUS response: {message}")]
    InvalidResponse {
        /// A description of the missing or malformed response data.
        message: String,
    },

    /// The decrypted firmware archive failed structural or checksum
    /// verification and must not be installed or flashed.
    #[error("Firmware integrity verification failed: {message}")]
    Integrity {
        /// Description of the malformed archive or failed checksum.
        message: String,
    },

    /// The requested download was cancelled by the caller.
    #[error("Download cancelled")]
    Cancelled,

    /// A parallel download was requested with a worker count outside the
    /// library's supported range.
    #[error("Download thread count must be between 1 and {max} (requested {requested})")]
    InvalidThreadCount {
        /// The worker count requested by the caller.
        requested: u64,
        /// The maximum worker count supported by the downloader.
        max: u64,
    },

    /// The requested destination already exists. Downloads use exclusive file
    /// creation so concurrent callers cannot truncate or share a memory map.
    #[error("Download destination already exists or is in use: {path}")]
    DestinationExists {
        /// Destination path that could not be exclusively created.
        path: String,
    },

    /// All retry attempts failed without advancing the download. The last
    /// transport or transient server failure is retained as `cause`.
    #[error("Download failed at offset {offset} after retries: {cause}")]
    RetryExhausted {
        /// Absolute ciphertext offset where progress stopped.
        offset: u64,
        /// Last observed transport or transient HTTP failure.
        cause: String,
    },

    /// The download stalled completely because the server stopped responding.
    #[error("Download stalled at offset {offset}: server stopped responding")]
    Stalled {
        /// The byte offset where the stall was detected.
        offset: u64,
    },
}

/// A specialized Result type for samloader-fus operations.
pub type Result<T> = std::result::Result<T, Error>;
