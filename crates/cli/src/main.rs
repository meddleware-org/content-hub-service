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
use content_hub_api::auth::oidc::{Oidc, OidcConfig};
use content_hub_api::auth::{self, AdminAuth, AuthConfig, Authenticator, JwksSource, RoleSource};
use content_hub_api::{admin, public};
use content_hub_store::repo::seed::{SeedReport, apply_definitions, apply_seed};
use content_hub_store::{Access, DatabaseSettings, migrate};
use tokio::net::TcpListener;

// `clap` derives the argument parser from this struct; the attribute below sets the name, version and help text.
#[derive(Parser)]
#[command(
    name = "content-hub",
    version,
    about = "The content hub: admin, public, worker, migrate, seed, export-snapshot."
)]
/// The command line: one binary, one process per subcommand.
struct Cli {
    /// What to run.
    #[command(subcommand)]
    command: Command,
}

/// The subcommands.
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

/// Why a command failed.
#[derive(Debug, thiserror::Error)]
enum CliError {
    /// The database failed.
    #[error(transparent)]
    Store(#[from] content_hub_store::StoreError),
    /// The database settings were refused.
    #[error(transparent)]
    Config(#[from] content_hub_store::ConfigError),
    /// A setting is missing or wrong; the message names the key and never repeats a credential.
    #[error("{0}")]
    Setting(String),
    /// A server or file operation failed.
    #[error("server: {0}")]
    Io(#[from] std::io::Error),
}

/// The value of an environment variable, if it is set and valid text.
fn env(k: &str) -> Option<String> {
    std::env::var(k).ok()
}

/// A required variable that is set and not blank.
fn required(k: &'static str) -> Result<String, CliError> {
    // A blank value counts as missing, so an empty Secret cannot pass as a setting.
    env(k).filter(|v| !v.trim().is_empty()).ok_or_else(|| CliError::Setting(format!("{k} is required")))
}

/// Check that a value is an https URL without whitespace.
fn https(k: &'static str, v: &str) -> Result<(), CliError> {
    // Only the scheme and the absence of whitespace are checked here; the address is not parsed or contacted.
    if v.starts_with("https://") && !v.contains(char::is_whitespace) {
        Ok(())
    } else {
        Err(CliError::Setting(format!("{k} must be an https URL")))
    }
}

/// A non-negative whole number from an environment variable (0 when unset).
fn micros(k: &'static str) -> Result<i64, CliError> {
    match env(k) {
        // An unset ceiling is zero, which the admin API treats as "no spend allowed".
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

/// Which sign-in paths the admin process accepts (`ADMIN_AUTH`: `access`, `both` or `oidc`; `access` when unset, which is what ran before the migration).
fn admin_auth() -> Result<AdminAuth, CliError> {
    match env("ADMIN_AUTH") {
        None => Ok(AdminAuth::Access),
        Some(v) => AdminAuth::parse(&v)
            .ok_or_else(|| CliError::Setting(format!("ADMIN_AUTH {v:?} must be access, both or oidc"))),
    }
}

/// The OpenID Connect sign-in's settings. The client secret is read from the file the Secret is mounted as, never from a variable.
fn oidc_config(admin_origin: &str, dev: bool) -> Result<OidcConfig, CliError> {
    let issuer = required("OIDC_ISSUER")?;
    // Development may use an http provider; production never does.
    if dev && issuer.starts_with("http://") && !issuer.contains(char::is_whitespace) {
        // accepted for local tests only
    } else {
        https("OIDC_ISSUER", &issuer)?;
    }
    let secret_path = required("OIDC_CLIENT_SECRET_FILE")?;
    let client_secret = std::fs::read_to_string(&secret_path)
        .map_err(|_| CliError::Setting(format!("cannot read OIDC_CLIENT_SECRET_FILE {secret_path}")))?
        .trim()
        .to_owned();
    if client_secret.len() < 20 {
        return Err(CliError::Setting("OIDC_CLIENT_SECRET_FILE holds no usable secret".into()));
    }
    let authz_key = match env("AUTHZ_KEY_FILE") {
        Some(path) => Some(
            std::fs::read_to_string(&path)
                .map_err(|_| CliError::Setting(format!("cannot read AUTHZ_KEY_FILE {path}")))?
                .trim()
                .to_owned(),
        ),
        None => None,
    };
    // The redirect URI is derived from the console's own origin, so it can never name another host.
    let redirect_uri = format!("{admin_origin}/auth/callback");
    let seconds = |k: &'static str, default: u64| -> Result<std::time::Duration, CliError> {
        match env(k) {
            None => Ok(std::time::Duration::from_secs(default)),
            Some(v) => v
                .parse::<u64>()
                .ok()
                .filter(|n| (60..=7 * 86_400).contains(n))
                .map(std::time::Duration::from_secs)
                .ok_or_else(|| CliError::Setting(format!("{k} must be 60 to 604800 seconds"))),
        }
    };
    Ok(OidcConfig {
        issuer: issuer.trim_end_matches('/').to_owned(),
        client_id: required("OIDC_CLIENT_ID")?,
        client_secret,
        redirect_uri,
        post_logout_uri: format!("{admin_origin}/"),
        internal_base: env("OIDC_INTERNAL_BASE").filter(|v| !v.trim().is_empty()),
        authz_base: required("AUTHZ_URL")?,
        authz_key,
        resource: env("OIDC_RESOURCE").unwrap_or_else(|| "content-hub".into()),
        cookie_name: "__Host-hub-admin".into(),
        binding_cookie_name: "__Host-hub-login".into(),
        idle: seconds("SESSION_IDLE_SECS", 3600)?,
        absolute: seconds("SESSION_ABSOLUTE_SECS", 12 * 3600)?,
        allow_insecure: dev,
    })
}

/// The admin process's access configuration (Cloudflare Access; not needed when only OpenID Connect is accepted).
fn auth_config(mode: AdminAuth) -> Result<AuthConfig, CliError> {
    // Development relaxes only where the keys and roles come from; it never skips verification.
    let dev = development()?;
    // Only OpenID Connect: no Access setting is read, so a deployment that has dropped them does not fail to start.
    if !mode.accepts_access() {
        let admin_origin = required("ADMIN_ORIGIN")?;
        https("ADMIN_ORIGIN", &admin_origin)?;
        return Ok(AuthConfig::without_access(admin_origin.trim_end_matches('/').to_owned()));
    }
    let team = required("ACCESS_TEAM_DOMAIN")?;
    https("ACCESS_TEAM_DOMAIN", &team)?;
    // The issuer is compared as written in the token, so a trailing slash is removed once, here.
    let team = team.trim_end_matches('/').to_owned();
    let audience = required("ACCESS_AUD")?;
    let admin_origin = required("ADMIN_ORIGIN")?;
    https("ADMIN_ORIGIN", &admin_origin)?;
    // The two development-only settings: a local key set instead of the remote one, and a fixed list of admins.
    let dev_jwks = env("ACCESS_JWKS_FILE");
    let dev_admins = env("CONTENT_HUB_DEV_ADMINS");
    // Production refuses them outright, so a leftover development setting cannot weaken a live deployment.
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
        // Without a local file the keys are fetched from the access provider's certificate path.
        None => JwksSource::Remote(format!("{team}/cdn-cgi/access/certs")),
    };
    let roles = match dev_admins {
        Some(list) => RoleSource::Static(
            list.split(',').map(|s| s.trim().to_owned()).filter(|s| !s.is_empty()).collect::<BTreeSet<_>>(),
        ),
        // Without a fixed list, roles come from Keto, and its read address is then required.
        None => RoleSource::Keto(required("KETO_READ_URL")?),
    };
    Ok(AuthConfig { issuer: team, audience, jwks, roles, admin_origin })
}

/// The public process's settings from the environment: the home page, the feeds' base URL, an optional title, the two machine files and the Walrus aggregator.
fn public_config() -> Result<public::PublicConfig, CliError> {
    // Reads the file a variable points at, if the variable is set; an unreadable file is a startup error.
    let read = |k: &'static str| -> Result<Option<String>, CliError> {
        env(k)
            .map(|p| std::fs::read_to_string(&p).map_err(|_| CliError::Setting(format!("cannot read {k} {p}"))))
            .transpose()
    };
    // Both public addresses are required and must be https.
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
        // The aggregator is optional: without it a snapshot has no blob link, but the route still works.
        aggregator_url: match env("WALRUS_AGGREGATOR_URL").filter(|v| !v.trim().is_empty()) {
            Some(u) => {
                https("WALRUS_AGGREGATOR_URL", &u)?;
                Some(u)
            }
            None => None,
        },
    })
}

