use arc_swap::ArcSwap;
use clap::Parser;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use tracing::{info, warn};
use tracing_subscriber::EnvFilter;

mod config;
mod cookies;
mod crawler;
mod embed;
mod embed_cache;
mod error;
mod fetch;
mod jq;
mod notifier;
mod parsers;
mod routes;
mod url_clean;

use crate::config::Config;
use crate::cookies::CookieJar;
use crate::fetch::Fetcher;
use crate::notifier::Notifier;
use crate::parsers::ParserCtx;
use crate::routes::{router, AppState};

/// Max Facebook post-renders in flight at once. Excess requests get a fast 503
/// instead of piling concurrent load onto the cookie pool. Tune for your host.
const MAX_INFLIGHT_FETCHES: usize = 16;

#[derive(Parser, Debug)]
#[command(name = "facebed", about = "Facebook embed proxy server")]
struct Args {
    /// Path to config YAML file (optional — defaults are used if omitted).
    #[arg(short, long)]
    config: Option<PathBuf>,

    /// Path to cookies.json (default: ./cookies.json).
    #[arg(long, default_value = "cookies.json")]
    cookies: PathBuf,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    tracing_subscriber::fmt().with_env_filter(filter).init();

    let args = Args::parse();

    let config = match &args.config {
        Some(p) => Config::load(&p)?,
        None => {
            warn!("no config provided; using defaults");
            Config::default()
        }
    };

    let cookies = Arc::new(ArcSwap::from_pointee(CookieJar::load(&args.cookies)?));
    let fetcher = Arc::new(Fetcher::new(cookies.clone())?);
    let notifier = Notifier::new(config.notifier_webhook.clone(), fetcher.client().clone());
    let addr: SocketAddr = format!("{}:{}", config.host, config.port).parse()?;
    let config = Arc::new(ArcSwap::from_pointee(config));

    if !cookies.load().is_empty() {
        let check_fetcher = fetcher.clone();
        let check_notifier = notifier.clone();
        tokio::spawn(async move {
            let checks = check_fetcher.check_cookie_accounts().await;
            let mut bad = Vec::new();
            for check in &checks {
                if check.ok {
                    info!(
                        account_index = check.index,
                        account = %check.label,
                        name = %check.account_name.as_deref().unwrap_or("?"),
                        status = ?check.status,
                        "cookie account alive"
                    );
                } else {
                    let reason = check.reason.as_deref().unwrap_or("unknown");
                    warn!(
                        account_index = check.index,
                        account = %check.label,
                        status = ?check.status,
                        reason = %reason,
                        "cookie account bad"
                    );
                    bad.push(format!("{} ({reason})", check.label));
                }
            }
            if !bad.is_empty() {
                check_notifier.warn(
                    format!("@everyone cookie account check failed: {}", bad.join(", ")),
                    None,
                );
            }
        });
    }

    #[cfg(unix)]
    {
        let jar = cookies.clone();
        let cookies_path = args.cookies.clone();
        let cfg_swap = config.clone();
        let cfg_path = args.config.clone();
        tokio::spawn(async move {
            use tokio::signal::unix::{signal, SignalKind};
            let mut hup = match signal(SignalKind::hangup()) {
                Ok(s) => s,
                Err(e) => {
                    warn!("cannot install SIGHUP handler: {e}");
                    return;
                }
            };
            while hup.recv().await.is_some() {
                match validate_cookie_json_files(&cookies_path)
                    .and_then(|_| CookieJar::load(&cookies_path))
                {
                    Ok(new_jar) => {
                        let n = new_jar.len();
                        jar.store(Arc::new(new_jar));
                        info!("reloaded {n} cookie account(s) on SIGHUP");
                    }
                    Err(e) => warn!("cookie reload failed: {e}"),
                }
                if let Some(p) = &cfg_path {
                    match Config::load(p) {
                        Ok(new_cfg) => {
                            cfg_swap.store(Arc::new(new_cfg));
                            info!(
                                "reloaded config on SIGHUP (timezone + banned_users live; host/port/webhook need restart)"
                            );
                        }
                        Err(e) => warn!("config reload failed: {e}"),
                    }
                }
            }
        });
    }

    let ctx = Arc::new(ParserCtx {
        fetcher: fetcher.clone(),
        cookies: cookies.clone(),
        config: config.clone(),
    });

    let state = AppState {
        config: config.clone(),
        ctx,
        notifier,
        fetcher,
        embed_cache: Arc::new(std::sync::Mutex::new(
            crate::embed_cache::EmbedCache::default(),
        )),
        fetch_limit: Arc::new(tokio::sync::Semaphore::new(MAX_INFLIGHT_FETCHES)),
        metrics: Arc::new(crate::routes::Metrics::default()),
        started_at: std::time::Instant::now(),
    };
    let app = router(state).layer(
        tower_http::trace::TraceLayer::new_for_http()
            .make_span_with(tower_http::trace::DefaultMakeSpan::new().level(tracing::Level::INFO))
            .on_response(tower_http::trace::DefaultOnResponse::new().level(tracing::Level::INFO)),
    );

    info!("listening on {}", addr);
    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, app).await?;
    Ok(())
}

#[cfg(unix)]
fn validate_cookie_json_files(path: &std::path::Path) -> anyhow::Result<()> {
    let mut files = Vec::new();
    if path.exists() {
        files.push(path.to_path_buf());
    }

    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .map(std::path::Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));
    for entry in std::fs::read_dir(&parent)
        .map_err(|e| anyhow::anyhow!("scan {}: {}", parent.display(), e))?
        .flatten()
    {
        let p = entry.path();
        if !p.is_file() {
            continue;
        }
        let Some(name) = p.file_name().and_then(|s| s.to_str()) else {
            continue;
        };
        if name.starts_with("cookies") && name.ends_with(".json") && name != "cookies.example.json"
        {
            files.push(p);
        }
    }
    files.sort();
    files.dedup();

    for p in files {
        let raw = std::fs::read_to_string(&p)
            .map_err(|e| anyhow::anyhow!("read {}: {}", p.display(), e))?;
        serde_json::from_str::<serde_json::Value>(&raw)
            .map_err(|e| anyhow::anyhow!("parse {}: {}", p.display(), e))?;
    }
    Ok(())
}
