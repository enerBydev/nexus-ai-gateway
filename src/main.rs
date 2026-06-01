mod circuit_breaker;
mod cli;
mod config;
mod config_cmd;
mod error;
mod models;
mod prompt_cache;
mod proxy;
mod scan;
mod setup;
mod str_utils;
mod telemetry;
mod tokenizer;
mod transform;
mod watcher;
mod web_fetch;

use axum::http::HeaderValue;
use axum::response::Response;
use axum::{routing::post, Extension, Router};
use clap::Parser;
use cli::{Cli, Command};
use config::{Config, SharedConfig};
use daemonize::Daemonize;
use metrics_exporter_prometheus::{PrometheusBuilder, PrometheusHandle};
use reqwest::Client;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tokio::signal::unix::{signal, SignalKind};
use tower_http::{
    cors::{Any, CorsLayer},
    trace::TraceLayer,
};
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

use crate::proxy::retry::chunk_timeout_secs;

/// Global flag — true when server is draining for shutdown.
/// /health endpoint returns 503 when this is true.
/// Retry logic (S8) and config reload (S11) also check this flag.
pub static IS_DRAINING: AtomicBool = AtomicBool::new(false);

use tokio_util::sync::CancellationToken;

/// Global cancellation token — cancelled on shutdown signal.
/// All SSE streams monitor this token via tokio::select! to terminate gracefully.
pub static SHUTDOWN_TOKEN: std::sync::LazyLock<CancellationToken> =
    std::sync::LazyLock::new(CancellationToken::new);

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();

    if let Some(command) = cli.command {
        match command {
            Command::Stop { pid_file } => {
                stop_daemon(&pid_file)?;
                return Ok(());
            }
            Command::Status { pid_file } => {
                check_status(&pid_file)?;
                return Ok(());
            }
            Command::Scan { env, launcher, check } => {
                handle_scan(env, launcher, check)?;
                return Ok(());
            }
            Command::Setup { quick } => {
                setup::run_setup(quick, cli.config)?;
                return Ok(());
            }
            Command::Config { action } => {
                config_cmd::handle_config(action, cli.config)?;
                return Ok(());
            }
        }
    }

    if cli.daemon {
        use std::fs::OpenOptions;

        let stdout =
            OpenOptions::new().create(true).append(true).open("/tmp/nexus-ai-gateway.log")?;

        let stderr =
            OpenOptions::new().create(true).append(true).open("/tmp/nexus-ai-gateway.log")?;

        let daemonize = Daemonize::new()
            .pid_file(&cli.pid_file)
            .working_directory(std::env::current_dir()?)
            .stdout(stdout)
            .stderr(stderr)
            .umask(0o027);

        match daemonize.start() {
            Ok(_) => {}
            Err(e) => {
                eprintln!("✗ Failed to daemonize: {}", e);
                std::process::exit(1);
            }
        }
    } else {
        eprintln!("✓ Starting proxy in foreground mode");
    }

    let runtime = tokio::runtime::Runtime::new()?;
    runtime.block_on(async_main(cli))
}

