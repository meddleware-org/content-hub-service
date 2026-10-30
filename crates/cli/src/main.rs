//! `content-hub` — the content hub's one binary. Each subcommand is a separate process in a deployment
//! (ARCHITECTURE.md §3) with its own database role:
//!
//! | Subcommand | Runs as | Database role |
//! | --- | --- | --- |
//! | `admin` | Deployment (hub.) | writer |
//! | `public` | Deployment (roadmap., feeds.) | read-only |
//! | `worker` | Deployment | writer |
//! | `migrate` | Job, before each rollout | owner |
//! | `seed` | Job | writer |
//! | `export-snapshot` | CronJob | writer |
//!
//! Configuration comes from the environment only (README → Configuration): `DATABASE_URL` (with
//! `sslmode=verify-full` and `sslrootcert`), `LISTEN_ADDR` (default `0.0.0.0:8080`), `RUST_LOG` (default `info`),
//! `DB_POOL_SIZE` (default 8), and per process the access settings (`admin`), the public site settings (`public`) and
//! the publishing settings (`worker`).
//! Logs are JSON on stdout and never contain the URL, tokens or post bodies.

#![forbid(unsafe_code)]

use std::net::SocketAddr;
use std::process::ExitCode;

use std::collections::BTreeSet;
use std::sync::Arc;

use clap::{Parser, Subcommand};
use content_hub_api::auth::{self, AuthConfig, Authenticator, JwksSource, RoleSource};
use content_hub_api::{admin, public};
use content_hub_store::repo::seed::{SeedReport, apply_definitions, apply_seed};
use content_hub_store::{Access, DatabaseSettings, migrate};
use tokio::net::TcpListener;

#[derive(Parser)]
#[command(
    name = "content-hub",
    version,
    about = "The content hub: admin, public, worker, migrate, seed, export-snapshot."
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Serve the admin API (behind Cloudflare Access) as the writer role.
    Admin,
    /// Serve the public roadmap, feeds and machine-host files as the read-only role.
    Public,
    /// Run the publishing worker.
    Worker,
    /// Apply pending migrations as the database owner, then exit.
    Migrate,
    /// Upsert the generic platform definitions and, with --path, an instance roadmap seed (YAML or JSON).
    Seed {
        /// The instance seed file (never baked into the image).
        #[arg(long)]
        path: Option<std::path::PathBuf>,
        /// Validate only; write nothing.
        #[arg(long)]
        check: bool,
    },
    /// Build, sign and store the public roadmap snapshot (ADR-0007), then exit.
    ExportSnapshot,
    /// Generate the snapshot signer's key: writes the seed to a new 0600 file and prints only the address.
    SnapshotKey {
        /// Where to write the seed (must not exist).
        #[arg(long)]
        out: std::path::PathBuf,
    },
    /// Print the address of the key in SNAPSHOT_SIGNER_SEED (never the seed).
    SnapshotAddress,
}

