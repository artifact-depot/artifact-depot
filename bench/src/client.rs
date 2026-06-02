// SPDX-FileCopyrightText: 2026 Artifact Depot Contributors
//
// SPDX-License-Identifier: Apache-2.0

use anyhow::{Context, Result};
use base64::Engine;
use reqwest::{Client, StatusCode};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tokio::sync::Mutex;

#[derive(Clone)]
pub struct DepotClient {
    http: Client,
    base_url: String,
    username: String,
    password: String,
    basic_auth: String,
    jwt_token: Arc<Mutex<Option<String>>>,
}

#[derive(Debug, Serialize)]
pub struct CreateRepoRequest {
    pub name: String,
    pub repo_type: String,
    pub format: String,
    pub store: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub upstream_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_ttl_secs: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub members: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub listen: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct RepoResponse {
    pub name: String,
    #[allow(dead_code)]
    pub repo_type: String,
    #[allow(dead_code)]
    pub format: String,
}

#[derive(Debug, Deserialize)]
pub struct ArtifactResponse {
    pub path: String,
    #[allow(dead_code)]
    pub size: u64,
}

#[derive(Debug, Deserialize)]
pub struct BrowseResponse {
    #[allow(dead_code)]
    pub dirs: Vec<serde_json::Value>,
    pub artifacts: Vec<ArtifactResponse>,
}

#[derive(Debug, Deserialize)]
struct LoginResponse {
    token: String,
}

#[derive(Debug, Deserialize)]
struct TagListResponse {
    #[allow(dead_code)]
    name: String,
    tags: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct CatalogResponse {
    repositories: Vec<String>,
}

const MANIFEST_ACCEPT_ALL: &str = "application/vnd.docker.distribution.manifest.v2+json, \
    application/vnd.docker.distribution.manifest.list.v2+json, \
    application/vnd.oci.image.manifest.v1+json, \
    application/vnd.oci.image.index.v1+json";

impl DepotClient {
    pub fn new(base_url: &str, username: &str, password: &str, insecure: bool) -> Result<Self> {
        let http = Client::builder()
            .danger_accept_invalid_certs(insecure)
            .build()
            .context("failed to build HTTP client")?;

        let basic_auth =
            base64::engine::general_purpose::STANDARD.encode(format!("{username}:{password}"));

        Ok(Self {
            http,
            base_url: base_url.trim_end_matches('/').to_string(),
            username: username.to_string(),
            password: password.to_string(),
            basic_auth,
            jwt_token: Arc::new(Mutex::new(None)),
        })
    }

    /// Create an independent copy with its own HTTP connection pool.
    /// Shares credentials and JWT token but uses a separate TCP connection.
    pub fn fork(&self) -> Result<Self> {
        let http = Client::builder()
            .danger_accept_invalid_certs(true)
            .http2_initial_stream_window_size(16 * 1024 * 1024)
            .http2_initial_connection_window_size(16 * 1024 * 1024)
            .http2_adaptive_window(true)
            .build()
            .context("failed to build HTTP client")?;

        Ok(Self {
            http,
            base_url: self.base_url.clone(),
            username: self.username.clone(),
            password: self.password.clone(),
            basic_auth: self.basic_auth.clone(),
            jwt_token: self.jwt_token.clone(),
        })
    }

    pub async fn login(&self) -> Result<()> {
        let resp = self
            .http
            .post(format!("{}/api/v1/auth/login", self.base_url))
            .json(&serde_json::json!({
                "username": self.username,
                "password": self.password,
            }))
            .send()
            .await
            .context("login request failed")?;

        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!("login failed ({}): {}", status, body);
        }

        let login: LoginResponse = resp.json().await.context("parse login response")?;
        *self.jwt_token.lock().await = Some(login.token);
        Ok(())
    }

    /// Discard the cached JWT so the next `bearer_token()` call re-authenticates.
    pub async fn clear_token(&self) {
        *self.jwt_token.lock().await = None;
    }

    async fn bearer_token(&self) -> Result<String> {
        {
            let guard = self.jwt_token.lock().await;
            if let Some(ref token) = *guard {
                return Ok(token.clone());
            }
        }
        self.login().await?;
        let guard = self.jwt_token.lock().await;
        guard
            .clone()
            .ok_or_else(|| anyhow::anyhow!("no JWT token after login"))
    }

