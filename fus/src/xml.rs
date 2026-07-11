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

use roxmltree::{Document, Node};
use std::collections::{BTreeMap, BTreeSet, HashMap};

/// Escapes untrusted values before placing them in XML element text. Keep all
/// request builders on this one path so server metadata and caller-provided
/// model/region/version values cannot alter the document structure.
fn escape_xml_text(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len());
    for character in value.chars() {
        match character {
            '&' => escaped.push_str("&amp;"),
            '<' => escaped.push_str("&lt;"),
            '>' => escaped.push_str("&gt;"),
            '"' => escaped.push_str("&quot;"),
            '\'' => escaped.push_str("&apos;"),
            _ => escaped.push(character),
        }
    }
    escaped
}

fn get_logic_check(inp: &str, nonce: &str) -> String {
    let mut out = String::new();
    for c in nonce.chars() {
        let idx = (c as u32) & 0xf;
        if let Some(ch) = inp.chars().nth(idx as usize) {
            out.push(ch);
        } else {
            out.push('.');
        }
    }
    out
}

/// Firmware version information parsed from server responses (e.g., version.xml or history.xml).
pub struct VersionInfo {
    /// The latest available stable firmware version string.
    pub latest: String,
    /// A list of previous stable firmware version strings, sorted from newer build to older build.
    pub previous: Vec<String>,
    /// A list of beta firmware version strings, sorted from newer build to older build.
    pub beta: Vec<String>,
}

fn normalize_version(v: &str) -> String {
    let mut parts: Vec<&str> = v.split('/').collect();
    if parts.len() == 3 {
        parts.push(parts[0]);
    }
    if parts.len() >= 3 && parts[2].is_empty() {
        parts[2] = parts[0];
    }
    parts.join("/")
}

pub(crate) fn parse_version_xml(xml: &str) -> Option<VersionInfo> {
    let doc = Document::parse(xml).ok()?;

    let latest_node = doc.descendants().find(|n| n.has_tag_name("latest"))?;
    let latest_text = latest_node.text()?.trim();
    if latest_text.is_empty() {
        return None;
    }

    let latest = normalize_version(latest_text);

    let mut upgrade_entries = Vec::new();
    if let Some(upgrade_node) = doc.descendants().find(|n| n.has_tag_name("upgrade")) {
        for value_node in upgrade_node.children().filter(|n| n.has_tag_name("value")) {
            if let Some(text) = value_node.text() {
                let trimmed = text.trim();
                if !trimmed.is_empty() {
                    let rcount = value_node
                        .attribute("rcount")
                        .and_then(|a| a.parse::<u64>().ok())
                        .unwrap_or(0);
                    let fwsize = value_node
                        .attribute("fwsize")
                        .and_then(|a| a.parse::<u64>().ok())
                        .unwrap_or(0);

                    upgrade_entries.push((rcount, fwsize, normalize_version(trimmed)));
                }
            }
        }
    }

    upgrade_entries.sort_by(|a, b| {
        a.0.cmp(&b.0)
            .then_with(|| a.1.cmp(&b.1))
            .then_with(|| a.2.cmp(&b.2))
    });
    let mut seen = BTreeSet::new();
    let previous: Vec<String> = upgrade_entries
        .into_iter()
        .map(|(_, _, version)| version)
        .filter(|version| seen.insert(version.clone()))
        .collect();

    Some(VersionInfo {
        latest,
        previous,
        beta: Vec::new(),
    })
}

