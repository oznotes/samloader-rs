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

//! Crate for interacting with Samsung Firmware Update Server (FUS) to query
//! and download device firmware binaries.

#![deny(missing_docs)]

mod auth;
mod download;
mod error;
mod fusclient;
mod http;
mod xml;

pub use download::{DownloadProgress, MAX_DOWNLOAD_THREADS};
pub use error::{Error, Result};
pub use fusclient::{Aes128EcbDec, FusClient};
pub use xml::{BinaryInform, VersionInfo};

// Re-export public dependencies to avoid type mismatch and SemVer issues in public APIs.
pub use aes;
pub use ecb;
pub use reqwest;

const VERSION_XML_BASE_URL: &str = "https://fota-cloud-dn.ospserver.net:443/firmware/";

/// Queries and fetches the standard firmware `version.xml` document for the specified device model and region.
pub fn fetch_version_xml(model: &str, region: &str) -> reqwest::Result<VersionInfo> {
    let version_xml = fetch_version_xml_text(model, region)?;
    Ok(xml::parse_version_xml(&version_xml).expect("Failed to parse version.xml"))
}

/// Queries the standard firmware `version.xml` document without panicking when
/// the server returns malformed XML or no matching firmware version.
pub fn try_fetch_version_xml(model: &str, region: &str) -> Result<VersionInfo> {
    let version_xml = fetch_version_xml_text(model, region)?;
    parse_version_xml_response(&version_xml, model, region)
}

fn parse_version_xml_response(xml: &str, model: &str, region: &str) -> Result<VersionInfo> {
    xml::parse_version_xml(xml).ok_or_else(|| Error::InvalidResponse {
        message: format!(
            "firmware version information was missing or malformed for model {model} and region {region}"
        ),
    })
}

fn fetch_version_xml_text(model: &str, region: &str) -> reqwest::Result<String> {
    let version_url = version_xml_url(model, region);
    let client = http::client_builder().build()?;
    let version_xml = client
        .get(version_url)
        .header(reqwest::header::USER_AGENT, "Kies2.0_FUS")
        .send()?
        .text()?;

    Ok(version_xml)
}

fn version_xml_url(model: &str, region: &str) -> reqwest::Url {
    let mut url = reqwest::Url::parse(VERSION_XML_BASE_URL)
        .expect("the built-in version.xml base URL must remain valid");
    url.path_segments_mut()
        .expect("the built-in version.xml base URL must support path segments")
        .pop_if_empty()
        .push(region)
        .push(model)
        .push("version.xml");
    url
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_xml_parser_reports_invalid_response() {
        let error = match parse_version_xml_response("not XML", "SM-S931U1", "XAA") {
            Ok(_) => panic!("invalid response unexpectedly parsed"),
            Err(error) => error,
        };

        assert!(matches!(error, Error::InvalidResponse { .. }));
        assert!(error.to_string().contains("firmware version information"));
    }

    #[test]
    fn version_xml_inputs_are_encoded_as_single_path_segments() {
        let url = version_xml_url("SM/../MODEL?query#fragment%", "../XAA?region#fragment%");

        assert_eq!(url.scheme(), "https");
        assert_eq!(url.host_str(), Some("fota-cloud-dn.ospserver.net"));
        assert!(url.query().is_none());
        assert!(url.fragment().is_none());
        assert_eq!(
            url.path(),
            "/firmware/..%2FXAA%3Fregion%23fragment%25/SM%2F..%2FMODEL%3Fquery%23fragment%25/version.xml"
        );
    }
}
