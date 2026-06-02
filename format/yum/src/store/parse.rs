// SPDX-FileCopyrightText: 2026 Artifact Depot Contributors
//
// SPDX-License-Identifier: Apache-2.0

use depot_core::error::{self, DepotError};
use depot_core::store::kv::{RpmChangelog, RpmDep, RpmFileEntry, RpmPackageMeta};

/// Convert rpm crate Dependency to our RpmDep.
fn convert_dep(dep: &rpm::Dependency) -> RpmDep {
    let flags = dep.flags;
    let flag_str = if flags.contains(rpm::DependencyFlags::LESS | rpm::DependencyFlags::EQUAL) {
        "LE"
    } else if flags.contains(rpm::DependencyFlags::GREATER | rpm::DependencyFlags::EQUAL) {
        "GE"
    } else if flags.contains(rpm::DependencyFlags::EQUAL) {
        "EQ"
    } else if flags.contains(rpm::DependencyFlags::LESS) {
        "LT"
    } else if flags.contains(rpm::DependencyFlags::GREATER) {
        "GT"
    } else {
        ""
    };
    let (epoch, ver, rel) = parse_evr(&dep.version);
    RpmDep {
        name: dep.name.clone(),
        flags: flag_str.to_string(),
        epoch,
        ver,
        rel,
    }
}

/// Parse an EVR string like "1:2.0-3" into (epoch, version, release).
pub(super) fn parse_evr(evr: &str) -> (String, String, String) {
    let (epoch, vr) = if let Some((e, rest)) = evr.split_once(':') {
        (e.to_string(), rest)
    } else {
        ("0".to_string(), evr)
    };
    let (ver, rel) = if let Some((v, r)) = vr.split_once('-') {
        (v.to_string(), r.to_string())
    } else {
        (vr.to_string(), String::new())
    };
    (epoch, ver, rel)
}

/// Parse an RPM file and extract package metadata.
/// The SHA-256 hash and archive size must be provided by the caller (computed during streaming).
pub fn parse_rpm(
    reader: impl std::io::Read,
    sha256: &str,
    archive_size: u64,
) -> error::Result<RpmPackageMeta> {
    let pkg = rpm::Package::parse(&mut std::io::BufReader::new(reader))
        .map_err(|e| DepotError::BadRequest(format!("invalid RPM file: {e}")))?;

    let meta = &pkg.metadata;

    let name = meta.get_name().unwrap_or("").to_string();
    let version = meta.get_version().unwrap_or("").to_string();
    let release = meta.get_release().unwrap_or("").to_string();
    let epoch = meta.get_epoch().unwrap_or(0);
    let arch = meta.get_arch().unwrap_or("").to_string();
    let summary = meta.get_summary().unwrap_or("").to_string();
    let description = meta.get_description().unwrap_or("").to_string();
    let url = meta.get_url().unwrap_or("").to_string();
    let license = meta.get_license().unwrap_or("").to_string();
    let vendor = meta.get_vendor().unwrap_or("").to_string();
    let group = meta.get_group().unwrap_or("").to_string();
    let buildhost = meta.get_build_host().unwrap_or("").to_string();
    let buildtime = meta.get_build_time().unwrap_or(0);
    let packager = meta.get_packager().unwrap_or("").to_string();
    let installed_size = meta.get_installed_size().unwrap_or(0);

    let sha256 = sha256.to_string();

    let requires = meta
        .get_requires()
        .unwrap_or_default()
        .iter()
        .map(convert_dep)
        .collect();
    let provides = meta
        .get_provides()
        .unwrap_or_default()
        .iter()
        .map(convert_dep)
        .collect();
    let conflicts = meta
        .get_conflicts()
        .unwrap_or_default()
        .iter()
        .map(convert_dep)
        .collect();
    let obsoletes = meta
        .get_obsoletes()
        .unwrap_or_default()
        .iter()
        .map(convert_dep)
        .collect();

    let files = match meta.get_file_entries() {
        Ok(entries) => entries
            .iter()
            .map(|e| {
                let file_type = if e.mode.file_type() == rpm::FileType::Dir {
                    "dir"
                } else if e.flags.contains(rpm::FileFlags::GHOST) {
                    "ghost"
                } else {
                    "file"
                };
                RpmFileEntry {
                    path: e.path.to_string_lossy().to_string(),
                    file_type: file_type.to_string(),
                }
            })
            .collect(),
        Err(_) => Vec::new(),
    };

    let changelogs = match meta.get_changelog_entries() {
        Ok(entries) => entries
            .into_iter()
            .map(|e| RpmChangelog {
                author: e.name,
                timestamp: e.timestamp,
                text: e.description,
            })
            .collect(),
        Err(_) => Vec::new(),
    };

    if name.is_empty() {
        return Err(DepotError::BadRequest(
            "RPM missing required Name header".to_string(),
        ));
    }

    Ok(RpmPackageMeta {
        name,
        epoch,
        version,
        release,
        arch,
        summary,
        description,
        url,
        license,
        vendor,
        group,
        buildhost,
        buildtime,
        packager,
        installed_size,
        archive_size,
        sha256,
        requires,
        provides,
        conflicts,
        obsoletes,
        files,
        changelogs,
    })
}