pub(crate) fn binary_inform_req_xml(model: &str, region: &str, fw: &str, nonce: &str) -> String {
    let logic_check = escape_xml_text(&get_logic_check(fw, nonce));
    let fw = escape_xml_text(fw);
    let region = escape_xml_text(region);
    let model = escape_xml_text(model);

    format!(
        r#"<FUSMsg>
<FUSHdr><ProtoVer>1.0</ProtoVer><SessionID>0</SessionID><MsgID>1</MsgID></FUSHdr>
<FUSBody>
    <Put>
        <CmdID>1</CmdID>
        <ACCESS_MODE><Data>1</Data></ACCESS_MODE>
        <BINARY_NATURE><Data>1</Data></BINARY_NATURE>
        <REQUEST_TYPE><Data>2</Data></REQUEST_TYPE>
        <LOGIC_CHECK><Data>{logic_check}</Data></LOGIC_CHECK>
        <BINARY_SW_VERSION><Data>{fw}</Data></BINARY_SW_VERSION>
        <BINARY_LOCAL_CODE><Data>{region}</Data></BINARY_LOCAL_CODE>
        <BINARY_MODEL_NAME><Data>{model}</Data></BINARY_MODEL_NAME>
    </Put>
    <Get>
        <CmdID>2</CmdID>
        <BINARY_SW_VERSION></BINARY_SW_VERSION>
    </Get>
</FUSBody>
</FUSMsg>"#
    )
}

pub(crate) fn binary_init_req_xml(
    filename: &str,
    nonce: &str,
    fw: &str,
    model_type: &str,
    region: &str,
) -> String {
    // Work in characters rather than byte indexes. FUS normally supplies an
    // ASCII filename, but malformed/non-ASCII metadata must never panic at a
    // UTF-8 boundary while constructing the authorization request.
    let filename_chars: Vec<char> = filename.chars().collect();
    let start = filename_chars.len().saturating_sub(25);
    let end = filename_chars.len().saturating_sub(9);
    let checkinp: String = filename_chars[start..end].iter().collect();

    let filename = escape_xml_text(filename);
    let fw = escape_xml_text(fw);
    let region = escape_xml_text(region);
    let model_type = escape_xml_text(model_type);
    let logic_check = escape_xml_text(&get_logic_check(&checkinp, nonce));

    format!(
        r#"<FUSMsg>
<FUSHdr><ProtoVer>1.0</ProtoVer><SessionID>0</SessionID><MsgID>1</MsgID></FUSHdr>
<FUSBody>
    <Put>
        <BINARY_NAME><Data>{filename}</Data></BINARY_NAME>
        <BINARY_SW_VERSION><Data>{fw}</Data></BINARY_SW_VERSION>
        <DEVICE_LOCAL_CODE><Data>{region}</Data></DEVICE_LOCAL_CODE>
        <DEVICE_MODEL_TYPE><Data>{model_type}</Data></DEVICE_MODEL_TYPE>
        <LOGIC_CHECK><Data>{logic_check}</Data></LOGIC_CHECK>
    </Put>
</FUSBody>
</FUSMsg>"#
    )
}

pub(crate) fn history_req_xml(model: &str, region: &str) -> String {
    let model = escape_xml_text(model);
    let region = escape_xml_text(region);
    format!(
        r#"<FUSMsg>
<FUSHdr><ProtoVer>1</ProtoVer><SessionID>0</SessionID><MsgID>1</MsgID></FUSHdr>
<FUSBody>
    <Put>
        <CmdID>1</CmdID>
        <ACCESS_MODE><Data>1</Data></ACCESS_MODE>
        <BINARY_LOCAL_CODE><Data>{region}</Data></BINARY_LOCAL_CODE>
        <BINARY_MODEL_NAME><Data>{model}</Data></BINARY_MODEL_NAME>
    </Put>
</FUSBody>
</FUSMsg>"#
    )
}

fn parse_xml_data(xml: &str) -> Option<HashMap<String, String>> {
    let doc = Document::parse(xml).ok()?;

    let status_str = doc
        .root_element()
        .children()
        .find(|n| n.has_tag_name("FUSBody"))?
        .children()
        .find(|n| n.has_tag_name("Results"))?
        .children()
        .find(|n| n.has_tag_name("Status"))?
        .text()?;

    if status_str != "200" && status_str != "S00" {
        return None;
    }

    Some(get_xml_node_data(doc.root()))
}

fn get_xml_node_data(node: Node) -> HashMap<String, String> {
    let mut kv = HashMap::new();
    node.descendants()
        .filter(|n| n.has_tag_name("Data"))
        .for_each(|n| {
            if let Some(v) = n.text()
                && let Some(parent) = n.parent()
                && parent.is_element()
            {
                kv.insert(parent.tag_name().name().to_string(), v.to_string());
            }
        });
    kv
}

