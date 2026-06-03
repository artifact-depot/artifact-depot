---
title: Runtime Settings
parent: Configuration
nav_order: 2
---

# Runtime Settings

These settings live in the KV store, not the TOML config file. They are
managed via the REST API or the **Settings** page in the web UI:

- `GET /api/v1/settings` -- read current settings
- `PUT /api/v1/settings` -- update settings (admin only)

Changes propagate to every cluster instance within 30 seconds.

## Settings table

| Setting | Type | Default | Description |
|---------|------|---------|-------------|
| `access_log` | bool | `true` | Log all HTTP requests (method, path, status, duration) |
| `max_artifact_bytes` | integer | `34359738368` (32 GiB) | Maximum upload body size |
| `default_docker_repo` | string or null | `null` | Route `/v2/` requests to this repo name |
| `cors` | object or null | `null` | CORS configuration (`allowed_origins`, `allowed_methods`, `allowed_headers`, `max_age_secs`) |
| `rate_limit` | object or null | `null` | Rate limiting (`requests_per_second`, `burst_size`) per client IP |
| `gc_interval_secs` | integer or null | `86400` (24h) | Blob garbage collection interval; `null` disables GC |
| `gc_min_interval_secs` | integer or null | `3600` (1h) | Minimum gap between GC runs; prevents premature candidate deletion |
| `gc_max_orphan_candidates` | integer or null | `1000000` | Cap on the orphan-candidate map carried across GC passes |
| `max_docker_chunk_count` | integer | `10000` | Maximum chunks per Docker chunked upload session |
| `jwt_expiry_secs` | integer | `86400` (24h) | JWT bearer-token lifetime; must be `>= 60` |
| `jwt_rotation_interval_secs` | integer | `604800` (7d) | JWT signing-key rotation interval; must be `>= 3600` and `>= jwt_expiry_secs` |
| `state_scan_interval_ms` | integer | `1000` (1s) | State-scanner poll interval for cross-instance state changes |
| `task_retention_secs` | integer | `86400` (24h) | Retention for completed / failed / cancelled tasks before auto-reap |
| `logging` | object or null | `null` | Override the TOML `[logging]` config at runtime; currently only carries `otlp_endpoint`. Takes effect after restart. |
| `tracing_endpoint` | string or null | `null` | Override the TOML `[tracing].otlp_endpoint` (takes effect after restart) |
| `base_url` | string or null | `null` | External base URL (`scheme://host[:port]`, no path) used to build absolute URLs in artifact metadata (npm `dist.tarball`, Docker `WWW-Authenticate` realm, Nexus-compat asset URLs). Hard override applied regardless of request headers — the equivalent of Nexus's "Base URL" capability. When `null`, the origin is derived per-request from `X-Forwarded-Proto`/`Host`, falling back to the scheme of the listener that accepted the request (`http` for the plain-HTTP listener, `https` for the TLS listener). Set it to pin a canonical public URL, e.g. to force `http://` while a TLS certificate is being fixed. |

## Blob Store Management

Blob stores are managed at runtime via the REST API or the
**Stores** page in the web UI:

- `POST /api/v1/stores` -- create a new blob store
- `GET /api/v1/stores` -- list all stores
- `POST /api/v1/stores/{name}/check` -- probe a store for reachability
- `DELETE /api/v1/stores/{name}` -- delete a store

A default store can be auto-created on first boot via the
`[initialization]` TOML section (see
[TOML Reference -- `[initialization]`](toml-reference.md#initialization)).
If no `[initialization]` section is present, the server logs a warning
and expects at least one store to be created via the API before
repositories can accept writes. By convention the first store is named
`default`.

### File store

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `type` | string | -- | Must be `"file"` |
| `root` | path | **required** | Root directory for blob storage |
| `sync` | bool | `true` | Fsync files and parent directories after writes |
| `io_size` | integer | `1024` | Read/write buffer size in KiB |
| `direct_io` | bool | `false` | Use `O_DIRECT` (bypasses page cache) |

### S3 store

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `type` | string | -- | Must be `"s3"` |
| `bucket` | string | **required** | S3 bucket name |
| `endpoint` | string | _(none)_ | Custom endpoint (MinIO, Garage, etc.) |
| `region` | string | `"us-east-1"` | AWS region |
| `prefix` | string | _(none)_ | Key prefix within the bucket |
| `access_key` | string | _(none)_ | AWS access key; falls back to default credential chain |
| `secret_key` | string | _(none)_ | AWS secret key |
| `max_retries` | integer | `3` | Retry attempts |
| `connect_timeout_secs` | integer | `3` | TCP connection timeout |
| `read_timeout_secs` | integer | `0` | Per-request read timeout; `0` disables |
| `retry_mode` | string | `"standard"` | `"standard"` or `"adaptive"` |
