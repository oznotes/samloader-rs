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

use crate::{Error, Result, auth, http, xml};
use aes::cipher::KeyInit;
use reqwest::StatusCode;
use reqwest::blocking::{Client, Response};
use reqwest::header::{AUTHORIZATION, CONTENT_RANGE, RANGE, USER_AGENT};
use std::sync::Mutex;
use std::time::Duration;
use xml::{BinaryInform, VersionInfo};

const FUS_API_BASE: &str = "https://neofussvr.sslcs.cdngc.net";
const FUS_DOWNLOAD_ENDPOINT: &str =
    "https://cloud-neofussvr.samsungmobile.com/NF_SmartDownloadBinaryForMass.do";

/// Block decryption cipher alias using AES-128 in ECB mode for FUS stream processing.
pub type Aes128EcbDec = ecb::Decryptor<aes::Aes128>;

/// Authentication token state. The download GET signs every request with
/// `encnonce`/`auth`; these expire, so they live behind a lock and are shared
/// by reference across all download threads (which only read them) and the
/// occasional reauthentication (which rewrites them). See [`FusClient`].
#[derive(Default)]
struct AuthState {
    auth: String,
    nonce: String,
    encnonce: String,
}

/// Client coordinating API communication, security authentication, and file downloading
/// with the Samsung Firmware Update Server (FUS).
pub struct FusClient {
    client: Client,
    api_base: String,
    download_endpoint: String,
    auth_state: Mutex<AuthState>,
    /// Reauthentication generation counter. It also serves as the lock that
    /// serializes reauth: a token expiry observed by many download threads at
    /// once triggers a single refresh, and the rest reuse its result.
    reauth_gen: Mutex<u64>,
    /// The parsed and resolved binary download metadata for the active firmware session.
    pub info: BinaryInform,
}

impl FusClient {
    /// Creates a new `FusClient` and initiates the standard FUS handshake to generate a session nonce.
    pub fn new() -> reqwest::Result<Self> {
        let client = http::client_builder()
            .cookie_store(true)
            // Reqwest's blocking client applies `timeout` to request setup and
            // afresh to every response-body `Read`. It therefore detects 10s
            // stalls without imposing a whole-download deadline. (The async
            // ClientBuilder's same-named method has different total-deadline
            // semantics and is intentionally not the builder used here.)
            .timeout(Duration::from_secs(10))
            .connect_timeout(Duration::from_secs(10))
            .build()?;
        let fus = FusClient {
            client,
            api_base: FUS_API_BASE.to_string(),
            download_endpoint: FUS_DOWNLOAD_ENDPOINT.to_string(),
            auth_state: Mutex::new(AuthState::default()),
            reauth_gen: Mutex::new(0),
            info: Default::default(),
        };

        // Initialize nonce
        fus.make_req(
            "NF_SmartDownloadGenerateNonce.do",
            fus.make_authorization_value(),
            "",
        )?;

        Ok(fus)
    }

    /// Constructs a handshake-free client pointed at a local HTTP fixture.
    #[cfg(test)]
    pub(crate) fn new_for_download_test(
        download_endpoint: String,
        info: BinaryInform,
    ) -> reqwest::Result<Self> {
        Ok(Self {
            client: http::client_builder()
                .timeout(Duration::from_secs(2))
                .build()?,
            api_base: download_endpoint.clone(),
            download_endpoint,
            auth_state: Mutex::new(AuthState::default()),
            reauth_gen: Mutex::new(0),
            info,
        })
    }

    /// Queries the server for firmware binary metadata matching the requested model, region, and version.
    pub fn fetch_binary_info(&mut self, model: &str, region: &str, version: &str) {
        self.try_fetch_binary_info(model, region, version)
            .expect("Info request failed");
    }

