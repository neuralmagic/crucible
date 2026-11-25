//! A passing run's result, read from its declared artifact's zip.

use std::io::Read as _;

use crucible_contract::calls::{FileRef, GITHUB_RESULT_FILE, GithubOutputs, MAX_FILE_PARAM_BYTES};
use crucible_contract::emits::{FieldType, value_type};
use serde_json::{Map, Value};
use sha2::{Digest as _, Sha256};

/// One file of a passing run's result, with its bytes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResultFile {
    pub file: FileRef,
    pub bytes: Vec<u8>,
}

/// The result fields and files `outputs` declares, read from the artifact's zip. `Err` is the
/// note a call fails with.
pub fn read_result(
    zip: &[u8],
    outputs: &GithubOutputs,
) -> Result<(Value, Vec<ResultFile>), String> {
    let artifact = &outputs.artifact;
    let entries = zip_entries(zip).map_err(|e| format!("artifact {artifact:?}: {e}"))?;
    let find = |path: &str| {
        let want = path.trim_start_matches("./");
        entries
            .iter()
            .find(|e| e.name.trim_start_matches("./") == want)
    };
    let mut result = Map::new();
    match find(GITHUB_RESULT_FILE) {
        Some(entry) => {
            let bytes = zip_read(zip, entry)
                .map_err(|e| format!("artifact {artifact:?} {GITHUB_RESULT_FILE}: {e}"))?;
            let object = match serde_json::from_slice::<Value>(&bytes) {
                Ok(Value::Object(object)) => object,
                Ok(other) => {
                    return Err(format!(
                        "artifact {artifact:?} {GITHUB_RESULT_FILE} is {}, not an object",
                        value_type(&other)
                    ));
                }
                Err(e) => {
                    return Err(format!(
                        "artifact {artifact:?} {GITHUB_RESULT_FILE} is not JSON: {e}"
                    ));
                }
            };
            for (field, ty) in &outputs.fields {
                let value = object.get(field).ok_or_else(|| {
                    format!("artifact {artifact:?} {GITHUB_RESULT_FILE} has no field {field:?}")
                })?;
                if let Some(why) = field_refusal(ty, value) {
                    return Err(format!(
                        "artifact {artifact:?} {GITHUB_RESULT_FILE} field {field:?}: {why}"
                    ));
                }
                result.insert(field.clone(), value.clone());
            }
        }
        None if outputs.fields.is_empty() => {}
        None => {
            return Err(format!("artifact {artifact:?} has no {GITHUB_RESULT_FILE}"));
        }
    }
    let mut files = Vec::new();
    for path in &outputs.files {
        let entry =
            find(path).ok_or_else(|| format!("artifact {artifact:?} has no file {path:?}"))?;
        let bytes =
            zip_read(zip, entry).map_err(|e| format!("artifact {artifact:?} {path}: {e}"))?;
        files.push(ResultFile {
            file: FileRef {
                digest: format!("sha256:{:x}", Sha256::digest(&bytes)),
                size: bytes.len() as u64,
                path: Some(path.clone()),
                media_type: None,
            },
            bytes,
        });
    }
    Ok((Value::Object(result), files))
}

fn field_refusal(ty: &FieldType, value: &Value) -> Option<String> {
    if let Some(why) = ty.link_refusal(value) {
        return Some(why);
    }
    if let FieldType::Schema(schema) = ty {
        return schema.refusal(value);
    }
    match ty.admits(value) {
        true => None,
        false => Some(format!("expected {ty}, got {}", value_type(value))),
    }
}

/// One file of a zip archive, from its central directory.
#[derive(Clone, Debug, PartialEq, Eq)]
struct ZipEntry {
    name: String,
    method: u16,
    crc: u32,
    compressed: u64,
    size: u64,
    local_offset: u64,
}