async fn async_main(cli: Cli) -> anyhow::Result<()> {
    let mut config = Config::from_env_with_path(cli.config)?;

    if cli.debug {
        config.debug = true;
    }
    if cli.verbose {
        config.verbose = true;
    }
    if let Some(port) = cli.port {
        config.port = port;
    }

    let log_level = if config.verbose {
        tracing::Level::TRACE
    } else if config.debug {
        tracing::Level::DEBUG
    } else {
        tracing::Level::INFO
    };

    tracing_subscriber::registry()
        .with(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| format!("nexus_ai_gateway={}", log_level).into()),
        )
        .with(tracing_subscriber::fmt::layer())
        .init();

    // v0.18.0: Install Prometheus metrics recorder
    let prometheus_handle =
        PrometheusBuilder::new().install_recorder().expect("Failed to install Prometheus recorder");

    // v0.18.0: Initialize telemetry (privacy-first analytics)
    // CR fix: Pass beacon_url + explicit failure logging
    let telemetry_ctx = crate::telemetry::TelemetryContext::init(
        config.telemetry_enabled,
        &config.telemetry_db_path,
        &config.telemetry_secret_path,
        config.telemetry_retention_days,
        config.telemetry_beacon_url.clone(),
    );
    if config.telemetry_enabled && telemetry_ctx.is_none() {
        tracing::warn!(
            "⚠️ Telemetry was enabled but initialization failed — running without telemetry"
        );
    }

    tracing::info!("Starting NEXUS-AI-Gateway v{}", env!("CARGO_PKG_VERSION"));
    tracing::info!("Port: {}", config.port);
    tracing::info!("Upstream URL: {}", config.base_url);

    // Phase 3.4: Startup scan of CC binary
    match scan::scan_cc_binary() {
        Ok(result) => {
            tracing::info!(
                "🔍 CC scan: {} models, {} tools, {} capabilities",
                result.models.len(),
                result.tools.len(),
                result.capabilities.len()
            );
        }
        Err(e) => {
            tracing::warn!("⚠️ CC binary scan skipped: {}", e);
        }
    }
    if let Some(ref model) = config.reasoning_model {
        tracing::info!("Reasoning Model Override: {}", model);
    }
    if let Some(ref model) = config.completion_model {
        tracing::info!("Completion Model Override: {}", model);
    }
    tracing::info!(
        "WebFetch Interceptor: {}",
        if config.web_fetch_enabled { "enabled" } else { "disabled" }
    );
    if config.api_key.is_some() {
        tracing::info!("API Key: configured");
    } else {
        tracing::info!("API Key: not set (using unauthenticated endpoint)");
    }

    let client = Client::builder()
        // CR1-fix: NO global timeout — it kills legitimate streams of 4-6 min.
        // read_timeout is per-chunk: resets on each chunk received from upstream.
        // Streams can run 10+ min as long as data keeps flowing.
        // Non-streaming requests use per-request .timeout(300s) in retry.rs.
        .read_timeout(std::time::Duration::from_secs(chunk_timeout_secs()))
        .connect_timeout(std::time::Duration::from_secs(10))
        // v0.12.0: HTTP Client Hardening (Gap #3, #4)
        .pool_max_idle_per_host(50) // Increased from 10 for multi-agent scenarios
        .pool_idle_timeout(std::time::Duration::from_secs(30)) // Faster release
        .use_rustls_tls()
        // REMOVED: http2_prior_knowledge - breaks HTTP/1.1-only upstreams
        // HTTP/2 is still used via ALPN negotiation when available
        .tcp_nodelay(true) // Reduce latency
        .tcp_keepalive(Some(std::time::Duration::from_secs(60))) // Detect dead connections
        .build()?;

    // v5.0: Auto-discovery model capabilities cache
    let model_cache: proxy::ModelCache =
        Arc::new(tokio::sync::RwLock::new(std::collections::HashMap::new()));

    // v5.0: Per-model concurrency semaphores (Doc1b)
    let model_semaphores: proxy::ModelSemaphores =
        Arc::new(tokio::sync::RwLock::new(std::collections::HashMap::new()));

    // v8.0: Per-model token calibration factors (EMA-based, converges to ~98% accuracy)
    let calibration_factors = tokenizer::CalibrationFactors::new();
    // v0.12.0: Circuit breaker for upstream protection
    // v0.14.1: Configurable CB via CB_ENABLED, CB_THRESHOLD, CB_RECOVERY_SECS
    let circuit_breaker: proxy::CircuitBreaker = if config.cb_enabled {
        Arc::new(circuit_breaker::CircuitBreaker::new(
            config.cb_threshold,
            std::time::Duration::from_secs(config.cb_recovery_secs),
        ))
    } else {
        Arc::new(circuit_breaker::CircuitBreaker::disabled())
    };
    tracing::info!(
        "Circuit breaker: {} (threshold={}, recovery={}s)",
        if config.cb_enabled { "ENABLED" } else { "disabled" },
        config.cb_threshold,
        config.cb_recovery_secs,
    );

    // Save CLI flags for hot-reload
    let cli_debug = config.debug;
    let cli_verbose = config.verbose;
    let cli_port = Some(config.port);

    let config: SharedConfig = Arc::new(arc_swap::ArcSwap::from(Arc::new(config)));

    let cors = match std::env::var("CORS_ALLOWED_ORIGINS") {
        Ok(origins) if !origins.is_empty() => {
            let list: Vec<HeaderValue> = origins
                .split(',')
                .map(|o| o.trim())
                .filter(|o| !o.is_empty())
                .map(|o| o.parse().expect("Invalid CORS origin"))
                .collect();
            tracing::info!("CORS: allowing {} origin(s)", list.len());
            CorsLayer::new().allow_origin(list).allow_methods(Any).allow_headers(Any)
        }
        _ => {
            // Default: localhost only for security
            tracing::info!(
                "CORS: default — localhost:8315 only (set CORS_ALLOWED_ORIGINS to customize)"
            );
            CorsLayer::new()
                .allow_origin(
                    "http://localhost:8315".parse::<HeaderValue>().expect("Valid localhost origin"),
                )
                .allow_methods(Any)
                .allow_headers(Any)
        }
    };

    let app = Router::new()
        .route("/v1/messages", post(proxy::proxy_handler))
        .route("/v1/messages/count_tokens", post(count_tokens_handler))
        .route("/health", axum::routing::get(health_handler))
        .route("/metrics", axum::routing::get(metrics_handler))
        .route("/analytics", axum::routing::get(analytics_handler))
        .layer(Extension(config.clone()))
        .layer(Extension(client))
        .layer(Extension(model_cache))
        .layer(Extension(model_semaphores))
        .layer(Extension(calibration_factors))
        .layer(Extension(circuit_breaker))
        .layer(Extension(prometheus_handle.clone()))
        .layer(TraceLayer::new_for_http())
        .layer(Extension(telemetry_ctx.clone()))
        .layer(cors);

    let addr = format!("0.0.0.0:{}", config.clone().load().port); // v0.11.0 (CR-04)
    let listener = tokio::net::TcpListener::bind(&addr).await?;

    tracing::info!("Listening on {}", addr);
    tracing::info!("Proxy ready to accept requests");
    {
        let cfg = config.load();
        tracing::info!(
            "Concurrency: {} per model, {}s permit timeout",
            cfg.max_concurrent_per_model,
            cfg.permit_timeout_secs
        );
    }

    // Hot-reload config on SIGHUP
    let reload_config = config.clone();
    // v0.11.0 (HI-06): Serialize reloads — prevent SIGHUP/watcher race
    let reload_in_progress = Arc::new(AtomicBool::new(false));
    let reload_flag_sighup = reload_in_progress.clone();
    let reload_flag_watcher = reload_in_progress.clone();
    tokio::spawn(async move {
        let mut sighup = signal(SignalKind::hangup()).expect("Failed to register SIGHUP handler");
        loop {
            sighup.recv().await;
            // S11: Skip config reload if server is draining for shutdown
            if IS_DRAINING.load(std::sync::atomic::Ordering::Relaxed) {
                tracing::warn!("🛑 Server is draining — skipping SIGHUP config reload");
                continue;
            }
            tracing::info!("🔄 SIGHUP received — reloading config...");
            // v0.11.0 (HI-06): Skip if another reload is already in progress
            if reload_flag_sighup
                .compare_exchange(false, true, Ordering::SeqCst, Ordering::Relaxed)
                .is_err()
            {
                tracing::warn!("⚠️ Config reload already in progress (SIGHUP skipped)");
                continue;
            }
            let reload_path = reload_config.load().config_path.clone();
            match Config::reload(cli_debug, cli_verbose, cli_port, reload_path) {
                Ok(new_config) => {
                    let old_maps = reload_config.load().model_map.len();
                    reload_config.store(Arc::new(new_config));
                    let new_maps = reload_config.load().model_map.len();
                    tracing::info!(
                        "✅ Config reloaded: {} model mappings (was {})",
                        new_maps,
                        old_maps
                    );
                }
                Err(e) => {
                    tracing::error!("❌ Config reload failed: {}", e);
                }
            }
            reload_flag_sighup.store(false, Ordering::SeqCst);
        }
    });

    // fix#52: File watcher with notify direct + 5-layer protection stack
    // Layer 1: notify v8 direct — filter ModifyKind::Data (excludes ACCESS/OPEN)
    // Layer 2: Manual debounce (10s) via tokio::select!
    // Layer 3: mtime check (defense-in-depth)
    // Layer 4: Cooldown 15s > debounce 10s (prevents burst reloads)
    // Layer 5: AtomicBool serialization (prevents concurrent SIGHUP+watcher reload)
    let watch_config = config.clone();
    tokio::spawn(async move {
        use notify::event::ModifyKind;
        use notify::{Event, EventKind, RecursiveMode, Watcher};

        // Derive watch_path from stored config (partial fix for #45)
        let env_path = watch_config.load().config_path.clone().unwrap_or_else(|| {
            std::env::var("HOME")
                .map(|h| std::path::PathBuf::from(h).join(".nexus-ai-gateway.env"))
                .unwrap_or_else(|_| std::path::PathBuf::from(".env"))
        });

        if !env_path.exists() {
            tracing::warn!("👁 File watcher: {} not found, skipping", env_path.display());
            return;
        }

        let (tx, mut rx) = tokio::sync::mpsc::channel::<()>(16);
        let rt = tokio::runtime::Handle::current();

        // Layer 1: notify direct — filter only Modify(Data) events
        let mut watcher =
            match notify::recommended_watcher(move |res: Result<Event, notify::Error>| {
                if let Ok(event) = res {
                    if matches!(event.kind, EventKind::Modify(ModifyKind::Data(_))) {
                        let tx = tx.clone();
                        rt.spawn(async move {
                            let _ = tx.send(()).await;
                        });
                    }
                }
            }) {
                Ok(w) => w,
                Err(e) => {
                    tracing::warn!("👁 File watcher init failed: {} (SIGHUP still works)", e);
                    return;
                }
            };

        if let Err(e) = watcher.watch(&env_path, RecursiveMode::NonRecursive) {
            tracing::warn!("👁 Cannot watch {}: {} (SIGHUP still works)", env_path.display(), e);
            return;
        }

        tracing::info!(
            "👁 Watching {} for changes (notify direct, 10s debounce, mtime check, 15s cooldown)",
            env_path.display()
        );

        // Layer 3: mtime tracking (defense-in-depth against spurious Modify events)
        let mut last_mtime = std::fs::metadata(&env_path).and_then(|m| m.modified()).ok();

        // Layer 4: Cooldown (15s > debounce 10s — prevents burst reloads)
        let mut last_reload = std::time::Instant::now() - std::time::Duration::from_secs(20); // Allow first reload immediately

        // Layer 2: Manual debounce with tokio::select!
        let debounce = std::time::Duration::from_secs(10);
        let debounce_sleep = tokio::time::sleep(debounce);
        tokio::pin!(debounce_sleep);
        // Reset to "now" so the first select! branch fires immediately on the first event
        debounce_sleep.as_mut().reset(tokio::time::Instant::now());

        loop {
            tokio::select! {
                // New Modify event arrived → reset debounce timer
                _ = rx.recv() => {
                    debounce_sleep.as_mut().reset(tokio::time::Instant::now() + debounce);
                }
                // Debounce expired → proceed with reload pipeline
                _ = debounce_sleep.as_mut() => {
                    // S11: Skip if server is draining
                    if IS_DRAINING.load(std::sync::atomic::Ordering::Relaxed) {
                        tracing::debug!("🛑 Server is draining — skipping file watcher reload");
                        debounce_sleep.as_mut().reset(tokio::time::Instant::now() + debounce);
                        continue;
                    }

                    // Layer 3: mtime check (defense-in-depth)
                    let current_mtime = std::fs::metadata(&env_path)
                        .and_then(|m| m.modified())
                        .ok();
                    if current_mtime == last_mtime {
                        tracing::debug!("🔄 .env modify event but mtime unchanged — skipping reload");
                        debounce_sleep.as_mut().reset(tokio::time::Instant::now() + debounce);
                        continue;
                    }
                    last_mtime = current_mtime;

                    // Layer 4: Cooldown check (15s minimum between reloads)
                    if last_reload.elapsed().as_secs() < 15 {
                        tracing::debug!(
                            "🔄 .env change debounced (cooldown {}s < 15s)",
                            last_reload.elapsed().as_secs()
                        );
                        debounce_sleep.as_mut().reset(tokio::time::Instant::now() + debounce);
                        continue;
                    }
                    last_reload = std::time::Instant::now();

                    // Layer 5: AtomicBool serialization
                    if reload_flag_watcher
                        .compare_exchange(false, true, Ordering::SeqCst, Ordering::Relaxed)
                        .is_err()
                    {
                        tracing::warn!("⚠️ Config reload already in progress (watcher skipped)");
                        debounce_sleep.as_mut().reset(tokio::time::Instant::now() + debounce);
                        continue;
                    }

                    tracing::info!("🔄 .env changed — auto-reloading config...");
                    let reload_path = watch_config.load().config_path.clone();
                    match Config::reload(cli_debug, cli_verbose, cli_port, reload_path) {
                        Ok(new_config) => {
                            let old_maps = watch_config.load().model_map.len();
                            watch_config.store(Arc::new(new_config));
                            let new_maps = watch_config.load().model_map.len();
                            tracing::info!(
                                "✅ Config auto-reloaded: {} model mappings (was {})",
                                new_maps, old_maps
                            );
                        }
                        Err(e) => {
                            tracing::error!("❌ Auto-reload failed: {}", e);
                        }
                    }
                    reload_flag_watcher.store(false, Ordering::SeqCst);

                    // Reset timer for next cycle
                    debounce_sleep.as_mut().reset(tokio::time::Instant::now() + debounce);
                }
            }
        }
    });

    // v0.13.0: Graceful shutdown with configurable drain timeout
    let drain_timeout_secs: u64 =
        std::env::var("DRAIN_TIMEOUT_SECS").ok().and_then(|v| v.parse().ok()).unwrap_or(30);
    tracing::info!("Drain timeout: {}s", drain_timeout_secs);

    // CR4/CR8 fix: Server must be spawned immediately so it starts accepting
    // connections. Drain timeout starts only AFTER the shutdown signal.
    // We use a dedicated CancellationToken for the server's graceful shutdown,
    // separate from SHUTDOWN_TOKEN (which cancels SSE streams). The flow is:
    // 1. Server is spawned — begins serving immediately
    // 2. shutdown_signal() fires → sets IS_DRAINING, cancels SHUTDOWN_TOKEN
    // 3. We then cancel server_shutdown_token → server stops accepting
    // 4. Drain timeout starts → races against server task completing
    let server_shutdown_token = tokio_util::sync::CancellationToken::new();
    let server_ct_for_graceful = server_shutdown_token.clone();
    let server_handle = tokio::spawn(async move {
        axum::serve(listener, app.into_make_service_with_connect_info::<std::net::SocketAddr>())
            .with_graceful_shutdown(async move {
                server_ct_for_graceful.cancelled().await;
            })
            .await
    });

    // Phase 1: Normal serving — wait for shutdown signal
    // (sets IS_DRAINING=true, cancels SHUTDOWN_TOKEN for SSE streams)
    shutdown_signal().await;

    // Phase 2: Signal server to stop accepting new connections
    server_shutdown_token.cancel();

    // Phase 3: Drain — race server task completion against drain timeout
    let drain_timeout = tokio::time::Duration::from_secs(drain_timeout_secs);
    tokio::select! {
        result = server_handle => {
            match result {
                Ok(Ok(())) => {
                    tracing::info!("✅ Shutdown complete: all connections drained before timeout");
                }
                Ok(Err(e)) => {
                    tracing::error!("Server error: {}", e);
                }
                Err(e) => {
                    tracing::error!("Server task failed: {}", e);
                }
            }
        }
        _ = tokio::time::sleep(drain_timeout) => {
            tracing::warn!(
                "⏱️ Drain timeout ({}s) exceeded — forcing shutdown ({} active connections)",
                drain_timeout_secs,
                proxy::ACTIVE_CONNECTIONS.load(Ordering::Relaxed)
            );
        }
    }

    Ok(())
}
async fn health_handler() -> Response {
    if IS_DRAINING.load(Ordering::Relaxed) {
        return Response::builder()
            .status(503)
            .header("content-type", "text/plain")
            .body("Service Unavailable: draining for shutdown".into())
            .unwrap();
    }
    Response::builder().status(200).header("content-type", "text/plain").body("OK".into()).unwrap()
}