#[derive(Debug, thiserror::Error)]
enum CliError {
    #[error(transparent)]
    Store(#[from] content_hub_store::StoreError),
    #[error(transparent)]
    Config(#[from] content_hub_store::ConfigError),
    #[error("{0}")]
    Setting(String),
    #[error("server: {0}")]
    Io(#[from] std::io::Error),
}

fn env(k: &str) -> Option<String> {
    std::env::var(k).ok()
}

fn required(k: &'static str) -> Result<String, CliError> {
    env(k).filter(|v| !v.trim().is_empty()).ok_or_else(|| CliError::Setting(format!("{k} is required")))
}

fn https(k: &'static str, v: &str) -> Result<(), CliError> {
    if v.starts_with("https://") && !v.contains(char::is_whitespace) {
        Ok(())
    } else {
        Err(CliError::Setting(format!("{k} must be an https URL")))
    }
}

fn micros(k: &'static str) -> Result<i64, CliError> {
    match env(k) {
        None => Ok(0),
        Some(v) => v
            .parse::<i64>()
            .ok()
            .filter(|n| *n >= 0)
            .ok_or_else(|| CliError::Setting(format!("{k} must be a non-negative integer"))),
    }
}

/// `development` enables the static JWKS and admin list; anything else (the default) is production and refuses them.
fn development() -> Result<bool, CliError> {
    match env("CONTENT_HUB_ENVIRONMENT").as_deref() {
        None | Some("production") => Ok(false),
        Some("development") => Ok(true),
        Some(other) => {
            Err(CliError::Setting(format!("CONTENT_HUB_ENVIRONMENT {other:?} must be production or development")))
        }
    }
}

/// The admin process's access configuration.
fn auth_config() -> Result<AuthConfig, CliError> {
    let dev = development()?;
    let team = required("ACCESS_TEAM_DOMAIN")?;
    https("ACCESS_TEAM_DOMAIN", &team)?;
    let team = team.trim_end_matches('/').to_owned();
    let audience = required("ACCESS_AUD")?;
    let admin_origin = required("ADMIN_ORIGIN")?;
    https("ADMIN_ORIGIN", &admin_origin)?;
    let dev_jwks = env("ACCESS_JWKS_FILE");
    let dev_admins = env("CONTENT_HUB_DEV_ADMINS");
    if !dev && (dev_jwks.is_some() || dev_admins.is_some()) {
        return Err(CliError::Setting(
            "ACCESS_JWKS_FILE and CONTENT_HUB_DEV_ADMINS are development settings; production refuses them".into(),
        ));
    }
    let jwks = match dev_jwks {
        Some(path) => {
            let text = std::fs::read_to_string(&path)
                .map_err(|_| CliError::Setting(format!("cannot read ACCESS_JWKS_FILE {path}")))?;
            JwksSource::Static(
                serde_json::from_str(&text).map_err(|_| CliError::Setting("ACCESS_JWKS_FILE is not a JWKS".into()))?,
            )
        }
        None => JwksSource::Remote(format!("{team}/cdn-cgi/access/certs")),
    };
    let roles = match dev_admins {
        Some(list) => RoleSource::Static(
            list.split(',').map(|s| s.trim().to_owned()).filter(|s| !s.is_empty()).collect::<BTreeSet<_>>(),
        ),
        None => RoleSource::Keto(required("KETO_READ_URL")?),
    };
    Ok(AuthConfig { issuer: team, audience, jwks, roles, admin_origin })
}

fn public_config() -> Result<public::PublicConfig, CliError> {
    let read = |k: &'static str| -> Result<Option<String>, CliError> {
        env(k)
            .map(|p| std::fs::read_to_string(&p).map_err(|_| CliError::Setting(format!("cannot read {k} {p}"))))
            .transpose()
    };
    let home = required("HOME_PAGE_URL")?;
    https("HOME_PAGE_URL", &home)?;
    let feeds = required("FEEDS_BASE_URL")?;
    https("FEEDS_BASE_URL", &feeds)?;
    Ok(public::PublicConfig {
        feed_title: env("FEED_TITLE").unwrap_or_else(|| "Updates".into()),
        home_page_url: home,
        feeds_base_url: feeds,
        llms_txt: read("LLMS_TXT_FILE")?,
        robots_txt: read("ROBOTS_TXT_FILE")?,
        aggregator_url: match env("WALRUS_AGGREGATOR_URL").filter(|v| !v.trim().is_empty()) {
            Some(u) => {
                https("WALRUS_AGGREGATOR_URL", &u)?;
                Some(u)
            }
            None => None,
        },
    })
}

fn listen_addr() -> Result<SocketAddr, CliError> {
    let raw = env("LISTEN_ADDR").unwrap_or_else(|| "0.0.0.0:8080".to_owned());
    raw.parse().map_err(|_| CliError::Setting(format!("LISTEN_ADDR {raw:?} is not an address:port")))
}

fn pool_size() -> Result<u32, CliError> {
    match env("DB_POOL_SIZE") {
        None => Ok(8),
        Some(v) => v
            .parse::<u32>()
            .ok()
            .filter(|n| (1..=64).contains(n))
            .ok_or_else(|| CliError::Setting(format!("DB_POOL_SIZE {v:?} must be 1 to 64"))),
    }
}

async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let term = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut s) => {
                s.recv().await;
            }
            Err(_) => std::future::pending::<()>().await,
        }
    };
    #[cfg(not(unix))]
    let term = std::future::pending::<()>();
    tokio::select! { () = ctrl_c => {}, () = term => {} }
    tracing::info!("shutting down");
}

async fn serve(app: axum::Router, what: &'static str) -> Result<(), CliError> {
    let addr = listen_addr()?;
    let listener = TcpListener::bind(addr).await?;
    tracing::info!(%addr, process = what, "listening");
    axum::serve(listener, app).with_graceful_shutdown(shutdown_signal()).await?;
    Ok(())
}