const EOCD_SIG: u32 = 0x0605_4b50;
const CENTRAL_SIG: u32 = 0x0201_4b50;
const LOCAL_SIG: u32 = 0x0403_4b50;
const EOCD_LEN: usize = 22;

fn le16(bytes: &[u8], at: usize) -> Result<u16, String> {
    bytes
        .get(at..at + 2)
        .and_then(|b| b.try_into().ok())
        .map(u16::from_le_bytes)
        .ok_or_else(|| "the zip is truncated".to_owned())
}

fn le32(bytes: &[u8], at: usize) -> Result<u32, String> {
    bytes
        .get(at..at + 4)
        .and_then(|b| b.try_into().ok())
        .map(u32::from_le_bytes)
        .ok_or_else(|| "the zip is truncated".to_owned())
}

fn zip_entries(zip: &[u8]) -> Result<Vec<ZipEntry>, String> {
    let last = zip
        .len()
        .checked_sub(EOCD_LEN)
        .ok_or_else(|| "not a zip archive".to_owned())?;
    let first = last.saturating_sub(usize::from(u16::MAX));
    let eocd = (first..=last)
        .rev()
        .find(|&at| le32(zip, at).is_ok_and(|sig| sig == EOCD_SIG))
        .ok_or_else(|| "not a zip archive".to_owned())?;
    let count = le16(zip, eocd + 10)?;
    let offset = le32(zip, eocd + 16)?;
    if count == u16::MAX || offset == u32::MAX {
        return Err("zip64 archives are not supported".to_owned());
    }
    let mut at = offset as usize;
    let mut entries = Vec::with_capacity(usize::from(count));
    for _ in 0..count {
        if le32(zip, at)? != CENTRAL_SIG {
            return Err("the zip's central directory is corrupt".to_owned());
        }
        let flags = le16(zip, at + 8)?;
        let compressed = le32(zip, at + 20)?;
        let size = le32(zip, at + 24)?;
        let local_offset = le32(zip, at + 42)?;
        if flags & 1 != 0 {
            return Err("encrypted zip entries are not supported".to_owned());
        }
        if [compressed, size, local_offset].contains(&u32::MAX) {
            return Err("zip64 archives are not supported".to_owned());
        }
        let name_len = usize::from(le16(zip, at + 28)?);
        let extra_len = usize::from(le16(zip, at + 30)?);
        let comment_len = usize::from(le16(zip, at + 32)?);
        let name = zip
            .get(at + 46..at + 46 + name_len)
            .ok_or_else(|| "the zip is truncated".to_owned())?;
        entries.push(ZipEntry {
            name: String::from_utf8_lossy(name).into_owned(),
            method: le16(zip, at + 10)?,
            crc: le32(zip, at + 16)?,
            compressed: u64::from(compressed),
            size: u64::from(size),
            local_offset: u64::from(local_offset),
        });
        at += 46 + name_len + extra_len + comment_len;
    }
    Ok(entries)
}

