#![deny(clippy::unwrap_used)]
#![deny(clippy::expect_used)]

// SPDX-FileCopyrightText: 2026 Artifact Depot Contributors
//
// SPDX-License-Identifier: Apache-2.0

use clap::Parser;
use std::net::SocketAddr;
use std::path::PathBuf;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use depot_core::service;
use depot_core::update::UpdateSender;
use depot_server::server::worker::update as update_worker;
use depot_server::server::worker::{
    blob_reaper, cleanup, cluster, docker_listeners, materializer, state_scanner,
};
use depot_server::server::{self, config};

struct OtelGuards {
    tracer_provider: opentelemetry_sdk::trace::SdkTracerProvider,
    logger_provider: opentelemetry_sdk::logs::SdkLoggerProvider,
}

#[derive(Parser)]
#[command(name = "depot", about = "Artifact Depot — artifact repository server", version = depot_server::VERSION)]
struct Cli {
    /// Path to configuration file
    #[arg(short, long, default_value = "depotd.toml")]
    config: PathBuf,
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    let cfg_path = cli.config;

    // Install the default TLS crypto provider for rustls.
    rustls::crypto::aws_lc_rs::default_provider()
        .install_default()
        .map_err(|_| anyhow::anyhow!("failed to install rustls crypto provider"))?;

    let cfg = config::load(&cfg_path)?;

    let mut rt_builder = tokio::runtime::Builder::new_multi_thread();
    rt_builder.enable_all();
    if cfg.worker_threads > 0 {
        rt_builder.worker_threads(cfg.worker_threads);
    }
    let rt = rt_builder.build()?;

    rt.block_on(async_main(cfg))
}