/// Detailed information about a firmware binary package.
#[derive(Default, Clone)]
pub struct BinaryInform {
    /// The firmware build version identifier (e.g., PDA/CSC/PHONE/PHONE).
    pub version: String,
    /// The actual name of the firmware file on the server.
    pub filename: String,
    /// The relative URL path on the server to download the firmware.
    pub path: String,
    /// The size of the firmware binary package in bytes.
    pub size: u64,
    /// The 128-bit key used for AES-128 decryption.
    pub key: Vec<u8>,
    /// The device model type classification string.
    pub model_type: String,
    /// The local CSC or sales region code.
    pub region: String,
}

impl BinaryInform {
    pub(crate) fn parse(xml: &str) -> Option<BinaryInform> {
        let mut kv = parse_xml_data(xml)?;
        let fw_ver = kv
            .remove("BINARY_SW_VERSION")
            .or_else(|| kv.remove("LATEST_FW_VERSION"))?;
        let logic_val = kv
            .remove("LOGIC_VALUE_FACTORY")
            .or_else(|| kv.remove("LOGIC_VALUE_HOME"))?;
        let key = get_logic_check(&fw_ver, &logic_val);

        Some(Self {
            version: fw_ver,
            filename: kv.remove("BINARY_NAME")?,
            path: kv.remove("MODEL_PATH")?,
            size: kv.remove("BINARY_BYTE_SIZE")?.parse().ok()?,
            key: fast_md5::digest(key.as_bytes()).to_vec(),
            model_type: kv.remove("DEVICE_MODEL_TYPE")?,
            region: kv.remove("BINARY_LOCAL_CODE")?,
        })
    }
}

struct BinaryInfo {
    sw_version: String,
    index: String,
    sequence: i64,
    open_date: String,
}

impl BinaryInfo {
    pub(crate) fn parse(mut kv: HashMap<String, String>) -> Option<Self> {
        Some(Self {
            sw_version: kv.remove("BINARY_SW_VERSION")?,
            index: kv.remove("BINARY_INDEX")?,
            sequence: kv.remove("BINARY_SEQUENCE")?.parse().ok()?,
            open_date: kv.remove("BINARY_OPEN_DATE").unwrap_or_default(),
        })
    }
}