    /// Queries the server for firmware binary metadata without panicking when
    /// the request fails or the server returns invalid metadata.
    pub fn try_fetch_binary_info(
        &mut self,
        model: &str,
        region: &str,
        version: &str,
    ) -> Result<()> {
        let mut parts: Vec<&str> = version.split('/').collect();
        if parts.len() == 3 {
            parts.push(parts[0]);
        }
        let fw = parts.join("/");
        let nonce = lock_unpoisoned(&self.auth_state).nonce.clone();
        let req_xml = xml::binary_inform_req_xml(model, region, &fw, &nonce);

        let xml = self
            .make_req(
                "NF_SmartDownloadBinaryInform.do",
                self.make_authorization_value(),
                &req_xml,
            )
            .and_then(Response::text)?;

        self.info = parse_binary_info_response(&xml, model, region, version)?;
        Ok(())
    }

    fn make_authorization_value(&self) -> String {
        let (encnonce, auth) = {
            let state = lock_unpoisoned(&self.auth_state);
            (state.encnonce.clone(), state.auth.clone())
        };
        format!(
            "FUS nonce=\"{}\", signature=\"{}\", nc=\"\", type=\"\", realm=\"\", newauth=\"1\"",
            encnonce, auth
        )
    }

    fn make_history_authorization_value(&self, model: &str) -> String {
        let (client_nonce, signature) = auth::compute_history_headers(model);
        format!(
            "FUS nonce=\"{}\", signature=\"{}\", nc=\"00000001\", type=\"auth\", realm=\"interface\"",
            client_nonce, signature
        )
    }

    fn make_req(&self, path: &str, authorization: String, data: &str) -> reqwest::Result<Response> {
        let url = format!("{}/{}", self.api_base.trim_end_matches('/'), path);
        let resp = self
            .client
            .post(&url)
            // Passing the server-derived nonce through RequestBuilder keeps
            // invalid header bytes as a typed reqwest builder error instead of
            // panicking during HeaderValue construction.
            .header(AUTHORIZATION, authorization)
            .header(USER_AGENT, "SMART 2.0")
            .body(data.to_string())
            .send()?
            .error_for_status()?;

        if let Some(nonce) = resp
            .headers()
            .get("NONCE")
            .or_else(|| resp.headers().get("nonce"))
            .and_then(|n| n.to_str().ok())
        {
            let nonce_str = nonce.to_string();
            if !nonce_str.is_empty() {
                let mut state = lock_unpoisoned(&self.auth_state);
                if nonce_str != state.encnonce {
                    state.encnonce = nonce_str;
                    state.nonce = state.encnonce.clone();
                    state.auth = auth::decrypt_nonce(&state.encnonce);
                }
            }
        }

        Ok(resp)
    }

    /// Fetches the historical list of released firmware versions for the target model and region.
    pub fn fetch_history(&self, model: &str, region: &str) -> reqwest::Result<VersionInfo> {
        let xml = self.fetch_history_xml(model, region)?;
        Ok(xml::parse_history_xml(&xml).expect("Failed to parse history.xml"))
    }

    /// Fetches firmware release history without panicking when the server
    /// returns malformed XML or no matching firmware entries.
    pub fn try_fetch_history(&self, model: &str, region: &str) -> Result<VersionInfo> {
        let xml = self.fetch_history_xml(model, region)?;
        parse_history_response(&xml, model, region)
    }

    fn fetch_history_xml(&self, model: &str, region: &str) -> reqwest::Result<String> {
        let req_xml = xml::history_req_xml(model, region);
        let resp = self.make_req(
            "SmartHistory.do",
            self.make_history_authorization_value(model),
            &req_xml,
        )?;
        resp.text()
    }

    /// Requests a download authorization ticket from FUS for the currently selected firmware file.
    pub fn init_download(&self) -> reqwest::Result<()> {
        let nonce = lock_unpoisoned(&self.auth_state).nonce.clone();
        let init_xml = xml::binary_init_req_xml(
            &self.info.filename,
            &nonce,
            &self.info.version,
            &self.info.model_type,
            &self.info.region,
        );
        self.make_req(
            "NF_SmartDownloadBinaryInitForMass.do",
            self.make_authorization_value(),
            &init_xml,
        )?;
        Ok(())
    }