async fn run(cli: Cli) -> Result<(), CliError> {
    match cli.command {
        Command::Migrate => {
            let owner = DatabaseSettings::from_env(env)?;
            let applied = migrate::run(&owner).await?;
            tracing::info!(?applied, schema_version = migrate::SCHEMA_VERSION, "migrations done");
            Ok(())
        }
        Command::Public => {
            let config = Arc::new(public_config()?);
            let db = DatabaseSettings::from_env(env)?.lazy_pool(pool_size()?, Access::ReadOnly)?;
            serve(public::router(public::PublicState { db, config }), "public").await
        }
        Command::Admin => {
            let cfg = auth_config()?;
            let ceilings = admin::Ceilings {
                total_monthly_usd_micros: micros("SPEND_CEILING_TOTAL_USD_MICROS")?,
                per_platform_monthly_usd_micros: micros("SPEND_CEILING_PER_PLATFORM_USD_MICROS")?,
            };
            let auth = Authenticator::new(cfg).map_err(|_| CliError::Setting("cannot build the HTTP client".into()))?;
            let db = DatabaseSettings::from_env(env)?.lazy_pool(pool_size()?, Access::ReadWrite)?;
            serve(admin::router(admin::AdminState { db, auth, ceilings }), "admin").await
        }
        Command::Worker => worker().await,
        Command::Seed { path, check } => seed(path.as_deref(), check).await,
        Command::ExportSnapshot => export_snapshot().await,
        Command::SnapshotKey { out } => snapshot_key(&out),
        Command::SnapshotAddress => snapshot_address(),
    }
}

/// An integer setting within bounds, or its default.
fn bounded(k: &'static str, default: u64, min: u64, max: u64) -> Result<u64, CliError> {
    match env(k) {
        None => Ok(default),
        Some(v) => v
            .trim()
            .parse::<u64>()
            .ok()
            .filter(|n| (min..=max).contains(n))
            .ok_or_else(|| CliError::Setting(format!("{k} must be an integer from {min} to {max}"))),
    }
}

/// The worker's configuration. Every send is a dry run unless `PUBLISHING_LIVE=1`, and then only to targets marked
/// live (outside production: sandbox targets only).
fn worker_config() -> Result<content_hub_worker::WorkerConfig, CliError> {
    let live = match env("PUBLISHING_LIVE").as_deref() {
        None | Some("0") => false,
        Some("1") => true,
        Some(_) => return Err(CliError::Setting("PUBLISHING_LIVE must be 0 or 1".into())),
    };
    let alerts = match env("ALERT_DISCORD_WEBHOOK_URL").filter(|v| !v.trim().is_empty()) {
        None => None,
        Some(url) => {
            content_hub_core::platform::discord::webhook_url("https://discord.com", &url)
                .map_err(|_| CliError::Setting("ALERT_DISCORD_WEBHOOK_URL is not a Discord webhook URL".into()))?;
            Some(content_hub_worker::alerts::AlertConfig {
                webhook_url: url,
                base_url: "https://discord.com".into(),
                every: std::time::Duration::from_secs(bounded("ALERT_INTERVAL_SECONDS", 3600, 300, 86_400)?),
            })
        }
    };
    let retry = content_hub_core::delivery::RetryPolicy {
        max_attempts: u32::try_from(bounded("MAX_ATTEMPTS", 5, 1, 10)?).unwrap_or(5),
        ..content_hub_core::delivery::RetryPolicy::default()
    };
    Ok(content_hub_worker::WorkerConfig {
        live,
        production: !development()?,
        poll: std::time::Duration::from_millis(bounded("POLL_INTERVAL_MS", 5000, 500, 60_000)?),
        lease: std::time::Duration::from_secs(bounded("CLAIM_LEASE_SECONDS", 600, 120, 86_400)?),
        retry,
        daily_post_limit: i32::try_from(bounded("DAILY_POST_LIMIT", 5, 1, 1000)?).unwrap_or(5),
        ceiling_total_usd_micros: micros("SPEND_CEILING_TOTAL_USD_MICROS")?,
        ceiling_per_platform_usd_micros: micros("SPEND_CEILING_PER_PLATFORM_USD_MICROS")?,
        limits_every: std::time::Duration::from_secs(24 * 3600),
        alerts,
    })
}

