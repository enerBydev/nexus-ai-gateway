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
mod tokenizer;
mod transform;
mod watcher;
mod web_fetch;

use axum::{routing::post, Extension, Router};
use clap::Parser;
use cli::{Cli, Command};
use config::{Config, SharedConfig};
use daemonize::Daemonize;
use reqwest::Client;
use std::sync::atomic::{AtomicBool, Ordering}; // v0.11.0 (HI-06): config reload mutex
use std::sync::{Arc, RwLock};
use tokio::signal::unix::{signal, SignalKind};
use tower_http::{
    cors::{Any, CorsLayer},
    trace::TraceLayer,
};
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

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
            Command::Scan {
                env,
                launcher,
                check,
            } => {
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

        let stdout = OpenOptions::new()
            .create(true)
            .append(true)
            .open("/tmp/nexus-ai-gateway.log")?;

        let stderr = OpenOptions::new()
            .create(true)
            .append(true)
            .open("/tmp/nexus-ai-gateway.log")?;

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
        if config.web_fetch_enabled {
            "enabled"
        } else {
            "disabled"
        }
    );
    if config.api_key.is_some() {
        tracing::info!("API Key: configured");
    } else {
        tracing::info!("API Key: not set (using unauthenticated endpoint)");
    }

    let client = Client::builder()
        .timeout(std::time::Duration::from_secs(300))
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
    let circuit_breaker: proxy::CircuitBreaker = Arc::new(circuit_breaker::CircuitBreaker::new(
        3,
        std::time::Duration::from_secs(30),
    ));

    // Save CLI flags for hot-reload
    let cli_debug = config.debug;
    let cli_verbose = config.verbose;
    let cli_port = Some(config.port);

    let config: SharedConfig = Arc::new(RwLock::new(config));

    let cors = CorsLayer::new()
        .allow_origin(Any)
        .allow_methods(Any)
        .allow_headers(Any);

    let app = Router::new()
        .route("/v1/messages", post(proxy::proxy_handler))
        .route("/v1/messages/count_tokens", post(count_tokens_handler))
        .route("/health", axum::routing::get(health_handler))
        .layer(Extension(config.clone()))
        .layer(Extension(client))
        .layer(Extension(model_cache))
        .layer(Extension(model_semaphores))
        .layer(Extension(calibration_factors))
        .layer(Extension(circuit_breaker))
        .layer(TraceLayer::new_for_http())
        .layer(cors);

    let addr = format!(
        "0.0.0.0:{}",
        config.read().unwrap_or_else(|e| e.into_inner()).port
    ); // v0.11.0 (CR-04)
    let listener = tokio::net::TcpListener::bind(&addr).await?;

    tracing::info!("Listening on {}", addr);
    tracing::info!("Proxy ready to accept requests");
    {
        let cfg = config.read().unwrap_or_else(|e| e.into_inner()); // v0.11.0 (CR-04)
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
            tracing::info!("🔄 SIGHUP received — reloading config...");
            // v0.11.0 (HI-06): Skip if another reload is already in progress
            if reload_flag_sighup
                .compare_exchange(false, true, Ordering::SeqCst, Ordering::Relaxed)
                .is_err()
            {
                tracing::warn!("⚠️ Config reload already in progress (SIGHUP skipped)");
                continue;
            }
            match Config::reload(cli_debug, cli_verbose, cli_port) {
                Ok(new_config) => {
                    let mut cfg = reload_config.write().unwrap_or_else(|e| e.into_inner()); // v0.11.0 (CR-04)
                    let old_maps = cfg.model_map.len();
                    *cfg = new_config;
                    tracing::info!(
                        "✅ Config reloaded: {} model mappings (was {})",
                        cfg.model_map.len(),
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

    // v5.0: File watcher for auto-reload on .env changes (Doc1 Component 1)
    let watch_config = config.clone();
    tokio::spawn(async move {
        use notify_debouncer_mini::{new_debouncer, DebouncedEventKind};

        let env_path = std::env::var("HOME")
            .map(|h| std::path::PathBuf::from(h).join(".nexus-ai-gateway.env"))
            .unwrap_or_else(|_| std::path::PathBuf::from(".env"));

        if !env_path.exists() {
            tracing::warn!("👁 File watcher: {} not found, skipping", env_path.display());
            return;
        }

        let (tx, mut rx) = tokio::sync::mpsc::channel(10);

        let rt = tokio::runtime::Handle::current();
        let mut debouncer = match new_debouncer(
            std::time::Duration::from_secs(10), // > cooldown * 2 to prevent burst reloads
            move |events: Result<Vec<notify_debouncer_mini::DebouncedEvent>, _>| {
                if let Ok(evts) = events {
                    for evt in evts {
                        if evt.kind == DebouncedEventKind::Any {
                            let tx = tx.clone();
                            rt.spawn(async move {
                                let _ = tx.send(()).await;
                            });
                        }
                    }
                }
            },
        ) {
            Ok(d) => d,
            Err(e) => {
                tracing::warn!("👁 File watcher init failed: {} (SIGHUP still works)", e);
                return;
            }
        };

        if let Err(e) = debouncer
            .watcher()
            .watch(&env_path, notify::RecursiveMode::NonRecursive)
        {
            tracing::warn!(
                "👁 Cannot watch {}: {} (SIGHUP still works)",
                env_path.display(),
                e
            );
            return;
        }

        tracing::info!(
            "👁 Watching {} for changes (auto-reload enabled, 10s debounce)",
            env_path.display()
        );

        // v10.3: Add cooldown to prevent rapid-fire reloads from write lock contention
        let mut last_reload = std::time::Instant::now();
        while rx.recv().await.is_some() {
            if last_reload.elapsed().as_secs() < 5 {
                // Increased for stability
                tracing::debug!("🔄 .env change debounced (cooldown)");
                continue;
            }
            last_reload = std::time::Instant::now();
            tracing::info!("🔄 .env changed — auto-reloading config...");
            // v0.11.0 (HI-06): Skip if another reload is already in progress
            if reload_flag_watcher
                .compare_exchange(false, true, Ordering::SeqCst, Ordering::Relaxed)
                .is_err()
            {
                tracing::warn!("⚠️ Config reload already in progress (watcher skipped)");
                continue;
            }
            match Config::reload(cli_debug, cli_verbose, cli_port) {
                Ok(new_config) => {
                    let mut cfg = watch_config.write().unwrap_or_else(|e| e.into_inner()); // v0.11.0 (CR-04)
                    let old_maps = cfg.model_map.len();
                    *cfg = new_config;
                    tracing::info!(
                        "✅ Config auto-reloaded: {} model mappings (was {})",
                        cfg.model_map.len(),
                        old_maps
                    );
                }
                Err(e) => {
                    tracing::error!("❌ Auto-reload failed: {}", e);
                }
            }
            reload_flag_watcher.store(false, Ordering::SeqCst);
        }
    });

    axum::serve(listener, app).await?;

    Ok(())
}

async fn health_handler() -> &'static str {
    "OK"
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
                println!("   Old: {}", old_scan.binary_sha256);
                println!("   New: {}", scan_result.binary_sha256);
                let new_models: Vec<&str> = scan_result
                    .models
                    .iter()
                    .filter(|m| !old_scan.models.iter().any(|o| o.id == m.id))
                    .map(|m| m.id.as_str())
                    .collect();
                if !new_models.is_empty() {
                    println!("   New models: {:?}", new_models);
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
        eprintln!("  Daemon is not running or PID file was removed");
        std::process::exit(1);
    }

    let pid_str = std::fs::read_to_string(pid_file)?;
    let pid: i32 = pid_str
        .trim()
        .parse()
        .map_err(|_| anyhow::anyhow!("Invalid PID in file: {}", pid_str))?;

    #[cfg(unix)]
    {
        use std::process::Command;
        let output = Command::new("kill").arg(pid.to_string()).output()?;

        if output.status.success() {
            std::fs::remove_file(pid_file)?;
            eprintln!("✓ Daemon stopped (PID: {})", pid);
        } else {
            eprintln!("✗ Failed to stop daemon (PID: {})", pid);
            eprintln!("  Process may have already exited");
            std::fs::remove_file(pid_file)?;
            std::process::exit(1);
        }
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
        eprintln!("  PID file not found: {}", pid_file.display());
        std::process::exit(1);
    }

    let pid_str = std::fs::read_to_string(pid_file)?;
    let pid: i32 = pid_str
        .trim()
        .parse()
        .map_err(|_| anyhow::anyhow!("Invalid PID in file: {}", pid_str))?;

    #[cfg(unix)]
    {
        use std::process::Command;
        let output = Command::new("ps").arg("-p").arg(pid.to_string()).output()?;

        if output.status.success() {
            eprintln!("✓ Daemon is running (PID: {})", pid);
            eprintln!("  PID file: {}", pid_file.display());
        } else {
            eprintln!("✗ Daemon is not running");
            eprintln!(
                "  Stale PID file found: {} (PID: {})",
                pid_file.display(),
                pid
            );
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