    /// Fetches a specific byte range or chunk of the firmware binary package, automatically handling re-authentication as needed.
    pub fn download_file(&self, start: Option<u64>, end: Option<u64>) -> reqwest::Result<Response> {
        self.download_response_with_reauth(start, end)?
            .error_for_status()
    }

    fn download_response_with_reauth(
        &self,
        start: Option<u64>,
        end: Option<u64>,
    ) -> reqwest::Result<Response> {
        // Capture the token generation backing this request. If the request is
        // rejected as unauthorized, this lets the refresh tell whether another
        // thread has already rotated the token in the meantime.
        let gen_used = *lock_unpoisoned(&self.reauth_gen);

        let response = self.download_file_once(start, end)?;
        if response.status() == StatusCode::UNAUTHORIZED {
            // The download token expired mid-transfer. Refresh it (at most
            // once per expiry across all threads) and retry the request once
            // with the new token. If it still fails, the caller's retry loop
            // takes over.
            self.reauthenticate(gen_used)?;
            self.download_file_once(start, end)
        } else {
            Ok(response)
        }
    }

    /// Fetches a firmware byte range and validates that the server honored the
    /// requested offsets before exposing the response body to the caller.
    ///
    /// This is the checked counterpart to [`FusClient::download_file`]. A
    /// partial response must include an exact `Content-Range` matching the
    /// requested start, end, and firmware size. A full `200 OK` response is
    /// accepted only for a `0..EOF` request whose content length matches the
    /// firmware metadata.
    pub fn download_file_checked(&self, start: Option<u64>, end: Option<u64>) -> Result<Response> {
        let response = self.download_response_with_reauth(start, end)?;
        if !response.status().is_success() {
            return Err(http_status_error(response.status(), start, end));
        }
        validate_download_response(&response, start, end, self.info.size)?;
        Ok(response)
    }

    fn download_file_once(
        &self,
        start: Option<u64>,
        end: Option<u64>,
    ) -> reqwest::Result<Response> {
        let requested_file = format!("{}{}", self.info.path, self.info.filename);
        let mut request = self
            .client
            .get(&self.download_endpoint)
            .query(&[("file", requested_file)])
            .header(AUTHORIZATION, self.make_authorization_value())
            .header(USER_AGENT, "SMART 2.0");
        request = match (start, end) {
            (Some(s), Some(e)) => request.header(RANGE, format!("bytes={s}-{e}")),
            (None, Some(e)) => request.header(RANGE, format!("bytes=0-{e}")),
            (Some(s), None) => request.header(RANGE, format!("bytes={s}-")),
            _ => request,
        };
        request.send()
    }

    /// Re-establish the session after the auth token expired (HTTP 401),
    /// rewriting the shared `nonce`/`encnonce`/`auth`.
    ///
    /// `gen_seen` is the generation the failed request was signed with. Holding
    /// `reauth_gen` serializes refreshes, and the generation check makes the
    /// refresh idempotent: the first thread to react to an expiry does the work
    /// and bumps the counter, so the others — which were waiting on the same
    /// lock with the same `gen_seen` — simply return and retry with the token
    /// that is now fresh. The new generation is published only on full success,
    /// so a failed refresh leaves the counter untouched and is retried.
    fn reauthenticate(&self, gen_seen: u64) -> reqwest::Result<()> {
        let mut generation = lock_unpoisoned(&self.reauth_gen);
        if *generation != gen_seen {
            return Ok(());
        }
        self.make_req(
            "NF_SmartDownloadGenerateNonce.do",
            self.make_authorization_value(),
            "",
        )?;
        self.init_download()?;
        *generation += 1;
        Ok(())
    }

    /// Instantiates a decryptor after validating the session key length.
    pub fn try_get_decryptor(&self) -> Result<Aes128EcbDec> {
        Aes128EcbDec::new_from_slice(self.info.key.as_slice()).map_err(|_| Error::InvalidResponse {
            message: format!(
                "firmware metadata contained an invalid {}-byte decryption key",
                self.info.key.len()
            ),
        })
    }