pub(crate) fn parse_history_xml(xml: &str) -> Option<VersionInfo> {
    let doc = Document::parse(xml).ok()?;
    let mut consolidated: BTreeMap<(String, String, i64), BinaryInfo> = BTreeMap::new();

    for node in doc.descendants().filter(|n| n.has_tag_name("BINARY_INFO")) {
        let kv = get_xml_node_data(node);
        if let Some(entry) = BinaryInfo::parse(kv) {
            let key = (
                entry.sw_version.clone(),
                entry.index.clone(),
                entry.sequence,
            );
            consolidated
                .entry(key)
                .and_modify(|existing| {
                    if entry.open_date > existing.open_date {
                        existing.open_date = entry.open_date.clone();
                    }
                })
                .or_insert(entry);
        }
    }

    if consolidated.is_empty() {
        return None;
    }

    // Chronological Sorting (Newest First)
    let mut sorted_entries: Vec<BinaryInfo> = consolidated.into_values().collect();
    sorted_entries.sort_by(|a, b| {
        b.sequence
            .cmp(&a.sequence)
            .then_with(|| b.open_date.cmp(&a.open_date))
            .then_with(|| a.index.cmp(&b.index))
            .then_with(|| a.sw_version.cmp(&b.sw_version))
    });

    let (stable_entries, beta_entries): (Vec<BinaryInfo>, Vec<BinaryInfo>) =
        sorted_entries.into_iter().partition(|e| e.index != "90");

    let mut seen_stable = BTreeSet::new();
    let mut previous: Vec<String> = stable_entries
        .into_iter()
        .map(|e| normalize_version(&e.sw_version))
        .filter(|version| seen_stable.insert(version.clone()))
        .collect();

    let mut seen_beta = BTreeSet::new();
    let beta: Vec<String> = beta_entries
        .into_iter()
        .map(|e| normalize_version(&e.sw_version))
        .filter(|version| seen_beta.insert(version.clone()))
        .collect();

    if previous.is_empty() {
        return None;
    }

    let latest = previous.remove(0);

    Some(VersionInfo {
        latest,
        previous,
        beta,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_version_xml() {
        let xml_content = include_str!("../../test-data/version.xml");

        let info = parse_version_xml(xml_content).unwrap();
        assert_eq!(
            info.latest,
            "S931U1UESACZE1/S931U1OYMACZE1/S931U1UESACZE1/S931U1UESACZE1"
        );
        assert_eq!(info.previous.len(), 15);
        assert_eq!(
            info.previous[0],
            "S931U1UES8BZBB/S931U1OYM8BZBB/S931U1UES8BZBB/S931U1UES8BZBB"
        );
        assert_eq!(
            info.previous[13],
            "S931U1UEU1AYB3/S931U1OYM1AYB3/S931U1UEU1AYB3/S931U1UEU1AYB3"
        );
        assert_eq!(
            info.previous[14],
            "S931U1UEU1AYA1/S931U1OYM1AYA1/S931U1UEU1AYA1/S931U1UEU1AYA1"
        );
        assert!(info.beta.is_empty());
    }

    #[test]
    fn test_parse_history_xml() {
        let xml_content = include_str!("../../test-data/history.xml");

        let info = parse_history_xml(xml_content).unwrap();
        assert_eq!(
            info.latest,
            "S931U1UEUACZF1/S931U1OYMACZF1/S931U1UEUACZF1/S931U1UEUACZF1"
        );
        assert_eq!(info.previous.len(), 18);
        assert_eq!(
            info.previous[17],
            "S931U1UEU1AYA1/S931U1OYM1AYA1/S931U1UEU1AYA1/S931U1UEU1AYA1"
        );
        assert_eq!(
            info.previous[16],
            "S931U1UEU1AYB3/S931U1OYM1AYB3/S931U1UEU1AYB3/S931U1UEU1AYB3"
        );
        assert_eq!(
            info.previous[1],
            "S931U1UEU9CZDP/S931U1OYM9CZDP/S931U1UEU9CZDP/S931U1UEU9CZDP"
        );
        assert_eq!(info.beta.len(), 11);
        assert_eq!(
            info.beta[10],
            "S931U1UEU7ZYL8/S931U1OYM7ZYL8/S931U1UEU7CYL8/S931U1UEU7ZYL8"
        );
        assert_eq!(
            info.beta[0],
            "S931U1UES9BZBH/S931U1OYM9BZBH/S931U1UES9BZBH/S931U1UES9BZBHZ"
        );
    }

    #[test]
    fn binary_init_request_is_safe_for_short_and_non_ascii_filenames() {
        for filename in ["x", "固件.zip.enc4", "éééééééééééééééééééééééé.enc4"]
        {
            let request = binary_init_req_xml(filename, "nonce", "A/B/C/D", "MODEL", "XAA");
            assert!(request.contains(filename));
            assert!(request.contains("<LOGIC_CHECK><Data>"));
        }
    }

    #[test]
    fn request_builders_escape_all_interpolated_xml_text() {
        const SPECIAL: &str = "<&>\"'";
        let model = format!("SM-{SPECIAL}");
        let region = format!("X{SPECIAL}");
        let firmware = format!("A{SPECIAL}/B/C/D");
        let filename = format!("firmware-{SPECIAL}-0123456789abcdef.zip.enc4");

        let inform = binary_inform_req_xml(&model, &region, &firmware, "0123456789abcdef");
        assert_eq!(request_data(&inform, "BINARY_MODEL_NAME"), model);
        assert_eq!(request_data(&inform, "BINARY_LOCAL_CODE"), region);
        assert_eq!(request_data(&inform, "BINARY_SW_VERSION"), firmware);

        let init = binary_init_req_xml(&filename, "0123456789abcdef", &firmware, &model, &region);
        assert_eq!(request_data(&init, "BINARY_NAME"), filename);
        assert_eq!(request_data(&init, "BINARY_SW_VERSION"), firmware);
        assert_eq!(request_data(&init, "DEVICE_LOCAL_CODE"), region);
        assert_eq!(request_data(&init, "DEVICE_MODEL_TYPE"), model);

        let history = history_req_xml(&model, &region);
        assert_eq!(request_data(&history, "BINARY_MODEL_NAME"), model);
        assert_eq!(request_data(&history, "BINARY_LOCAL_CODE"), region);

        for request in [&inform, &init, &history] {
            assert!(request.contains("&lt;"));
            assert!(request.contains("&amp;"));
            assert!(request.contains("&gt;"));
            assert!(request.contains("&quot;"));
            assert!(request.contains("&apos;"));
            Document::parse(request).unwrap();
        }
    }

    fn request_data(request: &str, element: &str) -> String {
        let document = Document::parse(request).unwrap();
        document
            .descendants()
            .find(|node| node.has_tag_name(element))
            .and_then(|node| node.children().find(|child| child.has_tag_name("Data")))
            .and_then(|node| node.text())
            .unwrap()
            .to_string()
    }

    #[test]
    fn history_deduplication_is_global_and_deterministic() {
        let xml = r#"
            <FUSMsg><FUSBody>
              <BINARY_INFO>
                <BINARY_SW_VERSION><Data>NEW/CSC/MODEM</Data></BINARY_SW_VERSION>
                <BINARY_INDEX><Data>00</Data></BINARY_INDEX>
                <BINARY_SEQUENCE><Data>30</Data></BINARY_SEQUENCE>
                <BINARY_OPEN_DATE><Data>20260303</Data></BINARY_OPEN_DATE>
              </BINARY_INFO>
              <BINARY_INFO>
                <BINARY_SW_VERSION><Data>OLD/CSC/MODEM</Data></BINARY_SW_VERSION>
                <BINARY_INDEX><Data>00</Data></BINARY_INDEX>
                <BINARY_SEQUENCE><Data>20</Data></BINARY_SEQUENCE>
                <BINARY_OPEN_DATE><Data>20260202</Data></BINARY_OPEN_DATE>
              </BINARY_INFO>
              <BINARY_INFO>
                <BINARY_SW_VERSION><Data>NEW/CSC/MODEM</Data></BINARY_SW_VERSION>
                <BINARY_INDEX><Data>01</Data></BINARY_INDEX>
                <BINARY_SEQUENCE><Data>10</Data></BINARY_SEQUENCE>
                <BINARY_OPEN_DATE><Data>20260101</Data></BINARY_OPEN_DATE>
              </BINARY_INFO>
              <BINARY_INFO>
                <BINARY_SW_VERSION><Data>BETA/CSC/MODEM</Data></BINARY_SW_VERSION>
                <BINARY_INDEX><Data>90</Data></BINARY_INDEX>
                <BINARY_SEQUENCE><Data>9</Data></BINARY_SEQUENCE>
                <BINARY_OPEN_DATE><Data>20260102</Data></BINARY_OPEN_DATE>
              </BINARY_INFO>
              <BINARY_INFO>
                <BINARY_SW_VERSION><Data>BETA/CSC/MODEM</Data></BINARY_SW_VERSION>
                <BINARY_INDEX><Data>90</Data></BINARY_INDEX>
                <BINARY_SEQUENCE><Data>8</Data></BINARY_SEQUENCE>
                <BINARY_OPEN_DATE><Data>20260101</Data></BINARY_OPEN_DATE>
              </BINARY_INFO>
            </FUSBody></FUSMsg>
        "#;

        for _ in 0..20 {
            let info = parse_history_xml(xml).unwrap();
            assert_eq!(info.latest, "NEW/CSC/MODEM/NEW");
            assert_eq!(info.previous, ["OLD/CSC/MODEM/OLD"]);
            assert_eq!(info.beta, ["BETA/CSC/MODEM/BETA"]);
        }
    }
}
