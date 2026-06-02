// SPDX-FileCopyrightText: 2026 Artifact Depot Contributors
//
// SPDX-License-Identifier: Apache-2.0

use rand::seq::IndexedRandom;
use rand::Rng;

/// Raw repository definitions with name and artifact style.
pub struct RawRepoDef {
    pub name: &'static str,
    pub description: &'static str,
    pub path_gen: fn(&mut dyn RngCore, usize) -> (String, &'static str),
}

use rand::RngCore;

const GROUPS: &[&str] = &[
    "com/example",
    "org/acme",
    "io/quantum",
    "net/widgets",
    "com/internal",
    "org/platform",
];

const RAW_ARTIFACTS: &[&str] = &[
    "core",
    "api-client",
    "commons",
    "utils",
    "service",
    "gateway",
    "worker",
    "scheduler",
    "pipeline",
    "connector",
    "adapter",
    "transformer",
    "metrics",
    "logging",
    "config-lib",
];

const VERSIONS: &[&str] = &[
    "1.0.0", "1.0.1", "1.1.0", "1.2.0", "1.2.1", "2.0.0", "2.0.1", "2.1.0", "3.0.0", "3.1.0",
];

const CONFIG_FILES: &[&str] = &[
    "application.yml",
    "database.yml",
    "logging.conf",
    "nginx.conf",
    "redis.conf",
    "haproxy.cfg",
    "prometheus.yml",
    "grafana.json",
    "alertmanager.yml",
    "docker-compose.yml",
    "k8s-deployment.yml",
    "terraform.json",
    "vault.hcl",
    "consul.json",
    "envoy.yaml",
];

const CONFIG_ENVS: &[&str] = &["dev", "staging", "production", "qa", "perf"];

const DOC_NAMES: &[&str] = &[
    "README.md",
    "CHANGELOG.md",
    "architecture.md",
    "api-spec.json",
    "runbook.md",
    "onboarding.md",
    "deployment-guide.md",
    "troubleshooting.md",
    "security-policy.md",
    "data-model.md",
];

const DOC_PROJECTS: &[&str] = &[
    "platform",
    "api-gateway",
    "data-pipeline",
    "auth-service",
    "billing",
    "notifications",
    "search",
    "analytics",
];

const DOCKER_APP_IMAGES: &[&str] = &[
    "web-frontend",
    "api-server",
    "auth-service",
    "worker",
    "scheduler",
    "notification-service",
    "search-indexer",
    "data-processor",
    "gateway",
    "admin-panel",
];

const DOCKER_BASE_IMAGES: &[&str] = &[
    "base-python",
    "base-java",
    "base-node",
    "base-golang",
    "base-rust",
    "base-alpine",
    "base-ubuntu",
    "base-nginx",
];

/// Generate a Maven-style path for a releases repo.
pub fn gen_release_path(rng: &mut dyn RngCore, _index: usize) -> (String, &'static str) {
    let group = GROUPS.choose(rng).unwrap();
    let artifact = RAW_ARTIFACTS.choose(rng).unwrap();
    let version = VERSIONS.choose(rng).unwrap();
    let ext = if rng.random_bool(0.7) {
        "jar"
    } else {
        "tar.gz"
    };
    let path = format!(
        "{}/{}/{}/{}-{}.{}",
        group, artifact, version, artifact, version, ext
    );
    let ct = if ext == "jar" {
        "application/java-archive"
    } else {
        "application/gzip"
    };
    (path, ct)
}

/// Generate a config file path.
pub fn gen_config_path(rng: &mut dyn RngCore, _index: usize) -> (String, &'static str) {
    let env = CONFIG_ENVS.choose(rng).unwrap();
    let file = CONFIG_FILES.choose(rng).unwrap();
    let path = format!("{}/{}", env, file);
    let ct = if file.ends_with(".yml") || file.ends_with(".yaml") {
        "text/yaml"
    } else if file.ends_with(".json") {
        "application/json"
    } else {
        "text/plain"
    };
    (path, ct)
}

/// Generate a docs path.
pub fn gen_doc_path(rng: &mut dyn RngCore, _index: usize) -> (String, &'static str) {
    let project = DOC_PROJECTS.choose(rng).unwrap();
    let doc = DOC_NAMES.choose(rng).unwrap();
    let version = VERSIONS.choose(rng).unwrap();
    let path = format!("{}/v{}/{}", project, version, doc);
    let ct = if doc.ends_with(".md") {
        "text/markdown"
    } else {
        "application/json"
    };
    (path, ct)
}

/// Get raw repo definitions for the given count.
pub fn raw_repo_defs(count: usize) -> Vec<RawRepoDef> {
    let all = [
        RawRepoDef {
            name: "releases",
            description: "Maven-style release artifacts",
            path_gen: gen_release_path,
        },
        RawRepoDef {
            name: "configs",
            description: "Infrastructure config files",
            path_gen: gen_config_path,
        },
        RawRepoDef {
            name: "docs",
            description: "Documentation and specs",
            path_gen: gen_doc_path,
        },
    ];
    let len = all.len();
    let mut result = Vec::with_capacity(count);
    for i in 0..count {
        let idx = i % len;
        result.push(RawRepoDef {
            name: all[idx].name,
            description: all[idx].description,
            path_gen: all[idx].path_gen,
        });
    }
    result
}

/// Docker repo names.
pub fn docker_repo_names(count: usize) -> Vec<&'static str> {
    let all: &[&str] = &["docker-internal", "docker-base"];
    (0..count).map(|i| all[i % all.len()]).collect()
}