    /// Instantiates an AES-128-ECB decryptor for the session.
    ///
    /// Invalid mutable metadata is mapped to an all-zero fallback key rather
    /// than panicking. New code should use [`FusClient::try_get_decryptor`] so
    /// it can report the invalid key directly; [`FusClient::download`] already
    /// does so.
    #[deprecated(note = "use try_get_decryptor to report invalid session metadata")]
    pub fn get_decryptor(&self) -> Aes128EcbDec {
        match self.try_get_decryptor() {
            Ok(decryptor) => decryptor,
            Err(_) => {
                let key: aes::cipher::Key<Aes128EcbDec> = Default::default();
                Aes128EcbDec::new(&key)
            }
        }
    }
}

fn lock_unpoisoned<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn http_status_error(status: StatusCode, start: Option<u64>, end: Option<u64>) -> Error {
    let range_request = start.is_some() || end.is_some();
    let retryable = matches!(
        status,
        StatusCode::REQUEST_TIMEOUT
            | StatusCode::TOO_EARLY
            | StatusCode::TOO_MANY_REQUESTS
            | StatusCode::INTERNAL_SERVER_ERROR
            | StatusCode::BAD_GATEWAY
            | StatusCode::SERVICE_UNAVAILABLE
            | StatusCode::GATEWAY_TIMEOUT
    ) || (range_request && status == StatusCode::NOT_FOUND);
    let range = match (start, end) {
        (Some(start), Some(end)) => format!("bytes {start}-{end}"),
        (Some(start), None) => format!("bytes {start}-EOF"),
        (None, Some(end)) => format!("bytes 0-{end}"),
        (None, None) => "the full file".to_string(),
    };
    Error::HttpStatus {
        status: status.as_u16(),
        retryable,
        message: format!(
            "{} while requesting {range}",
            status.canonical_reason().unwrap_or("unknown status")
        ),
    }
}

fn parse_binary_info_response(
    response: &str,
    model: &str,
    region: &str,
    version: &str,
) -> Result<BinaryInform> {
    BinaryInform::parse(response).ok_or_else(|| Error::InvalidResponse {
        message: format!(
            "firmware metadata was missing or malformed for model {model}, region {region}, and version {version}"
        ),
    })
}

fn parse_history_response(response: &str, model: &str, region: &str) -> Result<VersionInfo> {
    xml::parse_history_xml(response).ok_or_else(|| Error::InvalidResponse {
        message: format!(
            "firmware history was missing or malformed for model {model} and region {region}"
        ),
    })
}

fn validate_download_response(
    response: &Response,
    start: Option<u64>,
    end: Option<u64>,
    total: u64,
) -> Result<()> {
    let content_range = response
        .headers()
        .get(CONTENT_RANGE)
        .and_then(|value| value.to_str().ok());
    validate_download_response_parts(
        response.status(),
        content_range,
        response.content_length(),
        start,
        end,
        total,
    )
}