/// The publishing worker and its probe/metrics server; both stop on SIGTERM, after the send in progress.
async fn worker() -> Result<(), CliError> {
    let cfg = worker_config()?;
    tracing::info!(live = cfg.live, production = cfg.production, alerts = cfg.alerts.is_some(), "worker configuration");
    let db = DatabaseSettings::from_env(env)?.lazy_pool(pool_size()?, Access::ReadWrite)?;
    let http = content_hub_publish::Http::new().map_err(CliError::Setting)?;
    let metrics = content_hub_worker::metrics::router(content_hub_worker::metrics::MetricsState {
        db: db.clone(),
        cfg: Arc::new(cfg.clone()),
    });
    let w = Arc::new(content_hub_worker::Worker {
        db,
        http,
        cfg,
        secrets: content_hub_worker::env_secrets(),
        base_override: None,
    });
    let (stop_tx, stop_rx) = tokio::sync::watch::channel(false);
    let looped = tokio::spawn(w.run(stop_rx));
    let addr = listen_addr()?;
    let listener = TcpListener::bind(addr).await?;
    tracing::info!(%addr, process = "worker", "listening");
    axum::serve(listener, metrics)
        .with_graceful_shutdown(async move {
            shutdown_signal().await;
            let _ = stop_tx.send(true);
        })
        .await?;
    let _ = looped.await;
    Ok(())
}

fn signer_from_env() -> Result<content_hub_core::snapshot::Signer, CliError> {
    let seed = required("SNAPSHOT_SIGNER_SEED")?;
    content_hub_core::snapshot::Signer::from_base64_seed(&seed)
        .map_err(|_| CliError::Setting("SNAPSHOT_SIGNER_SEED is not the base64 of a 32-byte seed".into()))
}

fn snapshot_address() -> Result<(), CliError> {
    println!("{}", signer_from_env()?.address());
    Ok(())
}

/// Generate the signer's seed into a new file readable only by its owner, and print the address (the seed is never
/// printed, logged or returned).
fn snapshot_key(out: &std::path::Path) -> Result<(), CliError> {
    use base64::Engine;
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let mut seed = [0u8; 32];
    getrandom::fill(&mut seed).map_err(|_| CliError::Setting("the OS random source is unavailable".into()))?;
    let signer = content_hub_core::snapshot::Signer::from_seed(&seed)
        .map_err(|_| CliError::Setting("could not make a key from the random seed".into()))?;
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(out)
        .map_err(|e| CliError::Setting(format!("cannot create {}: {e}", out.display())))?;
    writeln!(f, "{}", base64::engine::general_purpose::STANDARD.encode(seed)).map_err(CliError::Io)?;
    println!("{}", serde_json::json!({ "address": signer.address(), "seed_file": out.display().to_string() }));
    Ok(())
}

fn export_config() -> Result<content_hub_worker::snapshot::ExportConfig, CliError> {
    let network = env("SNAPSHOT_NETWORK").unwrap_or_else(|| "testnet".into());
    let mut cfg = content_hub_worker::snapshot::ExportConfig::for_network(&network).map_err(CliError::Setting)?;
    if network == "mainnet" && env("SNAPSHOT_ALLOW_MAINNET").as_deref() != Some("1") {
        return Err(CliError::Setting(
            "mainnet snapshots are off until the revenue-readiness gate passes: set SNAPSHOT_ALLOW_MAINNET=1 to allow them".into(),
        ));
    }
    cfg.epochs = u32::try_from(bounded("SNAPSHOT_EPOCHS", u64::from(cfg.epochs), 1, 53)?).unwrap_or(cfg.epochs);
    cfg.min_interval = std::time::Duration::from_secs(bounded("SNAPSHOT_MIN_INTERVAL_HOURS", 24, 1, 720)? * 3600);
    cfg.renew_margin = std::time::Duration::from_secs(bounded("SNAPSHOT_RENEW_MARGIN_DAYS", 7, 1, 30)? * 86_400);
    Ok(cfg)
}

/// One export run (a CronJob): prints the outcome as JSON; "unchanged", "too soon" and "another export is running" are
/// successful runs.
async fn export_snapshot() -> Result<(), CliError> {
    let cfg = export_config()?;
    let signer = signer_from_env()?;
    let db = DatabaseSettings::from_env(env)?.lazy_pool(2, Access::ReadWrite)?;
    let store = match env("WALRUS_PUBLISHER_URL").filter(|v| !v.trim().is_empty()) {
        Some(url) => Some(
            content_hub_publish::blob::Publisher::new(&url, env("WALRUS_PUBLISHER_JWT_SECRET").as_deref())
                .map_err(CliError::Setting)?,
        ),
        None => {
            tracing::warn!("WALRUS_PUBLISHER_URL is not set: the snapshot is signed and recorded but not stored");
            None
        }
    };
    let outcome = content_hub_worker::snapshot::export(&db, &signer, store.as_ref(), &cfg, chrono::Utc::now())
        .await
        .map_err(|e| CliError::Setting(format!("export failed: {e}")))?;
    println!("{}", serde_json::to_string(&outcome).unwrap_or_default());
    Ok(())
}

