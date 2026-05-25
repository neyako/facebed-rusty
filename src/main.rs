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

    let config = match args.config {
        Some(p) => Config::load(&p)?,
        None => {
            warn!("no config provided; using defaults");
            Config::default()
        }
    };

    let cookies = Arc::new(CookieJar::load(&args.cookies)?);
    let fetcher = Arc::new(Fetcher::new(cookies.clone())?);
    let notifier = Notifier::new(config.notifier_webhook.clone(), fetcher.client().clone());

    if !cookies.is_empty() {
        let expired = cookies.expired_labels();
        if !expired.is_empty() {
            notifier.warn(
                format!("@everyone cookies expired for: {}", expired.join(", ")),
                None,
            );
        }
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

    let ctx = Arc::new(ParserCtx {
        fetcher: fetcher.clone(),
        cookies: cookies.clone(),
        banned_users: config.banned_users.clone(),
    });

    let addr: SocketAddr = format!("{}:{}", config.host, config.port).parse()?;
    let state = AppState {
        config: Arc::new(config),
        ctx,
        notifier,
        fetcher,
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