fn validate_download_response_parts(
    status: StatusCode,
    content_range: Option<&str>,
    content_length: Option<u64>,
    start: Option<u64>,
    end: Option<u64>,
    total: u64,
) -> Result<()> {
    if total == 0 {
        return Err(invalid_download_response(
            "firmware metadata reported a zero-byte download",
        ));
    }

    let expected_start = start.unwrap_or(0);
    let expected_end = end.unwrap_or(total - 1);
    if expected_start > expected_end || expected_end >= total {
        return Err(invalid_download_response(format!(
            "requested byte range {expected_start}-{expected_end} is outside the {total}-byte firmware"
        )));
    }

    if status == StatusCode::PARTIAL_CONTENT {
        let value = content_range.ok_or_else(|| {
            invalid_download_response("partial response omitted the Content-Range header")
        })?;
        let (actual_start, actual_end, actual_total) =
            parse_content_range(value).ok_or_else(|| {
                invalid_download_response(format!("malformed Content-Range header: {value:?}"))
            })?;
        if (actual_start, actual_end, actual_total) != (expected_start, expected_end, total) {
            return Err(invalid_download_response(format!(
                "server returned byte range {actual_start}-{actual_end}/{actual_total}, expected {expected_start}-{expected_end}/{total}"
            )));
        }

        let expected_length = expected_end - expected_start + 1;
        if let Some(actual_length) = content_length
            && actual_length != expected_length
        {
            return Err(invalid_download_response(format!(
                "partial response reported {actual_length} bytes, expected {expected_length}"
            )));
        }
        return Ok(());
    }

    if status == StatusCode::OK && expected_start == 0 && end.is_none() {
        if content_length == Some(total) {
            return Ok(());
        }
        return Err(invalid_download_response(format!(
            "full response length was {:?}, expected {total}",
            content_length
        )));
    }

    Err(invalid_download_response(format!(
        "server returned HTTP {status} for byte range {expected_start}-{expected_end}/{total}"
    )))
}

fn parse_content_range(value: &str) -> Option<(u64, u64, u64)> {
    let mut parts = value.split_whitespace();
    let unit = parts.next()?;
    let range_and_total = parts.next()?;
    if !unit.eq_ignore_ascii_case("bytes") || parts.next().is_some() {
        return None;
    }

    let (range, total) = range_and_total.split_once('/')?;
    let (start, end) = range.split_once('-')?;
    let start = start.parse().ok()?;
    let end = end.parse().ok()?;
    let total = total.parse().ok()?;
    if start > end || end >= total {
        return None;
    }
    Some((start, end, total))
}