/// Pick image names for a docker repo.
pub fn docker_image_names(repo_name: &str, count: usize) -> Vec<&'static str> {
    let source = if repo_name.contains("base") {
        DOCKER_BASE_IMAGES
    } else {
        DOCKER_APP_IMAGES
    };
    source.iter().copied().cycle().take(count).collect()
}

/// Generate tags for a docker image.
pub fn docker_tags(rng: &mut dyn RngCore, repo_name: &str, count: usize) -> Vec<String> {
    let mut tags = Vec::with_capacity(count);
    if repo_name.contains("base") {
        for i in 0..count {
            tags.push(format!("20260{}{:02}", (i / 30) + 1, (i % 30) + 1));
        }
    } else {
        for _ in 0..count {
            let major = rng.random_range(0u32..5);
            let minor = rng.random_range(0u32..20);
            let patch = rng.random_range(0u32..100);
            tags.push(format!("{}.{}.{}", major, minor, patch));
        }
    }
    tags
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
    fn gen_release_path_returns_maven_style() {
        let mut rng = test_rng();
        let (path, ct) = gen_release_path(&mut rng, 0);
        assert!(path.contains('/'), "path should have separators: {}", path);
        assert!(
            path.ends_with(".jar") || path.ends_with(".tar.gz"),
            "unexpected extension: {}",
            path
        );
        assert!(
            ct == "application/java-archive" || ct == "application/gzip",
            "unexpected content type: {}",
            ct
        );
    }

    #[test]
    fn gen_release_path_deterministic() {
        let (p1, c1) = gen_release_path(&mut test_rng(), 0);
        let (p2, c2) = gen_release_path(&mut test_rng(), 0);
        assert_eq!(p1, p2);
        assert_eq!(c1, c2);
    }

    #[test]
    fn gen_release_path_includes_group_artifact_version() {
        let mut rng = test_rng();
        for _ in 0..20 {
            let (path, _) = gen_release_path(&mut rng, 0);
            let parts: Vec<&str> = path.split('/').collect();
            // group (2 segments) / artifact / version / filename
            assert!(parts.len() >= 4, "too few segments: {}", path);
        }
    }

    #[test]
    fn gen_config_path_returns_env_and_file() {
        let mut rng = test_rng();
        let (path, ct) = gen_config_path(&mut rng, 0);
        assert!(path.contains('/'));
        let env = path.split('/').next().unwrap();
        assert!(
            ["dev", "staging", "production", "qa", "perf"].contains(&env),
            "unexpected env: {}",
            env
        );
        assert!(
            ct == "text/yaml" || ct == "application/json" || ct == "text/plain",
            "unexpected content type: {}",
            ct
        );
    }

    #[test]
    fn gen_config_path_content_type_matches_extension() {
        let mut rng = test_rng();
        for _ in 0..50 {
            let (path, ct) = gen_config_path(&mut rng, 0);
            if path.ends_with(".yml") || path.ends_with(".yaml") {
                assert_eq!(ct, "text/yaml");
            } else if path.ends_with(".json") {
                assert_eq!(ct, "application/json");
            } else {
                assert_eq!(ct, "text/plain");
            }
        }
    }

    #[test]
    fn gen_doc_path_returns_project_version_doc() {
        let mut rng = test_rng();
        let (path, ct) = gen_doc_path(&mut rng, 0);
        assert!(path.contains("/v"));
        assert!(
            ct == "text/markdown" || ct == "application/json",
            "unexpected content type: {}",
            ct
        );
    }

    #[test]
    fn gen_doc_path_content_type_matches_extension() {
        let mut rng = test_rng();
        for _ in 0..50 {
            let (path, ct) = gen_doc_path(&mut rng, 0);
            if path.ends_with(".md") {
                assert_eq!(ct, "text/markdown");
            } else {
                assert_eq!(ct, "application/json");
            }
        }
    }

    #[test]
    fn raw_repo_defs_returns_correct_count() {
        assert_eq!(raw_repo_defs(0).len(), 0);
        assert_eq!(raw_repo_defs(1).len(), 1);
        assert_eq!(raw_repo_defs(3).len(), 3);
        assert_eq!(raw_repo_defs(5).len(), 5);
    }

    #[test]
    fn raw_repo_defs_cycles_names() {
        let defs = raw_repo_defs(6);
        assert_eq!(defs[0].name, "releases");
        assert_eq!(defs[1].name, "configs");
        assert_eq!(defs[2].name, "docs");
        assert_eq!(defs[3].name, "releases");
        assert_eq!(defs[4].name, "configs");
        assert_eq!(defs[5].name, "docs");
    }

    #[test]
    fn raw_repo_defs_path_gen_is_callable() {
        let mut rng = test_rng();
        for def in raw_repo_defs(3) {
            let (path, ct) = (def.path_gen)(&mut rng, 0);
            assert!(!path.is_empty());
            assert!(!ct.is_empty());
        }
    }

    #[test]
    fn docker_repo_names_returns_correct_count() {
        assert_eq!(docker_repo_names(0).len(), 0);
        assert_eq!(docker_repo_names(1).len(), 1);
        assert_eq!(docker_repo_names(4).len(), 4);
    }

    #[test]
    fn docker_repo_names_cycles() {
        let names = docker_repo_names(4);
        assert_eq!(names[0], "docker-internal");
        assert_eq!(names[1], "docker-base");
        assert_eq!(names[2], "docker-internal");
        assert_eq!(names[3], "docker-base");
    }

    #[test]
    fn docker_image_names_uses_app_images_for_internal() {
        let images = docker_image_names("docker-internal", 3);
        assert_eq!(images.len(), 3);
        // All should come from DOCKER_APP_IMAGES
        for img in &images {
            assert!(
                !img.starts_with("base-"),
                "internal repo got base image: {}",
                img
            );
        }
    }

    #[test]
    fn docker_image_names_uses_base_images_for_base() {
        let images = docker_image_names("docker-base", 3);
        assert_eq!(images.len(), 3);
        for img in &images {
            assert!(
                img.starts_with("base-"),
                "base repo got non-base image: {}",
                img
            );
        }
    }

    #[test]
    fn docker_image_names_cycles_beyond_list_length() {
        let images = docker_image_names("docker-internal", 15);
        assert_eq!(images.len(), 15);
        // Should wrap around
        assert_eq!(images[0], images[10]);
    }

    #[test]
    fn docker_tags_base_uses_date_format() {
        let mut rng = test_rng();
        let tags = docker_tags(&mut rng, "docker-base", 3);
        assert_eq!(tags.len(), 3);
        for tag in &tags {
            assert!(tag.starts_with("2026"), "expected date tag: {}", tag);
        }
    }

    #[test]
    fn docker_tags_internal_uses_semver() {
        let mut rng = test_rng();
        let tags = docker_tags(&mut rng, "docker-internal", 5);
        assert_eq!(tags.len(), 5);
        for tag in &tags {
            let parts: Vec<&str> = tag.split('.').collect();
            assert_eq!(parts.len(), 3, "expected semver: {}", tag);
            for part in &parts {
                part.parse::<u32>().unwrap_or_else(|_| {
                    panic!("non-numeric semver component '{}' in {}", part, tag)
                });
            }
        }
    }

    #[test]
    fn docker_tags_deterministic() {
        let tags1 = docker_tags(&mut test_rng(), "docker-internal", 3);
        let tags2 = docker_tags(&mut test_rng(), "docker-internal", 3);
        assert_eq!(tags1, tags2);
    }

    #[test]
    fn docker_tags_empty() {
        let mut rng = test_rng();
        let tags = docker_tags(&mut rng, "docker-internal", 0);
        assert!(tags.is_empty());
    }
}