/// Phase 4.5: Prometheus metrics endpoint
async fn metrics_handler(Extension(prometheus_handle): Extension<PrometheusHandle>) -> Response {
    let response_body = prometheus_handle.render();
    Response::builder()
        .status(200)
        .header("content-type", "text/plain; charset=utf-8")
        .body(response_body.into())
        .unwrap()
}

/// v0.18.0: Analytics endpoint — returns aggregated telemetry stats (zero PII)
async fn analytics_handler(
    Extension(telemetry_ctx): Extension<Option<crate::telemetry::TelemetryContext>>,
) -> Response {
    let Some(ctx) = telemetry_ctx else {
        return Response::builder()
            .status(200)
            .header("content-type", "application/json")
            .body(r#"{"error":"telemetry_disabled"}"#.into())
            .unwrap();
    };

    match tokio::task::spawn_blocking(move || ctx.store.get_daily_stats(30)).await {
        Ok(Ok(stats)) => {
            let body = serde_json::to_string(&stats).unwrap_or_else(|_| "[]".to_string());
            Response::builder()
                .status(200)
                .header("content-type", "application/json")
                .body(body.into())
                .unwrap()
        }
        _ => Response::builder()
            .status(500)
            .header("content-type", "application/json")
            .body(r#"{"error":"query_failed"}"#.into())
            .unwrap(),
    }
}

/// Shutdown signal handler for graceful shutdown
/// Handles SIGINT (Ctrl+C) and SIGTERM
async fn shutdown_signal() {
    // Handle SIGINT (Ctrl+C)
    let ctrl_c = async {
        tokio::signal::ctrl_c().await.expect("Failed to install CTRL+C handler");
    };

    // Handle SIGTERM (Unix only)
    #[cfg(unix)]
    let sigterm = async {
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("Failed to install SIGTERM handler")
            .recv()
            .await;
    };

    // On non-Unix, only wait for Ctrl+C
    #[cfg(not(unix))]
    let sigterm = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {
            tracing::info!("🛑 CTRL+C received, draining connections...");
        },
        _ = sigterm => {
            tracing::info!("🛑 SIGTERM received, draining connections...");
        },
    }

    // Signal that server is draining — /health returns 503, retries fail-fast
    IS_DRAINING.store(true, Ordering::Relaxed);
    SHUTDOWN_TOKEN.cancel();
    let active = proxy::ACTIVE_CONNECTIONS.load(Ordering::Relaxed);
    tracing::info!("🔄 Draining {} active connections...", active);
}

/// Phase 12.2: Token count estimation endpoint
/// v7.0: Delegates to tokenizer module (tiktoken cl100k_base, ~95% accuracy)
async fn count_tokens_handler(
    axum::Json(req): axum::Json<serde_json::Value>,
) -> axum::Json<serde_json::Value> {
    let input_tokens = tokenizer::estimate_input_tokens(&req);
    axum::Json(serde_json::json!({
        "input_tokens": input_tokens
    }))
}

fn handle_scan(env: bool, launcher: bool, check: bool) -> anyhow::Result<()> {
    let scan_result = scan::scan_cc_binary().map_err(|e| anyhow::anyhow!(e))?;

    if check {
        // Just check for updates against saved state
        let state_path = std::env::var("HOME")
            .map(|h| std::path::PathBuf::from(h).join(".nexus-ai-gateway-scan.json"))
            .unwrap_or_else(|_| std::path::PathBuf::from("/tmp/nexus-ai-gateway-scan.json"));

        if let Some(old_scan) = watcher::CCWatcher::load_state(&state_path) {
            if old_scan.binary_sha256 == scan_result.binary_sha256 {
                println!(
                    "✅ CC binary unchanged (SHA256: {}...{})",
                    &scan_result.binary_sha256[..8],
                    &scan_result.binary_sha256[scan_result.binary_sha256.len() - 8..]
                );
            } else {
                println!("⚠️ CC binary UPDATED!");
                println!(" Old: {}", old_scan.binary_sha256);
                println!(" New: {}", scan_result.binary_sha256);
                let new_models: Vec<&str> = scan_result
                    .models
                    .iter()
                    .filter(|m| !old_scan.models.iter().any(|o| o.id == m.id))
                    .map(|m| m.id.as_str())
                    .collect();
                if !new_models.is_empty() {
                    println!(" New models: {:?}", new_models);
                }
            }
        } else {
            println!("ℹ️ No previous scan found, performing initial scan");
            scan::display_scan(&scan_result);
        }

        // Save new state
        let watcher = watcher::CCWatcher::new(
            std::path::PathBuf::from(&scan_result.binary_path),
            scan_result.binary_sha256.clone(),
            Some(scan_result),
        );
        watcher.save_state().map_err(|e| anyhow::anyhow!(e))?;
        return Ok(());
    }

    if env {
        let template = scan::generate_env_template(&scan_result);
        print!("{}", template);
        return Ok(());
    }

    if launcher {
        let script = scan::generate_launcher_script(&scan_result);
        print!("{}", script);
        return Ok(());
    }

    // Default: display full scan results
    scan::display_scan(&scan_result);
    Ok(())
}

fn stop_daemon(pid_file: &std::path::Path) -> anyhow::Result<()> {
    if !pid_file.exists() {
        eprintln!("✗ PID file not found: {}", pid_file.display());
        eprintln!(" Daemon is not running or PID file was removed");
        std::process::exit(1);
    }

    let pid_str = std::fs::read_to_string(pid_file)?;
    let pid: i32 =
        pid_str.trim().parse().map_err(|_| anyhow::anyhow!("Invalid PID in file: {}", pid_str))?;

    #[cfg(unix)]
    {
        use std::process::Command;
        let output = Command::new("kill").arg(pid.to_string()).output()?;

        if !output.status.success() {
            eprintln!("✗ Failed to stop daemon (PID: {})", pid);
            eprintln!(" Process may have already exited");
            std::fs::remove_file(pid_file)?;
            std::process::exit(1);
        }

        // S10: Wait for the daemon to exit gracefully (up to 30s)
        // This allows the daemon to drain active connections before we clean up
        eprintln!("⏳ Waiting for daemon (PID: {}) to drain...", pid);
        let start = std::time::Instant::now();
        let max_wait = std::time::Duration::from_secs(30);
        loop {
            let still_running = std::process::Command::new("kill")
                .args(["-0", &pid.to_string()])
                .status()
                .map(|s| s.success())
                .unwrap_or(false);
            if !still_running {
                eprintln!(
                    "✓ Daemon stopped gracefully (PID: {}, elapsed: {:.1}s)",
                    pid,
                    start.elapsed().as_secs_f64()
                );
                break;
            }
            if start.elapsed() > max_wait {
                eprintln!("⚠️ Daemon did not exit after 30s — sending SIGKILL...");
                let _ = Command::new("kill").args(["-9", &pid.to_string()]).output();
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(500));
        }
        std::fs::remove_file(pid_file)?;
    }

    #[cfg(not(unix))]
    {
        eprintln!("✗ Daemon stop is only supported on Unix systems");
        std::process::exit(1);
    }

    Ok(())
}

fn check_status(pid_file: &std::path::Path) -> anyhow::Result<()> {
    if !pid_file.exists() {
        eprintln!("✗ Daemon is not running");
        eprintln!(" PID file not found: {}", pid_file.display());
        std::process::exit(1);
    }

    let pid_str = std::fs::read_to_string(pid_file)?;
    let pid: i32 =
        pid_str.trim().parse().map_err(|_| anyhow::anyhow!("Invalid PID in file: {}", pid_str))?;

    #[cfg(unix)]
    {
        use std::process::Command;
        let output = Command::new("ps").arg("-p").arg(pid.to_string()).output()?;

        if output.status.success() {
            eprintln!("✓ Daemon is running (PID: {})", pid);
            eprintln!(" PID file: {}", pid_file.display());
        } else {
            eprintln!("✗ Daemon is not running");
            eprintln!(" Stale PID file found: {} (PID: {})", pid_file.display(), pid);
            std::process::exit(1);
        }
    }

    #[cfg(not(unix))]
    {
        eprintln!("✗ Daemon status check is only supported on Unix systems");
        std::process::exit(1);
    }

    Ok(())
}

// fix#52: Unit tests for file watcher 5-layer protection stack
#[cfg(test)]
#[path = "watcher_test.rs"]
mod watcher_test;