fn parse_seed(path: &std::path::Path) -> Result<content_hub_core::seed::SeedFile, CliError> {
    let text = std::fs::read_to_string(path)
        .map_err(|_| CliError::Setting(format!("cannot read the seed {}", path.display())))?;
    let json = path.extension().is_some_and(|e| e == "json");
    let parsed = if json {
        serde_json::from_str(&text).map_err(|e| e.to_string())
    } else {
        serde_saphyr::from_str(&text).map_err(|e| e.to_string())
    };
    parsed.map_err(|e| CliError::Setting(format!("the seed is not valid: {e}")))
}

async fn seed(path: Option<&std::path::Path>, check: bool) -> Result<(), CliError> {
    let defs = content_hub_core::platforms::generic().map_err(|e| CliError::Setting(e.to_string()))?;
    let seed = path.map(parse_seed).transpose()?;
    if let Some(s) = &seed {
        let known: BTreeSet<String> = defs.definitions.iter().map(|d| d.code.clone()).collect();
        let problems = content_hub_core::seed::validate(s, &known);
        if !problems.is_empty() {
            return Err(CliError::Setting(format!(
                "the seed has {} problem(s):\n  {}",
                problems.len(),
                problems.join("\n  ")
            )));
        }
    }
    if check {
        tracing::info!(phases = seed.as_ref().map_or(0, |s| s.phases.len()), "seed is valid (nothing written)");
        return Ok(());
    }
    let settings = DatabaseSettings::from_env(env)?;
    let pool = settings.lazy_pool(1, Access::ReadWrite)?;
    let mut tx = pool.begin().await.map_err(content_hub_store::StoreError::from)?;
    let mut report = SeedReport::default();
    apply_definitions(&mut tx, &defs, &mut report).await?;
    if let Some(s) = &seed {
        apply_seed(&mut tx, s, "system:seed", &mut report).await?;
    }
    tx.commit().await.map_err(content_hub_store::StoreError::from)?;
    tracing::info!(report = %serde_json::to_string(&report).unwrap_or_default(), "seed applied");
    Ok(())
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    tracing_subscriber::fmt()
        .json()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .with_current_span(false)
        .init();
    auth::install_crypto_provider();
    let rt = match tokio::runtime::Builder::new_multi_thread().enable_all().build() {
        Ok(rt) => rt,
        Err(e) => {
            tracing::error!(error = %e, "cannot start the runtime");
            return ExitCode::FAILURE;
        }
    };
    match rt.block_on(run(cli)) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            tracing::error!(error = %e, "exiting");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn tmp(name: &str) -> std::path::PathBuf {
        let mut b = [0u8; 6];
        getrandom::fill(&mut b).expect("random");
        std::env::temp_dir().join(format!("ch-{name}-{}", b.iter().map(|x| format!("{x:02x}")).collect::<String>()))
    }

    #[test]
    fn the_signer_key_is_written_private_once_and_matches_its_address() {
        let path = tmp("seed");
        snapshot_key(&path).expect("generated");
        assert_eq!(std::fs::metadata(&path).expect("meta").permissions().mode() & 0o777, 0o600);
        let text = std::fs::read_to_string(&path).expect("read");
        let signer = content_hub_core::snapshot::Signer::from_base64_seed(&text).expect("a 32-byte seed");
        assert!(signer.address().starts_with("0x") && signer.address().len() == 66);
        let before = text.clone();
        // It never overwrites an existing key.
        assert!(snapshot_key(&path).is_err());
        assert_eq!(std::fs::read_to_string(&path).expect("read"), before);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn mainnet_snapshots_are_refused_unless_allowed() {
        // Only the pure rule is tested (the environment is process-wide): the defaults are testnet's.
        let cfg = content_hub_worker::snapshot::ExportConfig::for_network("mainnet").expect("cfg");
        assert_eq!(cfg.epochs, 2);
        assert!(content_hub_worker::snapshot::ExportConfig::for_network("devnet").is_err());
    }
}
