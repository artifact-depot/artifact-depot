// SPDX-FileCopyrightText: 2026 Artifact Depot Contributors
//
// SPDX-License-Identifier: Apache-2.0

use anyhow::Result;
use flate2::write::GzEncoder;
use flate2::Compression;
use rand::Rng;
use rand::RngCore;
use sha2::{Digest, Sha256};
use std::io::Write;

use crate::client::DepotClient;

/// Push a synthetic OCI image with the given tag.
pub async fn push_image(
    client: &DepotClient,
    repo: &str,
    image: &str,
    tag: &str,
    rng: &mut (impl RngCore + ?Sized),
) -> Result<()> {
    let num_layers = rng.random_range(2u32..5);

    let mut layer_digests = Vec::new();
    let mut layer_sizes = Vec::new();
    let mut diff_ids = Vec::new();

    // Generate and push layers
    for i in 0..num_layers {
        let layer_data = generate_layer(rng, i)?;

        // diff_id is sha256 of uncompressed tar (we use compressed, so approximate)
        let uncompressed_digest = sha256_hex(&layer_data);
        diff_ids.push(format!("sha256:{}", uncompressed_digest));

        // Compress layer
        let compressed = gzip_compress(&layer_data);
        let digest = format!("sha256:{}", sha256_hex(&compressed));
        let size = compressed.len();

        client
            .docker_push_blob(repo, image, compressed, &digest)
            .await?;

        layer_digests.push(digest);
        layer_sizes.push(size);
    }

    // Build and push config blob
    let config = build_config(&diff_ids);
    let config_bytes = serde_json::to_vec(&config)?;
    let config_digest = format!("sha256:{}", sha256_hex(&config_bytes));
    let config_size = config_bytes.len();

    client
        .docker_push_blob(repo, image, config_bytes, &config_digest)
        .await?;

    // Build and push manifest
    let manifest = build_manifest(&config_digest, config_size, &layer_digests, &layer_sizes);
    let manifest_bytes = serde_json::to_vec_pretty(&manifest)?;

    client
        .docker_push_manifest(
            repo,
            image,
            tag,
            &manifest_bytes,
            "application/vnd.oci.image.manifest.v1+json",
        )
        .await?;

    Ok(())
}

pub(crate) fn generate_layer(
    rng: &mut (impl RngCore + ?Sized),
    layer_index: u32,
) -> Result<Vec<u8>> {
    let mut archive = tar::Builder::new(Vec::new());

    let num_files = rng.random_range(2u32..6);
    let dirs = ["usr/local/bin", "etc/app", "var/lib/app", "opt/service"];
    let dir = dirs[layer_index as usize % dirs.len()];

    for j in 0..num_files {
        let filename = format!("{}/file-{}-{}.dat", dir, layer_index, j);
        let size = rng.random_range(64usize..2048);
        let mut content = vec![0u8; size];
        rng.fill_bytes(&mut content);

        let mut header = tar::Header::new_gnu();
        header.set_path(&filename)?;
        header.set_size(content.len() as u64);
        header.set_mode(0o644);
        header.set_mtime(1700000000);
        header.set_cksum();

        archive.append(&header, &content[..])?;
    }

    archive.into_inner().map_err(Into::into)
}

pub(crate) fn gzip_compress(data: &[u8]) -> Vec<u8> {
    let mut encoder = GzEncoder::new(Vec::new(), Compression::fast());
    encoder.write_all(data).expect("gzip write");
    encoder.finish().expect("gzip finish")
}

pub(crate) fn sha256_hex(data: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(data);
    format!("{:x}", hasher.finalize())
}

pub(crate) fn build_config(diff_ids: &[String]) -> serde_json::Value {
    serde_json::json!({
        "architecture": "amd64",
        "os": "linux",
        "config": {
            "Env": ["PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin"],
            "Cmd": ["/bin/sh"],
            "WorkingDir": "/"
        },
        "rootfs": {
            "type": "layers",
            "diff_ids": diff_ids
        },
        "history": diff_ids.iter().enumerate().map(|(i, _)| {
            serde_json::json!({
                "created": "2026-01-01T00:00:00Z",
                "created_by": format!("depot-bench layer {}", i),
                "empty_layer": false
            })
        }).collect::<Vec<_>>()
    })
}