/// The address to listen on (`LISTEN_ADDR`, default `0.0.0.0:8080`).
fn listen_addr() -> Result<SocketAddr, CliError> {
    // All interfaces by default, because the process runs in a container behind the cluster network.
    let raw = env("LISTEN_ADDR").unwrap_or_else(|| "0.0.0.0:8080".to_owned());
    raw.parse().map_err(|_| CliError::Setting(format!("LISTEN_ADDR {raw:?} is not an address:port")))
}

/// The database pool size (`DB_POOL_SIZE`, 1 to 64, default 8).
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

/// Resolve when the process is asked to stop (Ctrl-C or SIGTERM).
async fn shutdown_signal() {
    // Ctrl-C, for a developer running the binary in a terminal.
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    // SIGTERM is what the cluster sends when it stops a Pod.
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
    // Whichever arrives first ends the wait.
    tokio::select! { () = ctrl_c => {}, () = term => {} }
    tracing::info!("shutting down");
}

/// Serve the router until the process is asked to stop.
async fn serve(app: axum::Router, what: &'static str) -> Result<(), CliError> {
    // Bind first, so a port already in use fails before anything is logged as "listening".
    let addr = listen_addr()?;
    let listener = TcpListener::bind(addr).await?;
    tracing::info!(%addr, process = what, "listening");
    // In-flight requests finish before the process exits.
    axum::serve(listener, app).with_graceful_shutdown(shutdown_signal()).await?;
    Ok(())
}

