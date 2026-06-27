// SPDX-FileCopyrightText: 2026 Artifact Depot Contributors
//
// SPDX-License-Identifier: Apache-2.0

use std::collections::HashMap;
use std::io::Read;

use depot_core::error::{self, DepotError};

/// Parsed control file fields from a `.deb` package.
#[derive(Debug, Clone)]
pub struct DebControl {
    pub package: String,
    pub version: String,
    pub architecture: String,
    pub maintainer: String,
    pub depends: String,
    pub section: String,
    pub priority: String,
    pub installed_size: String,
    pub description: String,
    pub full_control: String,
}

/// Pool path prefix for a package name: first letter, or "lib" + first letter for lib* packages.
pub fn pool_prefix(package: &str) -> String {
    if package.starts_with("lib") && package.len() > 3 {
        format!("lib{}", package.get(3..4).unwrap_or_default())
    } else {
        package.get(..1).unwrap_or("_").to_string()
    }
}

/// Build the KV path for a .deb in the pool.
pub fn pool_path(component: &str, package: &str, filename: &str) -> String {
    let prefix = pool_prefix(package);
    format!("pool/{component}/{prefix}/{package}/{filename}")
}

/// Parse a `.deb` file (ar archive) and extract the control file.
pub fn parse_deb(reader: impl std::io::Read) -> error::Result<DebControl> {
    let mut archive = ar::Archive::new(reader);

    while let Some(entry_result) = archive.next_entry() {
        let mut entry = entry_result?;
        let name = std::str::from_utf8(entry.header().identifier())
            .map_err(|e| {
                DepotError::BadRequest(format!("invalid UTF-8 in archive entry name: {e}"))
            })?
            .to_string();

        if name.starts_with("control.tar") {
            let mut entry_data = Vec::new();
            entry.read_to_end(&mut entry_data)?;

            let decompressed = decompress_control_tar(&name, &entry_data)?;
            let mut tar_archive = tar::Archive::new(std::io::Cursor::new(decompressed));

            for tar_entry in tar_archive.entries()? {
                let mut tar_entry = tar_entry?;
                let path = tar_entry.path()?.to_string_lossy().to_string();
                if path == "./control" || path == "control" {
                    let mut control_text = String::new();
                    tar_entry.read_to_string(&mut control_text)?;
                    return parse_control_file(&control_text);
                }
            }
            return Err(DepotError::BadRequest(
                "control file not found inside control.tar".to_string(),
            ));
        }
    }
    Err(DepotError::BadRequest(
        "control.tar not found in .deb archive".to_string(),
    ))
}

/// Decompress control.tar.{gz,xz,zst} based on the filename extension.
fn decompress_control_tar(name: &str, data: &[u8]) -> error::Result<Vec<u8>> {
    if name.ends_with(".gz") {
        let mut decoder = flate2::read::GzDecoder::new(data);
        let mut out = Vec::new();
        decoder.read_to_end(&mut out)?;
        Ok(out)
    } else if name.ends_with(".xz") {
        let mut out = Vec::new();
        lzma_rs::xz_decompress(&mut std::io::BufReader::new(data), &mut out)
            .map_err(|e| error::DepotError::Internal(format!("xz decompress failed: {e}")))?;
        Ok(out)
    } else if name.ends_with(".zst") {
        let mut decoder = ruzstd::decoding::StreamingDecoder::new(data)
            .map_err(|e| error::DepotError::Internal(format!("zstd decompress failed: {e}")))?;
        let mut out = Vec::new();
        decoder.read_to_end(&mut out)?;
        Ok(out)
    } else if name == "control.tar" {
        Ok(data.to_vec())
    } else {
        Err(DepotError::BadRequest(format!(
            "unsupported control archive format: {name}"
        )))
    }
}

/// Parse an RFC 822-like control file into a DebControl.
pub(super) fn parse_control_file(text: &str) -> error::Result<DebControl> {
    let mut fields: HashMap<String, String> = HashMap::new();
    let mut current_key = String::new();

    for line in text.lines() {
        if line.starts_with(' ') || line.starts_with('\t') {
            // Continuation line
            if !current_key.is_empty() {
                let entry = fields.entry(current_key.clone()).or_default();
                entry.push('\n');
                entry.push_str(line);
            }
        } else if let Some((key, value)) = line.split_once(':') {
            current_key = key.trim().to_lowercase();
            fields.insert(current_key.clone(), value.trim().to_string());
        }
    }

    let package = fields.get("package").cloned().ok_or_else(|| {
        DepotError::BadRequest("missing Package field in control file".to_string())
    })?;
    let version = fields.get("version").cloned().ok_or_else(|| {
        DepotError::BadRequest("missing Version field in control file".to_string())
    })?;
    let architecture = fields.get("architecture").cloned().ok_or_else(|| {
        DepotError::BadRequest("missing Architecture field in control file".to_string())
    })?;

    Ok(DebControl {
        package,
        version,
        architecture,
        maintainer: fields.get("maintainer").cloned().unwrap_or_default(),
        depends: fields.get("depends").cloned().unwrap_or_default(),
        section: fields.get("section").cloned().unwrap_or_default(),
        priority: fields.get("priority").cloned().unwrap_or_default(),
        installed_size: fields.get("installed-size").cloned().unwrap_or_default(),
        description: fields.get("description").cloned().unwrap_or_default(),
        full_control: text.to_string(),
    })
}