fn zip_read(zip: &[u8], entry: &ZipEntry) -> Result<Vec<u8>, String> {
    if entry.size > MAX_FILE_PARAM_BYTES {
        return Err(format!(
            "{} bytes is over the {MAX_FILE_PARAM_BYTES} byte limit",
            entry.size
        ));
    }
    let local = usize::try_from(entry.local_offset).map_err(|e| e.to_string())?;
    if le32(zip, local)? != LOCAL_SIG {
        return Err("the zip's local header is corrupt".to_owned());
    }
    let start =
        local + 30 + usize::from(le16(zip, local + 26)?) + usize::from(le16(zip, local + 28)?);
    let len = usize::try_from(entry.compressed).map_err(|e| e.to_string())?;
    let data = zip
        .get(start..start + len)
        .ok_or_else(|| "the zip is truncated".to_owned())?;
    let bytes = match entry.method {
        0 => data.to_vec(),
        8 => {
            let mut out = Vec::new();
            flate2::read::DeflateDecoder::new(data)
                .take(entry.size + 1)
                .read_to_end(&mut out)
                .map_err(|e| format!("inflating: {e}"))?;
            out
        }
        other => return Err(format!("zip compression method {other} is not supported")),
    };
    if bytes.len() as u64 != entry.size {
        return Err("its size does not match the zip directory".to_owned());
    }
    let mut crc = flate2::Crc::new();
    crc.update(&bytes);
    if crc.sum() != entry.crc {
        return Err("its checksum does not match".to_owned());
    }
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::io::Write as _;

    use crucible_contract::calls::GithubOutputs;
    use crucible_contract::emits::{FieldType, JsonSchema};
    use serde_json::json;

    use crate::calls::github::artifact::read_result;

    fn outputs() -> GithubOutputs {
        GithubOutputs {
            artifact: "crucible-result".to_owned(),
            fields: BTreeMap::from([
                ("summary".to_owned(), FieldType::String),
                ("legs".to_owned(), FieldType::Integer),
                ("loud".to_owned(), FieldType::Boolean),
            ]),
            files: vec!["notes.md".to_owned()],
        }
    }

    /// A zip as actions/upload-artifact writes it: deflated entries with a data descriptor, and
    /// stored ones without.
    fn zip(entries: &[(&str, &[u8], bool)]) -> Vec<u8> {
        let mut out = Vec::new();
        let mut central = Vec::new();
        for (name, bytes, deflate) in entries {
            let mut crc = flate2::Crc::new();
            crc.update(bytes);
            let crc = crc.sum();
            let data = match deflate {
                true => {
                    let mut enc = flate2::write::DeflateEncoder::new(
                        Vec::new(),
                        flate2::Compression::default(),
                    );
                    enc.write_all(bytes).unwrap();
                    enc.finish().unwrap()
                }
                false => bytes.to_vec(),
            };
            let (flags, method): (u16, u16) = if *deflate { (0x08, 8) } else { (0, 0) };
            let offset = out.len() as u32;
            let header_crc = if *deflate { 0 } else { crc };
            let header_sizes = if *deflate {
                (0, 0)
            } else {
                (data.len() as u32, bytes.len() as u32)
            };
            out.extend(0x0403_4b50u32.to_le_bytes());
            out.extend(20u16.to_le_bytes());
            out.extend(flags.to_le_bytes());
            out.extend(method.to_le_bytes());
            out.extend([0u8; 4]);
            out.extend(header_crc.to_le_bytes());
            out.extend(header_sizes.0.to_le_bytes());
            out.extend(header_sizes.1.to_le_bytes());
            out.extend((name.len() as u16).to_le_bytes());
            out.extend(0u16.to_le_bytes());
            out.extend(name.as_bytes());
            out.extend(&data);
            if *deflate {
                out.extend(0x0807_4b50u32.to_le_bytes());
                out.extend(crc.to_le_bytes());
                out.extend((data.len() as u32).to_le_bytes());
                out.extend((bytes.len() as u32).to_le_bytes());
            }
            central.extend(0x0201_4b50u32.to_le_bytes());
            central.extend(20u16.to_le_bytes());
            central.extend(20u16.to_le_bytes());
            central.extend(flags.to_le_bytes());
            central.extend(method.to_le_bytes());
            central.extend([0u8; 4]);
            central.extend(crc.to_le_bytes());
            central.extend((data.len() as u32).to_le_bytes());
            central.extend((bytes.len() as u32).to_le_bytes());
            central.extend((name.len() as u16).to_le_bytes());
            central.extend([0u8; 12]);
            central.extend(offset.to_le_bytes());
            central.extend(name.as_bytes());
        }
        let cd_offset = out.len() as u32;
        let count = entries.len() as u16;
        out.extend(&central);
        out.extend(0x0605_4b50u32.to_le_bytes());
        out.extend([0u8; 4]);
        out.extend(count.to_le_bytes());
        out.extend(count.to_le_bytes());
        out.extend((central.len() as u32).to_le_bytes());
        out.extend(cd_offset.to_le_bytes());
        out.extend(0u16.to_le_bytes());
        out
    }

    const RESULT: &[u8] =
        br#"{"summary": "fast build of 'hi'", "legs": 2, "loud": false, "extra": "ignored"}"#;

    #[test]
    fn the_result_is_read_from_the_artifact() {
        let notes = b"# notes\n".repeat(200);
        let archive = zip(&[("result.json", RESULT, true), ("notes.md", &notes, false)]);
        let (result, files) = read_result(&archive, &outputs()).unwrap();
        assert_eq!(
            result,
            json!({"summary": "fast build of 'hi'", "legs": 2, "loud": false})
        );
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].bytes, notes);
        assert_eq!(files[0].file.size, notes.len() as u64);
        assert_eq!(files[0].file.path.as_deref(), Some("notes.md"));
        assert_eq!(
            files[0].file.digest,
            format!(
                "sha256:{:x}",
                <sha2::Sha256 as sha2::Digest>::digest(&notes)
            )
        );
    }

    #[test]
    fn a_result_that_breaks_its_declaration_names_what_is_wrong() {
        let notes: &[u8] = b"n";
        let fails = |entries: &[(&str, &[u8], bool)], outputs: &GithubOutputs| {
            read_result(&zip(entries), outputs).unwrap_err()
        };
        let out = outputs();
        assert!(fails(&[("notes.md", notes, false)], &out).contains("has no result.json"));
        assert!(fails(&[("result.json", RESULT, true)], &out).contains("no file \"notes.md\""));
        assert!(
            fails(
                &[
                    ("result.json", br#"{"legs": 2, "loud": true}"#, true),
                    ("notes.md", notes, false)
                ],
                &out
            )
            .contains("no field \"summary\"")
        );
        assert!(
            fails(
                &[
                    (
                        "result.json",
                        br#"{"summary": "s", "legs": 2.5, "loud": true}"#,
                        true
                    ),
                    ("notes.md", notes, false)
                ],
                &out
            )
            .contains("field \"legs\": expected integer, got number")
        );
        assert!(
            fails(
                &[("result.json", b"[1]", false), ("notes.md", notes, false)],
                &out
            )
            .contains("not an object")
        );
        assert!(
            fails(
                &[("result.json", b"{", false), ("notes.md", notes, false)],
                &out
            )
            .contains("not JSON")
        );
        let mut schema_out = out.clone();
        schema_out.fields = BTreeMap::from([(
            "summary".to_owned(),
            FieldType::Schema(JsonSchema::new(json!({"type": "string", "maxLength": 3})).unwrap()),
        )]);
        schema_out.files.clear();
        assert!(fails(&[("result.json", RESULT, true)], &schema_out).contains("field \"summary\""));
        assert!(
            read_result(b"not a zip at all, honest", &out)
                .unwrap_err()
                .contains("not a zip")
        );
    }

    #[test]
    fn a_corrupt_entry_is_refused() {
        let mut archive = zip(&[("result.json", RESULT, false)]);
        let at = 30 + "result.json".len() + 2;
        archive[at] ^= 0xff;
        let mut fields_only = outputs();
        fields_only.files.clear();
        assert!(
            read_result(&archive, &fields_only)
                .unwrap_err()
                .contains("checksum")
        );
    }

    #[test]
    fn an_artifact_with_nothing_declared_passes_empty_and_paths_may_start_with_dot() {
        let none = GithubOutputs {
            artifact: "a".to_owned(),
            fields: BTreeMap::new(),
            files: vec!["./out/x.txt".to_owned()],
        };
        let (result, files) = read_result(&zip(&[("out/x.txt", b"x", true)]), &none).unwrap();
        assert_eq!(result, json!({}));
        assert_eq!(files[0].bytes, b"x");
    }
}