fn invalid_download_response(message: impl Into<String>) -> Error {
    Error::InvalidResponse {
        message: message.into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn production_binary_endpoint_uses_https() {
        assert!(FUS_DOWNLOAD_ENDPOINT.starts_with("https://"));
    }

    #[test]
    #[allow(deprecated)]
    fn public_decryptor_api_does_not_panic_on_mutated_key() {
        let info = BinaryInform {
            key: vec![1, 2, 3],
            ..Default::default()
        };
        let client =
            FusClient::new_for_download_test("http://127.0.0.1:1/unreachable".to_string(), info)
                .unwrap();

        assert!(client.try_get_decryptor().is_err());
        assert!(
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let _ = client.get_decryptor();
            }))
            .is_ok()
        );
    }

    #[test]
    fn server_nonce_with_invalid_header_bytes_returns_builder_error() {
        let client = FusClient::new_for_download_test(
            "http://127.0.0.1:1/unreachable".to_string(),
            BinaryInform::default(),
        )
        .unwrap();
        lock_unpoisoned(&client.auth_state).encnonce = "invalid\r\nnonce".to_string();

        let error = client.download_file_once(None, None).unwrap_err();

        assert!(error.is_builder());
    }

    #[test]
    fn http_status_classification_separates_transient_and_permanent_failures() {
        assert!(matches!(
            http_status_error(StatusCode::SERVICE_UNAVAILABLE, Some(0), Some(15)),
            Error::HttpStatus {
                status: 503,
                retryable: true,
                ..
            }
        ));
        assert!(matches!(
            http_status_error(StatusCode::NOT_FOUND, Some(0), Some(15)),
            Error::HttpStatus {
                status: 404,
                retryable: true,
                ..
            }
        ));
        assert!(matches!(
            http_status_error(StatusCode::NOT_FOUND, None, None),
            Error::HttpStatus {
                status: 404,
                retryable: false,
                ..
            }
        ));
    }

    #[test]
    fn binary_info_parser_reports_invalid_response() {
        let error = match parse_binary_info_response(
            "<FUSMsg><FUSBody><Results><Status>500</Status></Results></FUSBody></FUSMsg>",
            "SM-S931U1",
            "XAA",
            "BAD",
        ) {
            Ok(_) => panic!("invalid response unexpectedly parsed"),
            Err(error) => error,
        };

        assert!(matches!(error, Error::InvalidResponse { .. }));
        assert!(error.to_string().contains("SM-S931U1"));
        assert!(error.to_string().contains("XAA"));
    }

    #[test]
    fn binary_info_parser_accepts_complete_metadata() {
        let response = r#"
            <FUSMsg>
              <FUSBody>
                <Results><Status>200</Status></Results>
                <BINARY_SW_VERSION><Data>AAAA/BBBB/CCCC/DDDD</Data></BINARY_SW_VERSION>
                <LOGIC_VALUE_FACTORY><Data>0123456789abcdef</Data></LOGIC_VALUE_FACTORY>
                <BINARY_NAME><Data>firmware.zip.enc4</Data></BINARY_NAME>
                <MODEL_PATH><Data>/path/</Data></MODEL_PATH>
                <BINARY_BYTE_SIZE><Data>4096</Data></BINARY_BYTE_SIZE>
                <DEVICE_MODEL_TYPE><Data>MODEL</Data></DEVICE_MODEL_TYPE>
                <BINARY_LOCAL_CODE><Data>XAA</Data></BINARY_LOCAL_CODE>
              </FUSBody>
            </FUSMsg>
        "#;

        let info = parse_binary_info_response(response, "SM-S931U1", "XAA", "AAAA").unwrap();

        assert_eq!(info.filename, "firmware.zip.enc4");
        assert_eq!(info.size, 4096);
        assert_eq!(info.key.len(), 16);
    }

    #[test]
    fn history_parser_reports_invalid_response() {
        let error = match parse_history_response("not XML", "SM-S931U1", "XAA") {
            Ok(_) => panic!("invalid response unexpectedly parsed"),
            Err(error) => error,
        };

        assert!(matches!(error, Error::InvalidResponse { .. }));
        assert!(error.to_string().contains("firmware history"));
    }

    #[test]
    fn content_range_parser_accepts_complete_byte_ranges() {
        assert_eq!(parse_content_range("bytes 16-31/64"), Some((16, 31, 64)));
        assert_eq!(
            parse_content_range("BYTES 0-4095/4096"),
            Some((0, 4095, 4096))
        );
    }

    #[test]
    fn content_range_parser_rejects_invalid_or_unsatisfied_ranges() {
        assert_eq!(parse_content_range("bytes */64"), None);
        assert_eq!(parse_content_range("bytes 32-16/64"), None);
        assert_eq!(parse_content_range("bytes 0-64/64"), None);
        assert_eq!(parse_content_range("items 0-15/64"), None);
        assert_eq!(parse_content_range("bytes 0-15/*"), None);
    }

    #[test]
    fn checked_partial_response_requires_exact_range_metadata() {
        assert!(
            validate_download_response_parts(
                StatusCode::PARTIAL_CONTENT,
                Some("bytes 16-31/64"),
                Some(16),
                Some(16),
                Some(31),
                64,
            )
            .is_ok()
        );

        for value in ["bytes 0-15/64", "bytes 16-30/64", "bytes 16-31/65"] {
            let error = validate_download_response_parts(
                StatusCode::PARTIAL_CONTENT,
                Some(value),
                Some(16),
                Some(16),
                Some(31),
                64,
            )
            .unwrap_err();
            assert!(matches!(error, Error::InvalidResponse { .. }));
        }
    }

    #[test]
    fn full_response_is_only_valid_for_exact_zero_to_eof_request() {
        assert!(
            validate_download_response_parts(StatusCode::OK, None, Some(64), Some(0), None, 64,)
                .is_ok()
        );

        for (start, end, length) in [
            (Some(16), None, Some(64)),
            (Some(0), Some(63), Some(64)),
            (Some(0), None, Some(63)),
            (Some(0), None, None),
        ] {
            assert!(
                validate_download_response_parts(StatusCode::OK, None, length, start, end, 64,)
                    .is_err()
            );
        }
    }
}