    // --- User management ---

    pub async fn create_user(&self, username: &str, password: &str, roles: &[&str]) -> Result<()> {
        let token = self.bearer_token().await?;
        let resp = self
            .http
            .post(format!("{}/api/v1/users", self.base_url))
            .bearer_auth(&token)
            .json(&serde_json::json!({
                "username": username,
                "password": password,
                "roles": roles,
                "must_change_password": false,
            }))
            .send()
            .await?;
        let status = resp.status();
        if !status.is_success() && status != StatusCode::CONFLICT {
            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!("create user '{}' failed ({}): {}", username, status, body);
        }
        Ok(())
    }

    // --- Repository CRUD ---

    pub async fn list_repos(&self) -> Result<Vec<RepoResponse>> {
        let token = self.bearer_token().await?;
        let resp = self
            .http
            .get(format!("{}/api/v1/repositories", self.base_url))
            .bearer_auth(&token)
            .send()
            .await?;
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!("list repos failed ({}): {}", status, body);
        }
        Ok(resp.json().await?)
    }

    pub async fn create_repo(&self, req: &CreateRepoRequest) -> Result<RepoResponse> {
        let token = self.bearer_token().await?;
        let resp = self
            .http
            .post(format!("{}/api/v1/repositories", self.base_url))
            .bearer_auth(&token)
            .json(req)
            .send()
            .await?;
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!("create repo '{}' failed ({}): {}", req.name, status, body);
        }
        Ok(resp.json().await?)
    }

    pub async fn delete_repo(&self, name: &str) -> Result<()> {
        let token = self.bearer_token().await?;
        let resp = self
            .http
            .delete(format!("{}/api/v1/repositories/{}", self.base_url, name))
            .bearer_auth(&token)
            .send()
            .await?;
        let status = resp.status();
        if status == StatusCode::NOT_FOUND {
            return Ok(());
        }
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!("delete repo '{}' failed ({}): {}", name, status, body);
        }
        Ok(())
    }

    pub async fn create_store(&self, name: &str, store_type: &str, root: &str) -> Result<()> {
        let token = self.bearer_token().await?;
        let resp = self
            .http
            .post(format!("{}/api/v1/stores", self.base_url))
            .bearer_auth(&token)
            .json(&serde_json::json!({
                "name": name,
                "store_type": store_type,
                "root": root,
            }))
            .send()
            .await?;
        let status = resp.status();
        if status == StatusCode::CONFLICT {
            return Ok(());
        }
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!("create store '{}' failed ({}): {}", name, status, body);
        }
        Ok(())
    }

    pub async fn clone_repo(&self, name: &str, new_name: &str) -> Result<serde_json::Value> {
        let token = self.bearer_token().await?;
        let resp = self
            .http
            .post(format!(
                "{}/api/v1/repositories/{}/clone",
                self.base_url, name
            ))
            .bearer_auth(&token)
            .json(&serde_json::json!({ "new_name": new_name }))
            .send()
            .await?;
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!(
                "clone repo '{}' -> '{}' failed ({}): {}",
                name,
                new_name,
                status,
                body
            );
        }
        let task: serde_json::Value = resp.json().await?;
        // Wait for the clone task to complete.
        let task_id = task["id"].as_str().unwrap_or_default().to_string();
        self.wait_for_task(&task_id).await?;
        Ok(task)
    }

    /// Poll a task until it reaches a terminal state (completed/failed/cancelled).
    pub async fn wait_for_task(&self, task_id: &str) -> Result<serde_json::Value> {
        let token = self.bearer_token().await?;
        loop {
            let resp = self
                .http
                .get(format!("{}/api/v1/tasks/{}", self.base_url, task_id))
                .bearer_auth(&token)
                .send()
                .await?;
            let info: serde_json::Value = resp.json().await?;
            let status = info["status"].as_str().unwrap_or_default();
            match status {
                "completed" => return Ok(info),
                "failed" => anyhow::bail!("task failed: {}", info["error"]),
                "cancelled" => anyhow::bail!("task cancelled"),
                _ => tokio::time::sleep(std::time::Duration::from_millis(100)).await,
            }
        }
    }

    // --- Raw artifact ops ---

    pub async fn upload_raw(
        &self,
        repo: &str,
        path: &str,
        data: Vec<u8>,
        content_type: &str,
    ) -> Result<()> {
        let token = self.bearer_token().await?;
        let resp = self
            .http
            .put(format!("{}/repository/{}/{}", self.base_url, repo, path))
            .bearer_auth(&token)
            .header("Content-Type", content_type)
            .body(data)
            .send()
            .await?;
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!("upload {}/{} failed ({}): {}", repo, path, status, body);
        }
        Ok(())
    }

    pub async fn download_raw(&self, repo: &str, path: &str) -> Result<Vec<u8>> {
        let token = self.bearer_token().await?;
        let resp = self
            .http
            .get(format!("{}/repository/{}/{}", self.base_url, repo, path))
            .bearer_auth(&token)
            .send()
            .await?;
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!("download {}/{} failed ({}): {}", repo, path, status, body);
        }
        Ok(resp.bytes().await?.to_vec())
    }

    pub async fn delete_raw(&self, repo: &str, path: &str) -> Result<()> {
        let token = self.bearer_token().await?;
        let resp = self
            .http
            .delete(format!("{}/repository/{}/{}", self.base_url, repo, path))
            .bearer_auth(&token)
            .send()
            .await?;
        let status = resp.status();
        if !status.is_success() && status != StatusCode::NOT_FOUND {
            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!("delete {}/{} failed ({}): {}", repo, path, status, body);
        }
        Ok(())
    }

    pub async fn head_raw(&self, repo: &str, path: &str) -> Result<(u16, u64)> {
        let token = self.bearer_token().await?;
        let resp = self
            .http
            .head(format!("{}/repository/{}/{}", self.base_url, repo, path))
            .bearer_auth(&token)
            .send()
            .await?;
        let status = resp.status().as_u16();
        let size = resp
            .headers()
            .get("content-length")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(0);
        Ok((status, size))
    }

    // --- APT ops ---

    pub async fn apt_upload(
        &self,
        repo: &str,
        component: &str,
        filename: &str,
        data: Vec<u8>,
    ) -> Result<()> {
        let token = self.bearer_token().await?;
        let form = reqwest::multipart::Form::new()
            .text("component", component.to_string())
            .part(
                "file",
                reqwest::multipart::Part::bytes(data).file_name(filename.to_string()),
            );
        let resp = self
            .http
            .post(format!("{}/repository/{}/upload", self.base_url, repo))
            .bearer_auth(&token)
            .multipart(form)
            .send()
            .await?;
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!(
                "apt upload {}/{} failed ({}): {}",
                repo,
                filename,
                status,
                body
            );
        }
        Ok(())
    }

    // --- YUM ops ---

    pub async fn yum_upload(
        &self,
        repo: &str,
        filename: &str,
        data: Vec<u8>,
        directory: Option<&str>,
    ) -> Result<()> {
        let token = self.bearer_token().await?;
        let mut form = reqwest::multipart::Form::new().part(
            "file",
            reqwest::multipart::Part::bytes(data).file_name(filename.to_string()),
        );
        if let Some(dir) = directory {
            form = form.text("directory", dir.to_string());
        }
        let resp = self
            .http
            .post(format!("{}/repository/{}/upload", self.base_url, repo))
            .bearer_auth(&token)
            .multipart(form)
            .send()
            .await?;
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!(
                "yum upload {}/{} failed ({}): {}",
                repo,
                filename,
                status,
                body
            );
        }
        Ok(())
    }

    pub async fn apt_get(&self, repo: &str, path: &str) -> Result<Vec<u8>> {
        let token = self.bearer_token().await?;
        let resp = self
            .http
            .get(format!("{}/repository/{}/{}", self.base_url, repo, path))
            .bearer_auth(&token)
            .send()
            .await?;
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!("apt get {}/{} failed ({}): {}", repo, path, status, body);
        }
        Ok(resp.bytes().await?.to_vec())
    }

    // --- Golang ops ---

    pub async fn golang_upload(
        &self,
        repo: &str,
        module: &str,
        version: &str,
        info: &[u8],
        gomod: &[u8],
        zip: &[u8],
    ) -> Result<()> {
        let token = self.bearer_token().await?;
        let form = reqwest::multipart::Form::new()
            .text("module", module.to_string())
            .text("version", version.to_string())
            .part(
                "info",
                reqwest::multipart::Part::bytes(info.to_vec()).file_name("info"),
            )
            .part(
                "mod",
                reqwest::multipart::Part::bytes(gomod.to_vec()).file_name("go.mod"),
            )
            .part(
                "zip",
                reqwest::multipart::Part::bytes(zip.to_vec()).file_name("source.zip"),
            );
        let resp = self
            .http
            .post(format!("{}/repository/{}/upload", self.base_url, repo))
            .bearer_auth(&token)
            .multipart(form)
            .send()
            .await?;
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!(
                "golang upload {}@{} failed ({}): {}",
                module,
                version,
                status,
                body
            );
        }
        Ok(())
    }

    pub async fn golang_get(&self, repo: &str, path: &str) -> Result<Vec<u8>> {
        let token = self.bearer_token().await?;
        let resp = self
            .http
            .get(format!("{}/repository/{}/{}", self.base_url, repo, path))
            .bearer_auth(&token)
            .send()
            .await?;
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!("golang get {}/{} failed ({}): {}", repo, path, status, body);
        }
        Ok(resp.bytes().await?.to_vec())
    }

    // --- Helm ops ---

    pub async fn helm_upload(&self, repo: &str, filename: &str, data: Vec<u8>) -> Result<()> {
        let token = self.bearer_token().await?;
        let form = reqwest::multipart::Form::new().part(
            "chart",
            reqwest::multipart::Part::bytes(data).file_name(filename.to_string()),
        );
        let resp = self
            .http
            .post(format!("{}/repository/{}/upload", self.base_url, repo))
            .bearer_auth(&token)
            .multipart(form)
            .send()
            .await?;
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!(
                "helm upload {}/{} failed ({}): {}",
                repo,
                filename,
                status,
                body
            );
        }
        Ok(())
    }

    pub async fn helm_download(&self, repo: &str, filename: &str) -> Result<Vec<u8>> {
        let token = self.bearer_token().await?;
        let resp = self
            .http
            .get(format!(
                "{}/repository/{}/charts/{}",
                self.base_url, repo, filename
            ))
            .bearer_auth(&token)
            .send()
            .await?;
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!(
                "helm download {}/{} failed ({}): {}",
                repo,
                filename,
                status,
                body
            );
        }
        Ok(resp.bytes().await?.to_vec())
    }

    // --- Cargo ops ---

    pub async fn cargo_publish(&self, repo: &str, body: Vec<u8>) -> Result<()> {
        let token = self.bearer_token().await?;
        let resp = self
            .http
            .put(format!(
                "{}/repository/{}/api/v1/crates/new",
                self.base_url, repo
            ))
            .bearer_auth(&token)
            .header("Content-Type", "application/octet-stream")
            .body(body)
            .send()
            .await?;
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!("cargo publish to {} failed ({}): {}", repo, status, body);
        }
        Ok(())
    }

    pub async fn list_artifacts(&self, repo: &str) -> Result<Vec<ArtifactResponse>> {
        let token = self.bearer_token().await?;
        let resp = self
            .http
            .get(format!(
                "{}/api/v1/repositories/{}/artifacts?limit=10000",
                self.base_url, repo
            ))
            .bearer_auth(&token)
            .send()
            .await?;
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!("list artifacts '{}' failed ({}): {}", repo, status, body);
        }
        let browse: BrowseResponse = resp.json().await?;
        Ok(browse.artifacts)
    }

    pub async fn search_artifacts(&self, repo: &str, query: &str) -> Result<Vec<ArtifactResponse>> {
        let token = self.bearer_token().await?;
        let resp = self
            .http
            .get(format!(
                "{}/api/v1/repositories/{}/artifacts?q={}&limit=10000",
                self.base_url, repo, query
            ))
            .bearer_auth(&token)
            .send()
            .await?;
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!("search artifacts '{}' failed ({}): {}", repo, status, body);
        }
        let browse: BrowseResponse = resp.json().await?;
        Ok(browse.artifacts)
    }

    // --- npm ops ---

    pub async fn publish_npm(
        &self,
        repo: &str,
        name: &str,
        version: &str,
        tarball_data: &[u8],
    ) -> Result<()> {
        let token = self.bearer_token().await?;
        let b64 = base64::engine::general_purpose::STANDARD.encode(tarball_data);
        let bare_name = name.rsplit('/').next().unwrap_or(name);
        let filename = format!("{}-{}.tgz", bare_name, version);
        let body = serde_json::json!({
            "name": name,
            "versions": {
                version: {
                    "name": name,
                    "version": version,
                    "dist": {
                        "tarball": format!("placeholder/{filename}"),
                    }
                }
            },
            "dist-tags": {
                "latest": version,
            },
            "_attachments": {
                &filename: {
                    "data": b64,
                    "length": tarball_data.len(),
                }
            }
        });

        let url = format!("{}/repository/{}/{}", self.base_url, repo, name);
        let resp = self
            .http
            .put(&url)
            .bearer_auth(&token)
            .json(&body)
            .send()
            .await?;
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!(
                "npm publish {}@{} failed ({}): {}",
                name,
                version,
                status,
                body
            );
        }
        Ok(())
    }

    // --- Docker V2 ops (using two-segment routes: /v2/{repo}/{image}/...) ---

    pub async fn docker_push_blob(
        &self,
        repo: &str,
        image: &str,
        data: Vec<u8>,
        digest: &str,
    ) -> Result<()> {
        // POST to start upload
        let resp = self
            .http
            .post(format!(
                "{}/v2/{}/{}/blobs/uploads/",
                self.base_url, repo, image
            ))
            .header("Authorization", format!("Basic {}", self.basic_auth))
            .body(Vec::new())
            .send()
            .await?;
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!("docker upload start failed ({}): {}", status, body);
        }
        let location = resp
            .headers()
            .get("location")
            .and_then(|v| v.to_str().ok())
            .ok_or_else(|| anyhow::anyhow!("no Location header in upload start response"))?
            .to_string();

        // Resolve location (may be relative)
        let upload_url = if location.starts_with("http") {
            location
        } else {
            format!("{}{}", self.base_url, location)
        };

        // PATCH to upload data
        let resp = self
            .http
            .patch(&upload_url)
            .header("Authorization", format!("Basic {}", self.basic_auth))
            .header("Content-Type", "application/octet-stream")
            .body(data)
            .send()
            .await?;
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!("docker upload patch failed ({}): {}", status, body);
        }
        let location = resp
            .headers()
            .get("location")
            .and_then(|v| v.to_str().ok())
            .ok_or_else(|| anyhow::anyhow!("no Location header in patch response"))?
            .to_string();

        let put_url = if location.starts_with("http") {
            location
        } else {
            format!("{}{}", self.base_url, location)
        };

        // PUT to complete upload
        let separator = if put_url.contains('?') { "&" } else { "?" };
        let resp = self
            .http
            .put(format!("{}{}digest={}", put_url, separator, digest))
            .header("Authorization", format!("Basic {}", self.basic_auth))
            .body(Vec::new())
            .send()
            .await?;
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!("docker upload complete failed ({}): {}", status, body);
        }
        Ok(())
    }

    pub async fn docker_push_manifest(
        &self,
        repo: &str,
        image: &str,
        reference: &str,
        manifest: &[u8],
        media_type: &str,
    ) -> Result<String> {
        let resp = self
            .http
            .put(format!(
                "{}/v2/{}/{}/manifests/{}",
                self.base_url, repo, image, reference
            ))
            .header("Authorization", format!("Basic {}", self.basic_auth))
            .header("Content-Type", media_type)
            .body(manifest.to_vec())
            .send()
            .await?;
        let status = resp.status();
        let digest = resp
            .headers()
            .get("docker-content-digest")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!("docker push manifest failed ({}): {}", status, body);
        }
        Ok(digest)
    }

    pub async fn docker_pull_manifest(
        &self,
        repo: &str,
        image: &str,
        reference: &str,
    ) -> Result<(Vec<u8>, String)> {
        let resp = self
            .http
            .get(format!(
                "{}/v2/{}/{}/manifests/{}",
                self.base_url, repo, image, reference
            ))
            .header("Authorization", format!("Basic {}", self.basic_auth))
            .header("Accept", MANIFEST_ACCEPT_ALL)
            .send()
            .await?;
        let status = resp.status();
        let content_type = resp
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!("docker pull manifest failed ({}): {}", status, body);
        }
        let body = resp.bytes().await?.to_vec();
        Ok((body, content_type))
    }

    pub async fn docker_pull_blob(&self, repo: &str, image: &str, digest: &str) -> Result<Vec<u8>> {
        let resp = self
            .http
            .get(format!(
                "{}/v2/{}/{}/blobs/{}",
                self.base_url, repo, image, digest
            ))
            .header("Authorization", format!("Basic {}", self.basic_auth))
            .send()
            .await?;
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!("docker pull blob failed ({}): {}", status, body);
        }
        Ok(resp.bytes().await?.to_vec())
    }

    pub async fn docker_head_blob(
        &self,
        repo: &str,
        image: &str,
        digest: &str,
    ) -> Result<(u16, u64)> {
        let resp = self
            .http
            .head(format!(
                "{}/v2/{}/{}/blobs/{}",
                self.base_url, repo, image, digest
            ))
            .header("Authorization", format!("Basic {}", self.basic_auth))
            .send()
            .await?;
        let status = resp.status().as_u16();
        let size = resp
            .headers()
            .get("content-length")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(0);
        Ok((status, size))
    }

    pub async fn docker_delete_manifest(
        &self,
        repo: &str,
        image: &str,
        reference: &str,
    ) -> Result<u16> {
        let resp = self
            .http
            .delete(format!(
                "{}/v2/{}/{}/manifests/{}",
                self.base_url, repo, image, reference
            ))
            .header("Authorization", format!("Basic {}", self.basic_auth))
            .send()
            .await?;
        Ok(resp.status().as_u16())
    }

    pub async fn docker_head_manifest(
        &self,
        repo: &str,
        image: &str,
        reference: &str,
    ) -> Result<(u16, String, String)> {
        let resp = self
            .http
            .head(format!(
                "{}/v2/{}/{}/manifests/{}",
                self.base_url, repo, image, reference
            ))
            .header("Authorization", format!("Basic {}", self.basic_auth))
            .header("Accept", MANIFEST_ACCEPT_ALL)
            .send()
            .await?;
        let status = resp.status().as_u16();
        let content_type = resp
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();
        let digest = resp
            .headers()
            .get("docker-content-digest")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();
        Ok((status, content_type, digest))
    }

    pub async fn docker_list_tags(&self, repo: &str, image: &str) -> Result<Vec<String>> {
        let resp = self
            .http
            .get(format!("{}/v2/{}/{}/tags/list", self.base_url, repo, image))
            .header("Authorization", format!("Basic {}", self.basic_auth))
            .send()
            .await?;
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!("docker list tags failed ({}): {}", status, body);
        }
        let tag_list: TagListResponse = resp.json().await?;
        Ok(tag_list.tags)
    }

    pub async fn docker_catalog(&self) -> Result<Vec<String>> {
        let resp = self
            .http
            .get(format!("{}/v2/_catalog", self.base_url))
            .header("Authorization", format!("Basic {}", self.basic_auth))
            .send()
            .await?;
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!("docker catalog failed ({}): {}", status, body);
        }
        let catalog: CatalogResponse = resp.json().await?;
        Ok(catalog.repositories)
    }

    /// `GET /repository/{repo}/v2/_catalog` — list image names within a
    /// specific depot repo.
    pub async fn docker_repo_catalog(&self, repo: &str) -> Result<Vec<String>> {
        let resp = self
            .http
            .get(format!("{}/repository/{}/v2/_catalog", self.base_url, repo))
            .header("Authorization", format!("Basic {}", self.basic_auth))
            .send()
            .await?;
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!("docker repo catalog failed ({}): {}", status, body);
        }
        let catalog: CatalogResponse = resp.json().await?;
        Ok(catalog.repositories)
    }

    pub async fn docker_pull_manifest_accept(
        &self,
        repo: &str,
        image: &str,
        reference: &str,
        accept: &str,
    ) -> Result<(Vec<u8>, String, String)> {
        let resp = self
            .http
            .get(format!(
                "{}/v2/{}/{}/manifests/{}",
                self.base_url, repo, image, reference
            ))
            .header("Authorization", format!("Basic {}", self.basic_auth))
            .header("Accept", accept)
            .send()
            .await?;
        let status = resp.status();
        let content_type = resp
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();
        let digest_header = resp
            .headers()
            .get("docker-content-digest")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!("docker pull manifest failed ({}): {}", status, body);
        }
        let body = resp.bytes().await?.to_vec();
        Ok((body, content_type, digest_header))
    }

    pub async fn docker_monolithic_upload(
        &self,
        repo: &str,
        image: &str,
        data: Vec<u8>,
        digest: &str,
    ) -> Result<u16> {
        let resp = self
            .http
            .post(format!(
                "{}/v2/{}/{}/blobs/uploads/?digest={}",
                self.base_url, repo, image, digest
            ))
            .header("Authorization", format!("Basic {}", self.basic_auth))
            .header("Content-Type", "application/octet-stream")
            .body(data)
            .send()
            .await?;
        Ok(resp.status().as_u16())
    }

    pub async fn docker_mount_blob(
        &self,
        repo: &str,
        image: &str,
        digest: &str,
        from_repo: &str,
    ) -> Result<u16> {
        let resp = self
            .http
            .post(format!(
                "{}/v2/{}/{}/blobs/uploads/?mount={}&from={}",
                self.base_url, repo, image, digest, from_repo
            ))
            .header("Authorization", format!("Basic {}", self.basic_auth))
            .body(Vec::new())
            .send()
            .await?;
        Ok(resp.status().as_u16())
    }

    pub async fn docker_push_manifest_raw(
        &self,
        repo: &str,
        image: &str,
        reference: &str,
        manifest: &[u8],
        content_type: &str,
    ) -> Result<u16> {
        let resp = self
            .http
            .put(format!(
                "{}/v2/{}/{}/manifests/{}",
                self.base_url, repo, image, reference
            ))
            .header("Authorization", format!("Basic {}", self.basic_auth))
            .header("Content-Type", content_type)
            .body(manifest.to_vec())
            .send()
            .await?;
        Ok(resp.status().as_u16())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn client_new_trims_trailing_slash() {
        let client = DepotClient::new("http://localhost:8080/", "admin", "admin", false).unwrap();
        assert_eq!(client.base_url, "http://localhost:8080");
    }

    #[test]
    fn client_new_no_trailing_slash() {
        let client = DepotClient::new("http://localhost:8080", "admin", "admin", false).unwrap();
        assert_eq!(client.base_url, "http://localhost:8080");
    }

    #[test]
    fn client_basic_auth_encoding() {
        let client = DepotClient::new("http://localhost", "user", "pass", false).unwrap();
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(&client.basic_auth)
            .unwrap();
        assert_eq!(String::from_utf8(decoded).unwrap(), "user:pass");
    }

    #[test]
    fn client_basic_auth_admin() {
        let client = DepotClient::new("http://localhost", "admin", "admin", false).unwrap();
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(&client.basic_auth)
            .unwrap();
        assert_eq!(String::from_utf8(decoded).unwrap(), "admin:admin");
    }

    #[test]
    fn client_is_clone() {
        let client = DepotClient::new("http://localhost", "admin", "admin", false).unwrap();
        let clone = client.clone();
        assert_eq!(clone.base_url, client.base_url);
        assert_eq!(clone.username, client.username);
    }

    #[test]
    fn create_repo_request_serialization() {
        let req = CreateRepoRequest {
            name: "test".to_string(),
            repo_type: "hosted".to_string(),
            format: "raw".to_string(),
            store: "default".to_string(),
            upstream_url: None,
            cache_ttl_secs: None,
            members: None,
            listen: None,
        };
        let json = serde_json::to_value(&req).unwrap();
        assert_eq!(json["name"], "test");
        assert_eq!(json["repo_type"], "hosted");
        assert_eq!(json["format"], "raw");
        // None fields should be absent
        assert!(json.get("upstream_url").is_none());
        assert!(json.get("cache_ttl_secs").is_none());
        assert!(json.get("members").is_none());
    }

    #[test]
    fn create_repo_request_with_optional_fields() {
        let req = CreateRepoRequest {
            name: "cache".to_string(),
            repo_type: "cache".to_string(),
            format: "docker".to_string(),
            store: "default".to_string(),
            upstream_url: Some("https://registry.example.com".to_string()),
            cache_ttl_secs: Some(3600),
            members: Some(vec!["a".to_string(), "b".to_string()]),
            listen: Some("0.0.0.0:5000".to_string()),
        };
        let json = serde_json::to_value(&req).unwrap();
        assert_eq!(json["upstream_url"], "https://registry.example.com");
        assert_eq!(json["cache_ttl_secs"], 3600);
        assert_eq!(json["members"][0], "a");
        assert_eq!(json["listen"], "0.0.0.0:5000");
    }
}