async fn async_main(cfg: config::Config) -> anyhow::Result<()> {
    let instance_id = uuid::Uuid::new_v4().to_string();

    // Build a layered tracing subscriber: fmt output + OpenTelemetry.
    //
    // The fmt layer (stderr) uses a per-layer Targets filter to suppress
    // per-request events (target "depot.request") — those only go to the
    // OTel logging endpoints. System events pass through to both.
    let _otel_guards: Option<OtelGuards>;

    {
        use tracing_subscriber::layer::SubscriberExt;
        use tracing_subscriber::util::SubscriberInitExt;
        use tracing_subscriber::Layer;

        // When OTLP tracing is configured, allow debug-level spans through the
        // global filter so KV/blob child spans reach the OTel exporter. The fmt
        // layer has its own per-layer filter to keep stderr at info.
        let otlp_enabled = cfg
            .tracing
            .as_ref()
            .and_then(|t| t.otlp_endpoint.as_ref())
            .is_some();
        let default_filter = if otlp_enabled {
            "depot=debug,tower_http=info"
        } else {
            "depot=info,tower_http=info"
        };
        let env_filter = tracing_subscriber::EnvFilter::try_from_default_env()
            .unwrap_or_else(|_| default_filter.into());

        // Per-layer filter: keep stderr at info and suppress depot.request events.
        let fmt_filter = tracing_subscriber::filter::Targets::new()
            .with_default(tracing::level_filters::LevelFilter::INFO)
            .with_target("depot.request", tracing::level_filters::LevelFilter::OFF);
        let fmt_layer = tracing_subscriber::fmt::layer().with_filter(fmt_filter);

        let registry = tracing_subscriber::registry()
            .with(env_filter)
            .with(fmt_layer);

        let (guards, registry) = {
            use opentelemetry_otlp::WithExportConfig;

            let tracing_cfg = cfg.tracing.as_ref();
            let otlp_endpoint = tracing_cfg.and_then(|t| t.otlp_endpoint.clone());

            if let Some(ref endpoint) = otlp_endpoint {
                let service_name = tracing_cfg
                    .and_then(|t| t.service_name.clone())
                    .unwrap_or_else(|| "depot".to_string());

                let mut resource_attrs = vec![
                    opentelemetry::KeyValue::new("service.version", depot_server::VERSION),
                    opentelemetry::KeyValue::new("service.instance.id", instance_id.clone()),
                ];
                if let Some(ref env) = tracing_cfg.and_then(|t| t.deployment_environment.clone()) {
                    resource_attrs.push(opentelemetry::KeyValue::new(
                        "deployment.environment",
                        env.clone(),
                    ));
                }

                let resource = opentelemetry_sdk::Resource::builder()
                    .with_service_name(service_name)
                    .with_attributes(resource_attrs)
                    .build();

                // Trace exporter + provider.
                let span_exporter = opentelemetry_otlp::SpanExporter::builder()
                    .with_tonic()
                    .with_endpoint(endpoint)
                    .build()?;

                let mut tracer_builder = opentelemetry_sdk::trace::SdkTracerProvider::builder()
                    .with_batch_exporter(span_exporter)
                    .with_resource(resource.clone());

                // Sampling: max_traces_per_sec (adaptive) takes precedence over
                // sampling_ratio (fixed).  When neither is set, default to 100
                // traces/sec — full coverage at low load, bounded at high load.
                if let Some(ratio) = tracing_cfg.and_then(|t| t.sampling_ratio) {
                    if tracing_cfg.and_then(|t| t.max_traces_per_sec).is_none() {
                        tracer_builder = tracer_builder.with_sampler(
                            opentelemetry_sdk::trace::Sampler::TraceIdRatioBased(ratio),
                        );
                    }
                }
                let max_tps = tracing_cfg.and_then(|t| t.max_traces_per_sec).or_else(|| {
                    // Use adaptive sampling by default unless a fixed ratio was set.
                    if tracing_cfg.and_then(|t| t.sampling_ratio).is_none() {
                        Some(100)
                    } else {
                        None
                    }
                });
                if let Some(tps) = max_tps {
                    if tps > 0 {
                        tracer_builder = tracer_builder.with_sampler(
                            depot_server::server::infra::sampler::RateLimitingSampler::new(tps),
                        );
                    }
                }

                let tracer_provider = tracer_builder.build();
                let tracer =
                    opentelemetry::trace::TracerProvider::tracer(&tracer_provider, "depot");
                let otel_trace_layer = tracing_opentelemetry::layer().with_tracer(tracer);

                // Log exporter + provider.
                let log_exporter = opentelemetry_otlp::LogExporter::builder()
                    .with_tonic()
                    .with_endpoint(endpoint)
                    .build()?;

                let logger_provider = opentelemetry_sdk::logs::SdkLoggerProvider::builder()
                    .with_batch_exporter(log_exporter)
                    .with_resource(resource)
                    .build();

                let otel_log_layer =
                    opentelemetry_appender_tracing::layer::OpenTelemetryTracingBridge::new(
                        &logger_provider,
                    );

                // Register W3C TraceContext propagator for context propagation.
                opentelemetry::global::set_text_map_propagator(
                    opentelemetry_sdk::propagation::TraceContextPropagator::new(),
                );

                eprintln!("OpenTelemetry OTLP export enabled: {endpoint}");
                (
                    Some(OtelGuards {
                        tracer_provider,
                        logger_provider,
                    }),
                    registry
                        .with(Some(otel_trace_layer))
                        .with(Some(otel_log_layer)),
                )
            } else {
                (
                    None,
                    registry
                        .with(None::<tracing_opentelemetry::OpenTelemetryLayer<_, _>>)
                        .with(
                            None::<
                                opentelemetry_appender_tracing::layer::OpenTelemetryTracingBridge<
                                    opentelemetry_sdk::logs::SdkLoggerProvider,
                                    opentelemetry_sdk::logs::SdkLogger,
                                >,
                            >,
                        ),
                )
            }
        };

        _otel_guards = guards;

        registry.init();
    }

    if let Some(ref https) = cfg.https {
        tracing::info!("starting depot on https://{}", https.listen);
    } else if let Some(ref http) = cfg.http {
        tracing::info!("starting depot on http://{}", http.listen);
    }

    let metrics_handle = server::infra::metrics::install_recorder()?;

    let mut state = server::AppState::new(&cfg, instance_id).await?;

    // Detect a fresh KV BEFORE running any bootstrap so we can decide whether
    // to apply the declarative [initialization] section.
    let was_fresh = server::config::init::kv_is_fresh(state.repo.kv.as_ref()).await?;

    // If [initialization] declares an admin user, skip the default admin
    // generation so we don't print a throwaway random password that's about
    // to be overwritten.
    let skip_default_admin = cfg
        .initialization
        .as_ref()
        .is_some_and(|i| i.users.iter().any(|u| u.username == "admin"));

    // Bootstrap roles if none exist.
    if server::config::bootstrap::bootstrap_roles(state.repo.kv.as_ref()).await? {
        eprintln!("  Roles bootstrapped: admin, read-only");
    }

    // Bootstrap admin user (and anonymous) if no users exist.
    if let Some(password) = server::config::bootstrap::bootstrap_users(
        state.repo.kv.as_ref(),
        cfg.default_admin_password.clone(),
        skip_default_admin,
    )
    .await?
    {
        eprintln!();
        eprintln!("========================================");
        eprintln!("  Admin user created on first start");
        eprintln!("  Username: admin");
        eprintln!("  Password: {}", password);
        eprintln!("========================================");
        eprintln!();
    }

    // Apply declarative initialization, if configured and the KV was fresh.
    if was_fresh {
        if let Some(ref init_cfg) = cfg.initialization {
            if !init_cfg.is_empty() {
                let summary = server::config::init::apply_initialization(&state, init_cfg).await?;
                eprintln!(
                    "  Initialization applied: {} roles, {} stores, {} repos, {} users{}",
                    summary.roles_created,
                    summary.stores_created,
                    summary.repos_created,
                    summary.users_created,
                    if summary.settings_overridden {
                        ", settings overridden"
                    } else {
                        ""
                    },
                );
            }
        }
    }

    // Clean up any stale upload sessions from a previous crash.
    match service::cleanup_upload_sessions(
        state.repo.kv.as_ref(),
        &state.repo.stores,
        std::time::Duration::from_secs(0),
    )
    .await
    {
        Ok(0) => {}
        Ok(n) => tracing::info!(count = n, "cleaned up stale upload sessions"),
        Err(e) => tracing::warn!(error = %e, "failed to clean up upload sessions"),
    }

    let cancel = CancellationToken::new();
    let mut worker_handles: Vec<(&'static str, JoinHandle<()>)> = Vec::new();

    // Register this instance in the cluster and start heartbeat loop.
    {
        let hostname = gethostname::gethostname().to_string_lossy().into_owned();
        cluster::register_instance(state.repo.kv.as_ref(), &state.bg.instance_id, &hostname)
            .await?;
        tracing::info!(instance_id = %state.bg.instance_id, "registered cluster instance");
        let kv = state.repo.kv.clone();
        let instance_id = state.bg.instance_id.clone();
        let h = tokio::spawn(cluster::run_heartbeat_loop(kv, instance_id, cancel.clone()));
        worker_handles.push(("heartbeat", h));
    }

    // Spawn combined atime + dir-entry + store-stats update worker.
    let (update_sender, update_rx) = UpdateSender::new(4096);
    state.repo.updater = update_sender;
    {
        let kv = state.repo.kv.clone();
        let h = tokio::spawn(update_worker::run_update_worker(
            update_rx,
            kv,
            cancel.clone(),
        ));
        worker_handles.push(("update_worker", h));
        tracing::info!("update worker started");
    }

    // Spawn settings refresh worker.
    {
        let kv = state.repo.kv.clone();
        let settings_handle = state.settings.clone();
        let rate_limiter = state.rate_limiter.clone();
        let jwt_secret = state.auth.jwt_secret.clone();
        let h = tokio::spawn(
            depot_server::server::config::settings::run_settings_refresh(
                kv,
                settings_handle,
                rate_limiter,
                jwt_secret,
                cancel.clone(),
            ),
        );
        worker_handles.push(("settings_refresh", h));
        tracing::info!("settings refresh worker started");
    }

    // Spawn JWT secret rotation worker.
    {
        let kv = state.repo.kv.clone();
        let jwt_secret = state.auth.jwt_secret.clone();
        let settings = state.settings.clone();
        let h = tokio::spawn(depot_server::server::auth::run_jwt_rotation(
            kv,
            jwt_secret,
            settings,
            cancel.clone(),
        ));
        worker_handles.push(("jwt_rotation", h));
        tracing::info!("JWT rotation worker started");
    }

    // On single-node backends (redb), clear any stale leases left by a
    // previous instance that crashed or was killed. No other instance could
    // legitimately hold them.
    if state.repo.kv.is_single_node() {
        use depot_core::store::keys;
        let kv = state.repo.kv.as_ref();
        let lease_pks = kv
            .scan_prefix(
                keys::TABLE_LEASES,
                std::borrow::Cow::Borrowed(keys::SINGLE_PK),
                std::borrow::Cow::Borrowed(""),
                100,
            )
            .await;
        if let Ok(result) = lease_pks {
            for (sk, _) in &result.items {
                let _ = kv
                    .delete(
                        keys::TABLE_LEASES,
                        std::borrow::Cow::Borrowed(keys::SINGLE_PK),
                        std::borrow::Cow::Borrowed(sk),
                    )
                    .await;
                tracing::info!(lease = sk, "cleared stale lease on startup");
            }
        }
    }

    // Spawn blob GC worker (interval from settings, default 24h).
    {
        let kv = state.repo.kv.clone();
        let stores = state.repo.stores.clone();
        let instance_id = state.bg.instance_id.clone();
        let settings = state.settings.clone();
        let tasks = state.bg.tasks.clone();
        let updater = state.repo.updater.clone();
        let gc_state = state.bg.gc_state.clone();
        let h = tokio::spawn(blob_reaper::run_blob_reaper(
            kv,
            stores,
            instance_id,
            cancel.clone(),
            settings,
            tasks,
            updater,
            gc_state,
        ));
        worker_handles.push(("blob_gc", h));
        tracing::info!("blob GC worker started");
    }

    // Spawn model materializer — maintains in-memory snapshot from event bus.
    {
        let model = state.bg.model.clone();
        let event_bus = state.bg.event_bus.clone();
        let kv = state.repo.kv.clone();
        let tasks = state.bg.tasks.clone();
        let settings = state.settings.clone();
        let h = tokio::spawn(materializer::run_model_materializer(
            model,
            event_bus,
            kv,
            tasks,
            settings,
            cancel.clone(),
        ));
        worker_handles.push(("model_materializer", h));
        tracing::info!("model materializer started");
    }

    // Spawn Docker port listener manager — reconciles dedicated Docker port
    // listeners against repo configs every 30 seconds.
    {
        let h = tokio::spawn(docker_listeners::run_docker_listener_refresh(
            state.clone(),
            cfg.https.as_ref().map(|h| h.tls_cert.clone()),
            cfg.https.as_ref().map(|h| h.tls_key.clone()),
            cancel.clone(),
        ));
        worker_handles.push(("docker_listeners", h));
        tracing::info!("docker listener manager started");
    }

    // Spawn state scanner — polls KV for cross-instance repo/store/task
    // changes and emits events to the local bus.
    {
        let kv = state.repo.kv.clone();
        let event_bus = state.bg.event_bus.clone();
        let settings = state.settings.clone();
        let scan_trigger = state.bg.scan_trigger.clone();
        let h = tokio::spawn(state_scanner::run_state_scanner(
            kv,
            event_bus,
            settings,
            scan_trigger,
            cancel.clone(),
        ));
        worker_handles.push(("state_scanner", h));
        tracing::info!("state scanner started");
    }

    // Spawn cleanup loop — drains deleted repos and reaps tasks (orphans +
    // TTL + drain-deleting).
    {
        let kv = state.repo.kv.clone();
        let stores = state.repo.stores.clone();
        let settings = state.settings.clone();
        let h = tokio::spawn(cleanup::run_cleanup_loop(
            kv,
            stores,
            settings,
            cancel.clone(),
        ));
        worker_handles.push(("cleanup", h));
        tracing::info!("cleanup loop started");
    }

    // Spawn supervisor that monitors all background workers for unexpected exits.
    {
        let cancel_clone = cancel.clone();
        tokio::spawn(async move {
            supervise_workers(worker_handles, cancel_clone).await;
        });
    }

    // Spawn a dedicated metrics listener if configured.
    if let Some(ref metrics_addr) = cfg.metrics_listen {
        let addr: std::net::SocketAddr = metrics_addr.parse()?;
        let metrics_router = server::build_metrics_router(metrics_handle.clone());
        let listener = tokio::net::TcpListener::bind(addr).await?;
        tracing::info!("metrics listener on http://{}", addr);
        tokio::spawn(async move {
            if let Err(e) = axum::serve(listener, metrics_router).await {
                tracing::error!("metrics listener error: {}", e);
            }
        });
    }

    let app = server::build_router(state.clone(), Some(metrics_handle));

    if let Some(ref https) = cfg.https {
        // When TLS is enabled, optionally spawn a plain HTTP listener as a
        // secondary (non-blocking) listener.
        if let Some(ref http) = cfg.http {
            let http_app = server::build_router(state.clone(), None);
            let addr: std::net::SocketAddr = http.listen.parse()?;
            tracing::info!("plain HTTP listener on http://{}", addr);
            let listener = tokio::net::TcpListener::bind(addr).await?;
            tokio::spawn(async move {
                if let Err(e) = axum::serve(listener, http_app).await {
                    tracing::error!("HTTP listener error: {}", e);
                }
            });
        }

        let tls_config =
            axum_server::tls_rustls::RustlsConfig::from_pem_file(&https.tls_cert, &https.tls_key)
                .await?;

        let addr: std::net::SocketAddr = https.listen.parse()?;
        tracing::info!("listening on https://{}", addr);

        let handle = axum_server::Handle::new();
        let h = handle.clone();
        let cancel_clone = cancel.clone();
        tokio::spawn(async move {
            shutdown_signal(cancel_clone).await;
            h.graceful_shutdown(Some(std::time::Duration::from_secs(3)));
        });

        let mut server = axum_server::bind_rustls(addr, tls_config);
        server
            .http_builder()
            .http2()
            .initial_stream_window_size(16 * 1024 * 1024)
            .initial_connection_window_size(16 * 1024 * 1024);
        server
            .handle(handle)
            .serve(app.into_make_service_with_connect_info::<SocketAddr>())
            .await?;
    } else if let Some(ref http) = cfg.http {
        let listener = tokio::net::TcpListener::bind(&http.listen).await?;
        tracing::info!("listening on http://{}", listener.local_addr()?);
        axum::serve(
            listener,
            app.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .with_graceful_shutdown(async move {
            shutdown_signal(cancel).await;
            // Give in-flight requests a few seconds, then shut down.
            tokio::time::sleep(std::time::Duration::from_secs(3)).await;
        })
        .await?;
    }

    // Deregister this instance from the cluster on shutdown.
    if let Err(e) =
        cluster::deregister_instance(state.repo.kv.as_ref(), &state.bg.instance_id).await
    {
        tracing::warn!("failed to deregister instance: {}", e);
    }

    // Flush pending spans and log records before exit.
    if let Some(guards) = _otel_guards {
        if let Err(e) = guards.tracer_provider.shutdown() {
            eprintln!("OpenTelemetry tracer shutdown error: {e}");
        }
        if let Err(e) = guards.logger_provider.shutdown() {
            eprintln!("OpenTelemetry logger shutdown error: {e}");
        }
    }

    Ok(())
}

/// Monitor background workers and trigger shutdown if any exits unexpectedly.
async fn supervise_workers(
    handles: Vec<(&'static str, JoinHandle<()>)>,
    cancel: CancellationToken,
) {
    let futs: Vec<_> = handles
        .into_iter()
        .map(|(name, h)| Box::pin(async move { (name, h.await) }))
        .collect();
    let ((name, result), _, _) = futures::future::select_all(futs).await;
    match result {
        Ok(()) => {
            tracing::error!(worker = name, "background worker exited unexpectedly");
        }
        Err(ref e) => {
            tracing::error!(worker = name, error = %e, "background worker panicked");
        }
    }
    cancel.cancel();
}

#[allow(clippy::expect_used)]
async fn shutdown_signal(cancel: CancellationToken) {
    let ctrl_c = tokio::signal::ctrl_c();
    let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        .expect("failed to install SIGTERM handler");
    tokio::select! {
        _ = ctrl_c => {},
        _ = sigterm.recv() => {},
    }
    tracing::info!("shutdown signal received, stopping gracefully");
    cancel.cancel();
}
