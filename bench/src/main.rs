#![deny(clippy::unwrap_used)]
#![deny(clippy::expect_used)]

// SPDX-FileCopyrightText: 2026 Artifact Depot Contributors
//
// SPDX-License-Identifier: Apache-2.0

use clap::{Parser, Subcommand};

use depot_bench::{audit, client, demo, reorg, runner};

#[derive(Parser)]
#[command(
    name = "depot-bench",
    about = "Artifact Depot — demo seeding and benchmarking"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Seed realistic content into a depot instance
    Demo {
        /// Base URL of the depot instance
        #[arg(long, default_value = "http://localhost:8080")]
        url: String,

        /// Username for authentication
        #[arg(long, default_value = "admin")]
        username: String,

        /// Password for authentication
        #[arg(long, default_value = "admin")]
        password: String,

        /// Skip TLS certificate verification
        #[arg(long, default_value_t = false)]
        insecure: bool,

        /// Number of raw hosted repos to create
        #[arg(long, default_value_t = 3)]
        repos: usize,

        /// Number of Docker hosted repos to create
        #[arg(long, default_value_t = 2)]
        docker_repos: usize,

        /// Artifacts per raw repo
        #[arg(long, default_value_t = 50)]
        artifacts: usize,

        /// Images per Docker repo
        #[arg(long, default_value_t = 5)]
        images: usize,

        /// Tags per image
        #[arg(long, default_value_t = 3)]
        tags: usize,

        /// Delete existing repos first
        #[arg(long, default_value_t = false)]
        clean: bool,
    },

    /// Run slow background activity against a seeded depot instance
    Trickle {
        /// Base URL of the depot instance
        #[arg(long, default_value = "http://localhost:8080")]
        url: String,

        /// Username for authentication
        #[arg(long, default_value = "admin")]
        username: String,

        /// Password for authentication
        #[arg(long, default_value = "admin")]
        password: String,

        /// Skip TLS certificate verification
        #[arg(long, default_value_t = false)]
        insecure: bool,
    },

    /// Run benchmarks against a depot instance
    Bench {
        /// Base URL of the depot instance
        #[arg(long, default_value = "http://localhost:8080")]
        url: String,

        /// Username for authentication
        #[arg(long, default_value = "admin")]
        username: String,

        /// Password for authentication
        #[arg(long, default_value = "admin")]
        password: String,

        /// Skip TLS certificate verification
        #[arg(long, default_value_t = false)]
        insecure: bool,

        /// Scenario to run
        #[arg(long, default_value = "all")]
        scenario: String,

        /// Number of concurrent workers
        #[arg(long, default_value_t = 4)]
        concurrency: usize,

        /// Duration per scenario in seconds
        #[arg(long, default_value_t = 30)]
        duration: u64,

        /// Total operations (overrides --duration)
        #[arg(long)]
        count: Option<u64>,

        /// Raw artifact size in bytes
        #[arg(long, default_value_t = 1_048_576)]
        artifact_size: usize,

        /// Docker layer size in bytes
        #[arg(long, default_value_t = 4_194_304)]
        layer_size: usize,

        /// Warmup duration in seconds (excluded from stats)
        #[arg(long, default_value_t = 2)]
        warmup: u64,

        /// Output results as JSON
        #[arg(long, default_value_t = false)]
        json: bool,

        /// Blob store name to use for benchmark repos
        #[arg(long)]
        store: String,

        /// Use fully random data (expensive to generate). Default uses a
        /// cheap repeating pattern with a unique header per object.
        #[arg(long, default_value_t = false)]
        random_data: bool,

        /// Number of pipelined requests per worker (in-flight simultaneously
        /// on each connection). Default 1 = sequential.
        #[arg(long, default_value_t = 1)]
        pipeline: usize,
    },

    /// Re-file Docker images into the right repositories by tag, per a rules file
    Reorg {
        /// Base URL of the depot instance
        #[arg(long, default_value = "http://localhost:8080")]
        url: String,

        /// Username for authentication
        #[arg(long, default_value = "admin")]
        username: String,

        /// Password for authentication
        #[arg(long, default_value = "admin")]
        password: String,

        /// Skip TLS certificate verification
        #[arg(long, default_value_t = false)]
        insecure: bool,

        /// Path to the TOML rules file
        #[arg(long)]
        rules: String,

        /// Action groups to execute, comma-separated: any of `move`,
        /// `redundant`, `superseded`, `mismatch`, `retired`, `non-released`, or
        /// `all`. Omit for a dry run that prints the plan and each group's name.
        /// e.g. `--apply move,redundant` runs the moves and drops the redundant
        /// duplicates, leaving everything else untouched.
        #[arg(long)]
        apply: Option<String>,

        /// Use staging copy (leave the source intact) instead of move
        #[arg(long, default_value_t = false)]
        copy: bool,

        /// Print the full per-tag move list (default shows only the summary +
        /// removals; moves are summarized by destination)
        #[arg(long, default_value_t = false)]
        verbose: bool,
    },

    /// Audit a group repo for tags cloaked by an earlier member (shadowing)
    AuditShadow {
        /// Base URL of the depot instance
        #[arg(long, default_value = "http://localhost:8080")]
        url: String,

        /// Username for authentication
        #[arg(long, default_value = "admin")]
        username: String,

        /// Password for authentication
        #[arg(long, default_value = "admin")]
        password: String,

        /// Skip TLS certificate verification
        #[arg(long, default_value_t = false)]
        insecure: bool,

        /// Name of the group/proxy repo to audit
        #[arg(long)]
        group: String,
    },
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Command::Demo {
            url,
            username,
            password,
            insecure,
            repos,
            docker_repos,
            artifacts,
            images,
            tags,
            clean,
        } => {
            tracing_subscriber::fmt()
                .with_env_filter(
                    tracing_subscriber::EnvFilter::try_from_default_env()
                        .unwrap_or_else(|_| "depot=info".into()),
                )
                .init();

            let c = client::DepotClient::new(&url, &username, &password, insecure)?;
            demo::run(
                &c,
                demo::DemoConfig {
                    repos,
                    docker_repos,
                    artifacts,
                    images,
                    tags,
                    clean,
                    blob_root: "build/demo/blobs".to_string(),
                },
            )
            .await?;
        }

        Command::Trickle {
            url,
            username,
            password,
            insecure,
        } => {
            tracing_subscriber::fmt()
                .with_env_filter(
                    tracing_subscriber::EnvFilter::try_from_default_env()
                        .unwrap_or_else(|_| "depot=info".into()),
                )
                .init();

            let c = client::DepotClient::new(&url, &username, &password, insecure)?;
            demo::trickle::run_trickle(&c).await?;
        }

        Command::Bench {
            url,
            username,
            password,
            insecure,
            scenario,
            concurrency,
            duration,
            count,
            artifact_size,
            layer_size,
            warmup,
            json,
            store,
            random_data,
            pipeline,
        } => {
            tracing_subscriber::fmt()
                .with_env_filter(
                    tracing_subscriber::EnvFilter::try_from_default_env()
                        .unwrap_or_else(|_| "depot=info".into()),
                )
                .init();

            let c = client::DepotClient::new(&url, &username, &password, insecure)?;
            runner::run(
                &c,
                runner::BenchConfig {
                    scenario,
                    concurrency,
                    duration,
                    count,
                    artifact_size,
                    layer_size,
                    warmup,
                    json,
                    store,
                    random_data,
                    pipeline,
                },
            )
            .await?;
        }

        Command::Reorg {
            url,
            username,
            password,
            insecure,
            rules,
            apply,
            copy,
            verbose,
        } => {
            tracing_subscriber::fmt()
                .with_env_filter(
                    tracing_subscriber::EnvFilter::try_from_default_env()
                        .unwrap_or_else(|_| "depot=info".into()),
                )
                .init();

            let c = client::DepotClient::new(&url, &username, &password, insecure)?;
            // Parse the comma-separated group list; None => dry run.
            let apply = apply.map(|s| {
                s.split(',')
                    .map(|g| g.trim().to_string())
                    .filter(|g| !g.is_empty())
                    .collect::<Vec<_>>()
            });
            reorg::run(
                &c,
                reorg::ReorgConfig {
                    rules_path: rules,
                    apply,
                    copy,
                    insecure,
                    verbose,
                },
            )
            .await?;
        }

        Command::AuditShadow {
            url,
            username,
            password,
            insecure,
            group,
        } => {
            tracing_subscriber::fmt()
                .with_env_filter(
                    tracing_subscriber::EnvFilter::try_from_default_env()
                        .unwrap_or_else(|_| "depot=info".into()),
                )
                .init();

            let c = client::DepotClient::new(&url, &username, &password, insecure)?;
            audit::run(&c, audit::AuditConfig { group }).await?;
        }
    }

    Ok(())
}