/// Run the chosen subcommand.
async fn run(cli: Cli) -> Result<(), CliError> {
    match cli.command {
        // The owner role is the only one allowed to change the schema, which is why migrating is its own subcommand.
        Command::Migrate => {
            let owner = DatabaseSettings::from_env(env)?;
            let applied = migrate::run(&owner).await?;
            tracing::info!(?applied, schema_version = migrate::SCHEMA_VERSION, "migrations done");
            Ok(())
        }
        // The public process opens its pool read-only; a bug here cannot write to the database.
        Command::Public => {
            let config = Arc::new(public_config()?);
            let db = DatabaseSettings::from_env(env)?.lazy_pool(pool_size()?, Access::ReadOnly)?;
            serve(public::router(public::PublicState { db, config }), "public").await
        }
        // The admin process holds the writer role and the instance's spend ceilings.
        Command::Admin => {
            let mode = admin_auth()?;
            let cfg = auth_config(mode)?;
            let ceilings = admin::Ceilings {
                total_monthly_usd_micros: micros("SPEND_CEILING_TOTAL_USD_MICROS")?,
                per_platform_monthly_usd_micros: micros("SPEND_CEILING_PER_PLATFORM_USD_MICROS")?,
            };
            // The OpenID Connect sign-in exists only when a mode asks for it.
            let oidc = if mode.accepts_session() {
                let o = Oidc::new(oidc_config(&cfg.admin_origin, development()?)?)
                    .map_err(|_| CliError::Setting("cannot build the sign-in client".into()))?;
                Some(o)
            } else {
                None
            };
            let auth = Authenticator::with_oidc(cfg, mode, oidc.clone())
                .map_err(|_| CliError::Setting("cannot build the authenticator".into()))?;
            let db = DatabaseSettings::from_env(env)?.lazy_pool(pool_size()?, Access::ReadWrite)?;
            // Expired sign-ins and sessions are removed every ten minutes (they are never accepted after they expire; this only keeps the tables small).
            if let Some(o) = oidc {
                let pool = db.clone();
                tokio::spawn(async move {
                    loop {
                        tokio::time::sleep(std::time::Duration::from_secs(600)).await;
                        if let Err(e) = o.purge(&pool).await {
                            tracing::warn!(error = %e, "purging expired sessions failed");
                        }
                    }
                });
            }
            tracing::info!(?mode, "admin sign-in paths");
            serve(admin::router(admin::AdminState { db, auth, ceilings }), "admin").await
        }
        // The remaining subcommands have their own functions below.
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
    // Live sending is opt-in: unset or 0 is a dry run, and anything but 0 or 1 is refused.
    let live = match env("PUBLISHING_LIVE").as_deref() {
        None | Some("0") => false,
        Some("1") => true,
        Some(_) => return Err(CliError::Setting("PUBLISHING_LIVE must be 0 or 1".into())),
    };
    // The chat summary is optional; without the webhook there is no alerting from the worker itself.
    let alerts = match env("ALERT_DISCORD_WEBHOOK_URL").filter(|v| !v.trim().is_empty()) {
        None => None,
        Some(url) => {
            // The address is checked as a Discord webhook before it is accepted, and the error never repeats it.
            content_hub_core::platform::discord::webhook_url("https://discord.com", &url)
                .map_err(|_| CliError::Setting("ALERT_DISCORD_WEBHOOK_URL is not a Discord webhook URL".into()))?;
            Some(content_hub_worker::alerts::AlertConfig {
                webhook_url: url,
                base_url: "https://discord.com".into(),
                every: std::time::Duration::from_secs(bounded("ALERT_INTERVAL_SECONDS", 3600, 300, 86_400)?),
            })
        }
    };
    // Only the attempt limit is configurable; the back-off schedule is the default one.
    let retry = content_hub_core::delivery::RetryPolicy {
        max_attempts: u32::try_from(bounded("MAX_ATTEMPTS", 5, 1, 10)?).unwrap_or(5),
        ..content_hub_core::delivery::RetryPolicy::default()
    };
    // Production is the default, and the development flag turns on the sandbox-only rule.
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

/// The wallet watch (`WATCH_WALLETS=label=0xaddress,...`, `SUI_GRAPHQL_URL`, `WATCH_WAL_TYPE`, `WATCH_INTERVAL_SECONDS`): none unless
/// wallets are listed. Public addresses only.
fn watch_config() -> Result<Option<content_hub_worker::watch::WatchConfig>, CliError> {
    // No wallets listed means no watch, and none of its other settings are read.
    let Some(list) = env("WATCH_WALLETS").filter(|v| !v.trim().is_empty()) else { return Ok(None) };
    // The list holds labels and public addresses only.
    let wallets = content_hub_worker::watch::parse_wallets(&list)
        .map_err(|e| CliError::Setting(format!("WATCH_WALLETS: {e}")))?;
    if wallets.is_empty() {
        return Ok(None);
    }
    let graphql_url = required("SUI_GRAPHQL_URL")?;
    https("SUI_GRAPHQL_URL", &graphql_url)?;
    // WAL is identified by its full type (the package differs per network); a missing or malformed value refuses to start
    // rather than reading WAL as zero.
    let wal_type = content_hub_worker::watch::canonical_coin_type(&required("WATCH_WAL_TYPE")?)
        .ok_or_else(|| CliError::Setting("WATCH_WAL_TYPE: expected 0xPACKAGE::wal::WAL".into()))?;
    Ok(Some(content_hub_worker::watch::WatchConfig {
        graphql_url,
        wal_type,
        wallets,
        every: std::time::Duration::from_secs(bounded("WATCH_INTERVAL_SECONDS", 300, 60, 3600)?),
    }))
}

/// The publishing worker and its probe/metrics server; both stop on SIGTERM, after the send in progress.
async fn worker() -> Result<(), CliError> {
    // Settings are read first, so a bad one stops the process before it touches the database.
    let cfg = worker_config()?;
    tracing::info!(live = cfg.live, production = cfg.production, alerts = cfg.alerts.is_some(), "worker configuration");
    let db = DatabaseSettings::from_env(env)?.lazy_pool(pool_size()?, Access::ReadWrite)?;
    // One shared HTTP client for the senders.
    let http = content_hub_publish::Http::new().map_err(CliError::Setting)?;
    let watch_cfg = watch_config()?;
    let watch = Arc::new(content_hub_worker::watch::Watch::new(watch_cfg.as_ref().map_or(&[][..], |c| &c.wallets)));
    // The health and metrics server reads the database at scrape time and shares the watch's latest balances.
    let metrics = content_hub_worker::metrics::router(content_hub_worker::metrics::MetricsState {
        db: db.clone(),
        cfg: Arc::new(cfg.clone()),
        watch: watch.clone(),
    });
    let w = Arc::new(content_hub_worker::Worker {
        db,
        http,
        cfg,
        secrets: content_hub_worker::env_secrets(),
        base_override: None,
    });
    // One stop signal, shared by the send loop and the wallet watch.
    let (stop_tx, stop_rx) = tokio::sync::watch::channel(false);
    let watcher = match watch_cfg {
        Some(c) => {
            tracing::info!(wallets = c.wallets.len(), every_s = c.every.as_secs(), "watching wallet balances");
            let http = content_hub_publish::Http::new().map_err(CliError::Setting)?;
            Some(tokio::spawn(content_hub_worker::watch::run(http, c, watch, stop_rx.clone())))
        }
        None => None,
    };
    // The send loop runs as its own task, next to the HTTP server below.
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
    // After the server stops, wait for the send in progress and for the watch to finish.
    let _ = looped.await;
    if let Some(h) = watcher {
        let _ = h.await;
    }
    Ok(())
}

/// The snapshot signer from `SNAPSHOT_SIGNER_SEED`.
fn signer_from_env() -> Result<content_hub_core::snapshot::Signer, CliError> {
    let seed = required("SNAPSHOT_SIGNER_SEED")?;
    content_hub_core::snapshot::Signer::from_base64_seed(&seed)
        .map_err(|_| CliError::Setting("SNAPSHOT_SIGNER_SEED is not the base64 of a 32-byte seed".into()))
}

/// Print the signer's address (never the seed).
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
    // The seed comes from the operating system's random source and goes only into the new file.
    getrandom::fill(&mut seed).map_err(|_| CliError::Setting("the OS random source is unavailable".into()))?;
    let signer = content_hub_core::snapshot::Signer::from_seed(&seed)
        .map_err(|_| CliError::Setting("could not make a key from the random seed".into()))?;
    // `create_new` refuses an existing file, so a key is never overwritten; mode 0600 makes it readable by its owner only.
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(out)
        .map_err(|e| CliError::Setting(format!("cannot create {}: {e}", out.display())))?;
    writeln!(f, "{}", base64::engine::general_purpose::STANDARD.encode(seed)).map_err(CliError::Io)?;
    // Only the address and the file's path are printed.
    println!("{}", serde_json::json!({ "address": signer.address(), "seed_file": out.display().to_string() }));
    Ok(())
}

/// The export settings from the environment: the network (mainnet only when explicitly allowed), the epochs, the minimum interval and the renewal margin.
fn export_config() -> Result<content_hub_worker::snapshot::ExportConfig, CliError> {
    // Testnet unless told otherwise.
    let network = env("SNAPSHOT_NETWORK").unwrap_or_else(|| "testnet".into());
    let mut cfg = content_hub_worker::snapshot::ExportConfig::for_network(&network).map_err(CliError::Setting)?;
    if network == "mainnet" && env("SNAPSHOT_ALLOW_MAINNET").as_deref() != Some("1") {
        return Err(CliError::Setting(
            "mainnet snapshots are off until the revenue-readiness gate passes: set SNAPSHOT_ALLOW_MAINNET=1 to allow them".into(),
        ));
    }
    // The three tunables, each bounded, with the network's defaults where unset.
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
    // Storing the blob is optional: without a publisher the snapshot is still signed and recorded.
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
    // The export decides for itself whether anything needs doing (unchanged, too soon, already running).
    let outcome = content_hub_worker::snapshot::export(&db, &signer, store.as_ref(), &cfg, chrono::Utc::now())
        .await
        .map_err(|e| CliError::Setting(format!("export failed: {e}")))?;
    println!("{}", serde_json::to_string(&outcome).unwrap_or_default());
    Ok(())
}

/// Read a seed file as JSON or YAML (by its extension).
fn parse_seed(path: &std::path::Path) -> Result<content_hub_core::seed::SeedFile, CliError> {
    let text = std::fs::read_to_string(path)
        .map_err(|_| CliError::Setting(format!("cannot read the seed {}", path.display())))?;
    // The extension picks the parser: `.json` is JSON, anything else is read as YAML.
    let json = path.extension().is_some_and(|e| e == "json");
    let parsed = if json {
        serde_json::from_str(&text).map_err(|e| e.to_string())
    } else {
        serde_saphyr::from_str(&text).map_err(|e| e.to_string())
    };
    parsed.map_err(|e| CliError::Setting(format!("the seed is not valid: {e}")))
}

/// Upsert the generic definitions and, when a path is given, the instance seed; with `check` only validate.
async fn seed(path: Option<&std::path::Path>, check: bool) -> Result<(), CliError> {
    // The generic platform definitions are always applied; the instance seed is optional.
    let defs = content_hub_core::platforms::generic().map_err(|e| CliError::Setting(e.to_string()))?;
    let seed = path.map(parse_seed).transpose()?;
    if let Some(s) = &seed {
        // The seed may refer only to platforms that exist in the definitions.
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
    // `--check` stops after validation, before any database settings are read.
    if check {
        tracing::info!(phases = seed.as_ref().map_or(0, |s| s.phases.len()), "seed is valid (nothing written)");
        return Ok(());
    }
    let settings = DatabaseSettings::from_env(env)?;
    let pool = settings.lazy_pool(1, Access::ReadWrite)?;
    // One transaction, so a failed seed leaves the database as it was.
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

/// The entry point: parse the arguments, set up logging and the TLS provider, run the chosen subcommand to completion
/// on a multi-threaded runtime, and turn the result into the process's exit code.
fn main() -> ExitCode {
    // Arguments are parsed first, so `--help` and a bad flag need no environment.
    let cli = Cli::parse();
    tracing_subscriber::fmt()
        // One JSON object per log line, on stdout (the header lists what the logs never contain).
        .json()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .with_current_span(false)
        .init();
    // The TLS provider is installed once, before any HTTPS client or server is built.
    auth::install_crypto_provider();
    // The runtime is built by hand so that a failure to start it is logged as JSON and returns a failure code.
    let rt = match tokio::runtime::Builder::new_multi_thread().enable_all().build() {
        Ok(rt) => rt,
        Err(e) => {
            tracing::error!(error = %e, "cannot start the runtime");
            return ExitCode::FAILURE;
        }
    };
    // A failed command is logged once, and the exit code tells the Job or Pod that it failed.
    match rt.block_on(run(cli)) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            tracing::error!(error = %e, "exiting");
            ExitCode::FAILURE
        }
    }
}

/// Unit tests for this module.
#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    /// A fresh, unused path in the system temp directory, so tests never share or collide on a file.
    fn tmp(name: &str) -> std::path::PathBuf {
        let mut b = [0u8; 6];
        getrandom::fill(&mut b).expect("random");
        std::env::temp_dir().join(format!("ch-{name}-{}", b.iter().map(|x| format!("{x:02x}")).collect::<String>()))
    }

    /// The signer key is written private once and matches its address.
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

    /// A rejected URL setting names its key and never repeats the value, which may carry a credential.
    #[test]
    fn a_rejected_url_is_never_repeated_in_the_error() {
        // A stand-in secret in the user-info and the query string of an address that is not https.
        let value = "http://user:stand-in-secret@host.example/hook?token=stand-in-secret";
        let message = https("ALERT_DISCORD_WEBHOOK_URL", value).expect_err("not https").to_string();
        assert!(message.contains("ALERT_DISCORD_WEBHOOK_URL"), "{message}");
        assert!(!message.contains("stand-in-secret") && !message.contains("host.example"), "{message}");
        // The same for an https address that has whitespace in it.
        let spaced = "https://host.example/hook?token=stand-in-secret and more";
        let message = https("SUI_GRAPHQL_URL", spaced).expect_err("whitespace").to_string();
        assert!(message.contains("SUI_GRAPHQL_URL") && !message.contains("stand-in-secret"), "{message}");
    }

    /// Mainnet snapshots are refused unless allowed.
    #[test]
    fn mainnet_snapshots_are_refused_unless_allowed() {
        // Only the pure rule is tested (the environment is process-wide): the defaults are testnet's.
        let cfg = content_hub_worker::snapshot::ExportConfig::for_network("mainnet").expect("cfg");
        assert_eq!(cfg.epochs, 2);
        assert!(content_hub_worker::snapshot::ExportConfig::for_network("devnet").is_err());
    }
}