pub(crate) fn build_manifest(
    config_digest: &str,
    config_size: usize,
    layer_digests: &[String],
    layer_sizes: &[usize],
) -> serde_json::Value {
    serde_json::json!({
        "schemaVersion": 2,
        "mediaType": "application/vnd.oci.image.manifest.v1+json",
        "config": {
            "mediaType": "application/vnd.oci.image.config.v1+json",
            "digest": config_digest,
            "size": config_size
        },
        "layers": layer_digests.iter().zip(layer_sizes.iter()).map(|(d, s)| {
            serde_json::json!({
                "mediaType": "application/vnd.oci.image.layer.v1.tar+gzip",
                "digest": d,
                "size": s
            })
        }).collect::<Vec<_>>()
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use rand::SeedableRng;
    use rand_chacha::ChaCha8Rng;

    fn test_rng() -> ChaCha8Rng {
        ChaCha8Rng::seed_from_u64(42)
    }

    #[test]
    fn sha256_hex_known_value() {
        // SHA-256 of empty string
        let digest = sha256_hex(b"");
        assert_eq!(
            digest,
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }

    #[test]
    fn sha256_hex_produces_64_char_hex() {
        let digest = sha256_hex(b"hello world");
        assert_eq!(digest.len(), 64);
        assert!(digest.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn gzip_compress_produces_valid_gzip() {
        let data = b"hello world, this is test data for compression";
        let compressed = gzip_compress(data);
        // Check gzip magic bytes
        assert_eq!(compressed[0], 0x1f);
        assert_eq!(compressed[1], 0x8b);
        // Decompress and verify
        use flate2::read::GzDecoder;
        use std::io::Read;
        let mut decoder = GzDecoder::new(&compressed[..]);
        let mut decompressed = Vec::new();
        decoder.read_to_end(&mut decompressed).unwrap();
        assert_eq!(decompressed, data);
    }

    #[test]
    fn gzip_compress_empty() {
        let compressed = gzip_compress(b"");
        assert_eq!(compressed[0], 0x1f);
        assert_eq!(compressed[1], 0x8b);
        use flate2::read::GzDecoder;
        use std::io::Read;
        let mut decoder = GzDecoder::new(&compressed[..]);
        let mut decompressed = Vec::new();
        decoder.read_to_end(&mut decompressed).unwrap();
        assert!(decompressed.is_empty());
    }

    #[test]
    fn generate_layer_produces_valid_tar() {
        let mut rng = test_rng();
        let data = generate_layer(&mut rng, 0).unwrap();
        assert!(!data.is_empty());
        // Parse as tar to verify
        let mut archive = tar::Archive::new(&data[..]);
        let entries: Vec<_> = archive.entries().unwrap().collect();
        assert!(entries.len() >= 2, "expected at least 2 files in layer");
        assert!(entries.len() <= 5, "expected at most 5 files in layer");
    }

    #[test]
    fn generate_layer_uses_correct_directory() {
        let dirs = ["usr/local/bin", "etc/app", "var/lib/app", "opt/service"];
        for i in 0..4u32 {
            let mut rng = test_rng();
            let data = generate_layer(&mut rng, i).unwrap();
            let mut archive = tar::Archive::new(&data[..]);
            for entry in archive.entries().unwrap() {
                let entry = entry.unwrap();
                let path = entry.path().unwrap().to_string_lossy().to_string();
                assert!(
                    path.starts_with(dirs[i as usize]),
                    "layer {} file {} doesn't start with {}",
                    i,
                    path,
                    dirs[i as usize]
                );
            }
        }
    }

    #[test]
    fn generate_layer_wraps_directory_index() {
        // layer_index 4 should use dirs[0] = "usr/local/bin"
        let mut rng = test_rng();
        let data = generate_layer(&mut rng, 4).unwrap();
        let mut archive = tar::Archive::new(&data[..]);
        let entry = archive.entries().unwrap().next().unwrap().unwrap();
        let path = entry.path().unwrap().to_string_lossy().to_string();
        assert!(path.starts_with("usr/local/bin"));
    }

    #[test]
    fn generate_layer_deterministic() {
        let d1 = generate_layer(&mut test_rng(), 0).unwrap();
        let d2 = generate_layer(&mut test_rng(), 0).unwrap();
        assert_eq!(d1, d2);
    }

    #[test]
    fn build_config_has_required_fields() {
        let diff_ids = vec!["sha256:aaa".to_string(), "sha256:bbb".to_string()];
        let config = build_config(&diff_ids);
        assert_eq!(config["architecture"], "amd64");
        assert_eq!(config["os"], "linux");
        assert_eq!(config["rootfs"]["type"], "layers");
        let ids = config["rootfs"]["diff_ids"].as_array().unwrap();
        assert_eq!(ids.len(), 2);
        assert_eq!(ids[0], "sha256:aaa");
    }

    #[test]
    fn build_config_history_matches_layers() {
        let diff_ids = vec![
            "sha256:aaa".to_string(),
            "sha256:bbb".to_string(),
            "sha256:ccc".to_string(),
        ];
        let config = build_config(&diff_ids);
        let history = config["history"].as_array().unwrap();
        assert_eq!(history.len(), 3);
        assert_eq!(history[0]["empty_layer"], false);
        assert!(history[1]["created_by"]
            .as_str()
            .unwrap()
            .contains("layer 1"));
    }

    #[test]
    fn build_config_has_container_config() {
        let config = build_config(&["sha256:abc".to_string()]);
        let env = config["config"]["Env"].as_array().unwrap();
        assert!(!env.is_empty());
        assert_eq!(config["config"]["Cmd"][0], "/bin/sh");
    }

    #[test]
    fn build_config_empty_layers() {
        let config = build_config(&[]);
        assert_eq!(config["rootfs"]["diff_ids"].as_array().unwrap().len(), 0);
        assert_eq!(config["history"].as_array().unwrap().len(), 0);
    }

    #[test]
    fn build_manifest_has_correct_structure() {
        let config_digest = "sha256:configdigest";
        let layer_digests = vec!["sha256:layer1".to_string(), "sha256:layer2".to_string()];
        let layer_sizes = vec![1000, 2000];

        let manifest = build_manifest(config_digest, 500, &layer_digests, &layer_sizes);

        assert_eq!(manifest["schemaVersion"], 2);
        assert_eq!(
            manifest["mediaType"],
            "application/vnd.oci.image.manifest.v1+json"
        );
        assert_eq!(manifest["config"]["digest"], config_digest);
        assert_eq!(manifest["config"]["size"], 500);
        assert_eq!(
            manifest["config"]["mediaType"],
            "application/vnd.oci.image.config.v1+json"
        );

        let layers = manifest["layers"].as_array().unwrap();
        assert_eq!(layers.len(), 2);
        assert_eq!(layers[0]["digest"], "sha256:layer1");
        assert_eq!(layers[0]["size"], 1000);
        assert_eq!(
            layers[0]["mediaType"],
            "application/vnd.oci.image.layer.v1.tar+gzip"
        );
        assert_eq!(layers[1]["digest"], "sha256:layer2");
        assert_eq!(layers[1]["size"], 2000);
    }

    #[test]
    fn build_manifest_empty_layers() {
        let manifest = build_manifest("sha256:cfg", 100, &[], &[]);
        assert_eq!(manifest["layers"].as_array().unwrap().len(), 0);
    }

    #[test]
    fn build_manifest_serializes_to_valid_json() {
        let manifest = build_manifest("sha256:abc", 123, &["sha256:l1".to_string()], &[456]);
        let json_str = serde_json::to_string_pretty(&manifest).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json_str).unwrap();
        assert_eq!(parsed["schemaVersion"], 2);
    }
}
