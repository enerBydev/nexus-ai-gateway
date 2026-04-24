use anyhow::{Context, Result};
use console::style;
use dialoguer::{Confirm, Input, Password, Select};
use indicatif::{ProgressBar, ProgressStyle};
use serde_json::Value;
use std::fs;
use std::io::Write;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use crate::scan;

// ─── Structs ──────────────────────────────────────────────────────────

struct UpstreamSetup {
    api_key: String,
    base_url: String,
    latency_ms: u64,
    available_models: Vec<String>,
}

struct ModelMapping {
    claude_id: String,
    nim_model: String,
    upstream: String,
}

struct ServerConfig {
    port: u16,
    max_concurrent: usize,
    permit_timeout: u64,
    debug: bool,
}

// ─── Main Entry ───────────────────────────────────────────────────────

/// Run the interactive setup wizard
pub fn run_setup(quick: bool, config_path: Option<PathBuf>) -> Result<()> {
    print_banner();

    // Phase 1: Upstream connection
    let upstream = phase1_upstream(quick)?;

    // Phase 2: Model selection
    let mappings = phase2_models(&upstream, quick)?;

    // Phase 3: Server configuration
    let server = phase3_server(quick)?;

    // Phase 4: CC integration
    phase4_cc_integration(server.port)?;

    // Phase 5: Generate .env
    let env_path = phase5_generate_env(&upstream, &mappings, &server, config_path)?;

    // Phase 6: Install & verify
    phase6_install_verify(server.port, &env_path)?;

    print_success_banner(server.port);
    Ok(())
}

fn print_banner() {
    eprintln!();
    eprintln!(
        "{}",
        style("╔══════════════════════════════════════════════════╗")
            .cyan()
            .bold()
    );
    eprintln!(
        "{}",
        style("║     NEXUS AI Gateway — Setup Wizard              ║")
            .cyan()
            .bold()
    );
    eprintln!(
        "{}",
        style("╚══════════════════════════════════════════════════╝")
            .cyan()
            .bold()
    );
    eprintln!();
}

fn print_success_banner(port: u16) {
    eprintln!();
    eprintln!(
        "{}",
        style("╔══════════════════════════════════════════════════╗")
            .green()
            .bold()
    );
    eprintln!(
        "{}",
        style("║     ✅ Setup Complete!                            ║")
            .green()
            .bold()
    );
    eprintln!(
        "{}",
        style("╚══════════════════════════════════════════════════╝")
            .green()
            .bold()
    );
    eprintln!();
    eprintln!("  {} http://localhost:{}", style("Proxy:").bold(), port);
    eprintln!(
        "  {} nexus-ai-gateway config show",
        style("Check config:").bold()
    );
    eprintln!("  {} nexus-ai-gateway config test", style("Test:").bold());
    eprintln!();
}

// ─── Phase 1: Upstream Connection ─────────────────────────────────────

fn phase1_upstream(quick: bool) -> Result<UpstreamSetup> {
    eprintln!(
        "\n{}",
        style("━━━ Phase 1/6: Upstream Connection ━━━")
            .yellow()
            .bold()
    );

    let base_url: String = if quick {
        "https://integrate.api.nvidia.com".to_string()
    } else {
        Input::new()
            .with_prompt("Upstream API URL")
            .default("https://integrate.api.nvidia.com".to_string())
            .interact_text()?
    };

    let api_key: String = Password::new()
        .with_prompt("NVIDIA NIM API Key")
        .interact()?;

    if api_key.is_empty() {
        anyhow::bail!("API key cannot be empty");
    }

    // Validate connection
    let spinner = ProgressBar::new_spinner();
    spinner.set_style(
        ProgressStyle::with_template("{spinner:.cyan} {msg}")
            .unwrap()
            .tick_strings(&["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"]),
    );
    spinner.set_message("Validating API key...");
    spinner.enable_steady_tick(Duration::from_millis(80));

    let start = Instant::now();
    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(30))
        .build()?;

    let url = format!("{}/v1/models", base_url.trim_end_matches('/'));
    let resp = client
        .get(&url)
        .header("Authorization", format!("Bearer {}", api_key))
        .send()
        .context("Failed to connect to upstream API")?;

    let latency_ms = start.elapsed().as_millis() as u64;
    let status = resp.status();

    if status == reqwest::StatusCode::UNAUTHORIZED {
        spinner.finish_with_message("❌ Invalid API key");
        anyhow::bail!("API key is invalid (401 Unauthorized). Check your NVIDIA NIM API key.");
    }

    if !status.is_success() {
        spinner.finish_with_message(format!("❌ Upstream error: {}", status));
        anyhow::bail!("Upstream returned {} — check your URL", status);
    }

    let body: Value = resp.json().context("Failed to parse upstream response")?;
    let models: Vec<String> = body
        .get("data")
        .and_then(|d| d.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|m| m.get("id").and_then(|id| id.as_str()).map(String::from))
                .collect()
        })
        .unwrap_or_default();

    spinner.finish_with_message(format!(
        "✅ Connected! {}ms latency, {} models available",
        latency_ms,
        models.len()
    ));

    Ok(UpstreamSetup {
        api_key,
        base_url,
        latency_ms,
        available_models: models,
    })
}

// ─── Phase 2: Model Selection ─────────────────────────────────────────

fn phase2_models(upstream: &UpstreamSetup, quick: bool) -> Result<Vec<ModelMapping>> {
    eprintln!(
        "\n{}",
        style("━━━ Phase 2/6: Model Selection ━━━").yellow().bold()
    );

    // Scan CC binary for ClaudeModelIDs
    let spinner = ProgressBar::new_spinner();
    spinner.set_style(
        ProgressStyle::with_template("{spinner:.cyan} {msg}")
            .unwrap()
            .tick_strings(&["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"]),
    );
    spinner.set_message("Scanning Claude Code binary...");
    spinner.enable_steady_tick(Duration::from_millis(80));

    let scan_result = scan::scan_cc_binary();
    spinner.finish_with_message("✅ CC binary scanned");

    // Extract ClaudeModelIDs and categorize
    let claude_ids: Vec<String> = scan_result
        .as_ref()
        .map(|s| s.models.iter().map(|m| m.id.clone()).collect())
        .unwrap_or_default();

    let opus_ids: Vec<&str> = claude_ids
        .iter()
        .filter(|id| id.contains("opus"))
        .map(|s| s.as_str())
        .collect();
    let sonnet_ids: Vec<&str> = claude_ids
        .iter()
        .filter(|id| id.contains("sonnet"))
        .map(|s| s.as_str())
        .collect();
    let haiku_ids: Vec<&str> = claude_ids
        .iter()
        .filter(|id| id.contains("haiku"))
        .map(|s| s.as_str())
        .collect();

    eprintln!(
        "  Found {} ClaudeModelIDs: {} opus, {} sonnet, {} haiku",
        claude_ids.len(),
        opus_ids.len(),
        sonnet_ids.len(),
        haiku_ids.len()
    );

    // Filter NIM models (only chat models, exclude embedding/reranking)
    let chat_models: Vec<&String> = upstream
        .available_models
        .iter()
        .filter(|m| {
            !m.contains("embed")
                && !m.contains("rerank")
                && !m.contains("nv-")
                && !m.contains("vlm")
        })
        .collect();

    if chat_models.is_empty() {
        anyhow::bail!("No chat models found on upstream. Check your API key and URL.");
    }

    let model_names: Vec<&str> = chat_models.iter().map(|s| s.as_str()).collect();

    // Select models for each tier
    let boss_model = if quick {
        // Auto-select first matching recommended model
        find_recommended(&model_names, &["glm5", "kimi-k2", "qwen3.5"])
            .unwrap_or_else(|| model_names[0].to_string())
    } else {
        eprintln!(
            "\n  {} (for Opus — reasoning, boss agent)",
            style("Select BOSS model").bold()
        );
        let idx = Select::new()
            .items(&model_names)
            .default(find_index(&model_names, "glm5").unwrap_or(0))
            .interact()?;
        model_names[idx].to_string()
    };

    let agent_model = if quick {
        find_recommended(&model_names, &["kimi-k2", "glm4", "nemotron-ultra"])
            .unwrap_or_else(|| boss_model.clone())
    } else {
        eprintln!(
            "\n  {} (for Sonnet — workhorse, subagents)",
            style("Select AGENT model").bold()
        );
        let idx = Select::new()
            .items(&model_names)
            .default(find_index(&model_names, "kimi-k2").unwrap_or(0))
            .interact()?;
        model_names[idx].to_string()
    };

    let fast_model = if quick {
        find_recommended(
            &model_names,
            &["kimi-k2", "deepseek-v3", "nemotron-3-super"],
        )
        .unwrap_or_else(|| agent_model.clone())
    } else {
        eprintln!(
            "\n  {} (for Haiku — fast, simple tasks)",
            style("Select FAST model").bold()
        );
        let idx = Select::new()
            .items(&model_names)
            .default(find_index(&model_names, "kimi-k2").unwrap_or(0))
            .interact()?;
        model_names[idx].to_string()
    };

    eprintln!("\n  {}", style("Model assignments:").bold().underlined());
    eprintln!("    Boss  (Opus):   {}", style(&boss_model).green());
    eprintln!("    Agent (Sonnet): {}", style(&agent_model).green());
    eprintln!("    Fast  (Haiku):  {}", style(&fast_model).green());

    // Generate MODEL_MAP entries
    let mut mappings = Vec::new();
    for id in &opus_ids {
        mappings.push(ModelMapping {
            claude_id: id.to_string(),
            nim_model: boss_model.clone(),
            upstream: "default".to_string(),
        });
    }
    for id in &sonnet_ids {
        mappings.push(ModelMapping {
            claude_id: id.to_string(),
            nim_model: agent_model.clone(),
            upstream: "default".to_string(),
        });
    }
    for id in &haiku_ids {
        mappings.push(ModelMapping {
            claude_id: id.to_string(),
            nim_model: fast_model.clone(),
            upstream: "default".to_string(),
        });
    }

    eprintln!(
        "  Generated {} MODEL_MAP entries",
        style(mappings.len()).cyan()
    );
    Ok(mappings)
}

fn find_recommended(models: &[&str], patterns: &[&str]) -> Option<String> {
    for pattern in patterns {
        if let Some(model) = models.iter().find(|m| m.contains(pattern)) {
            return Some(model.to_string());
        }
    }
    None
}

fn find_index(models: &[&str], pattern: &str) -> Option<usize> {
    models.iter().position(|m| m.contains(pattern))
}

// ─── Phase 3: Server Configuration ───────────────────────────────────

fn phase3_server(quick: bool) -> Result<ServerConfig> {
    eprintln!(
        "\n{}",
        style("━━━ Phase 3/6: Server Configuration ━━━")
            .yellow()
            .bold()
    );

    if quick {
        eprintln!("  Using defaults: port=8315, concurrent=5, timeout=180s");
        return Ok(ServerConfig {
            port: 8315,
            max_concurrent: 5,
            permit_timeout: 180,
            debug: false,
        });
    }

    let port: u16 = Input::new()
        .with_prompt("Proxy port")
        .default(8315)
        .interact_text()?;

    let max_concurrent: usize = Input::new()
        .with_prompt("Max concurrent requests per model")
        .default(5)
        .interact_text()?;

    let permit_timeout: u64 = Input::new()
        .with_prompt("Concurrency timeout (seconds)")
        .default(180)
        .interact_text()?;

    let debug = Confirm::new()
        .with_prompt("Enable debug logging?")
        .default(false)
        .interact()?;

    Ok(ServerConfig {
        port,
        max_concurrent,
        permit_timeout,
        debug,
    })
}

// ─── Phase 4: CC Integration ─────────────────────────────────────────

fn phase4_cc_integration(port: u16) -> Result<()> {
    eprintln!(
        "\n{}",
        style("━━━ Phase 4/6: Claude Code Integration ━━━")
            .yellow()
            .bold()
    );

    // 4a: ~/.claude/settings.json
    configure_claude_settings(port)?;

    // 4b: ~/.bashrc claude wrapper
    configure_bashrc_wrapper()?;

    Ok(())
}

fn configure_claude_settings(port: u16) -> Result<()> {
    let home = std::env::var("HOME").context("HOME not set")?;
    let settings_path = PathBuf::from(&home).join(".claude").join("settings.json");

    eprintln!("  Configuring {}...", style(settings_path.display()).dim());

    let mut settings: Value = if settings_path.exists() {
        let content = fs::read_to_string(&settings_path)?;
        serde_json::from_str(&content).unwrap_or(serde_json::json!({}))
    } else {
        // Create .claude directory if needed
        if let Some(parent) = settings_path.parent() {
            fs::create_dir_all(parent)?;
        }
        serde_json::json!({})
    };

    // Merge env settings
    let env_obj = settings
        .as_object_mut()
        .context("settings.json is not an object")?
        .entry("env")
        .or_insert(serde_json::json!({}));

    if let Some(env_map) = env_obj.as_object_mut() {
        env_map.insert(
            "ANTHROPIC_BASE_URL".to_string(),
            serde_json::json!(format!("http://localhost:{}", port)),
        );
        env_map.insert(
            "ANTHROPIC_API_KEY".to_string(),
            serde_json::json!("proxy-key"),
        );
        env_map.insert(
            "CLAUDE_CODE_DISABLE_LEGACY_MODEL_REMAP".to_string(),
            serde_json::json!("true"),
        );
    }

    let json_str = serde_json::to_string_pretty(&settings)?;
    fs::write(&settings_path, json_str)?;
    eprintln!("  {} ~/.claude/settings.json", style("✅").green());

    Ok(())
}

fn configure_bashrc_wrapper() -> Result<()> {
    let home = std::env::var("HOME").context("HOME not set")?;
    let bashrc_path = PathBuf::from(&home).join(".bashrc");

    let marker = "# === NEXUS-AI-Gateway: Force effort max";

    if bashrc_path.exists() {
        let content = fs::read_to_string(&bashrc_path)?;
        if content.contains(marker) {
            eprintln!(
                "  {} ~/.bashrc (wrapper already installed)",
                style("✅").green()
            );
            return Ok(());
        }
    }

    let wrapper = r#"

# === NEXUS-AI-Gateway: Force effort max on all CC sessions ===
claude() {
    if [[ " $* " == *" --effort "* ]]; then
        command claude "$@"
    else
        command claude --effort max "$@"
    fi
}
"#;

    let mut file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&bashrc_path)?;

    file.write_all(wrapper.as_bytes())?;
    eprintln!(
        "  {} ~/.bashrc (claude --effort max wrapper added)",
        style("✅").green()
    );

    Ok(())
}

// ─── Phase 5: Generate .env ──────────────────────────────────────────

fn phase5_generate_env(
    upstream: &UpstreamSetup,
    mappings: &[ModelMapping],
    server: &ServerConfig,
    config_path: Option<PathBuf>,
) -> Result<PathBuf> {
    eprintln!(
        "\n{}",
        style("━━━ Phase 5/6: Generate Configuration ━━━")
            .yellow()
            .bold()
    );

    let env_path = match config_path {
        Some(p) => p,
        None => {
            let home = std::env::var("HOME").context("HOME not set")?;
            PathBuf::from(&home).join(".nexus-ai-gateway.env")
        }
    };

    // Backup if exists
    if env_path.exists() {
        let backup_path = env_path.with_extension("env.bak");
        fs::copy(&env_path, &backup_path)?;
        eprintln!(
            "  {} Backed up existing .env to .env.bak",
            style("📋").dim()
        );
    }

    let mut env_content = String::new();

    // Header
    env_content.push_str(&format!(
        "# NEXUS AI Gateway Configuration\n# Generated by setup wizard on {}\n# Version: {}\n\n",
        chrono::Local::now().format("%Y-%m-%d %H:%M:%S"),
        nexus_ai_gateway::VERSION
    ));

    // Server section
    env_content.push_str("# ─── Server Configuration ───────────────────────\n");
    env_content.push_str(&format!("PORT={}\n", server.port));
    if server.debug {
        env_content.push_str("DEBUG=true\n");
    }
    env_content.push('\n');

    // Upstream section
    env_content.push_str("# ─── Upstream Configuration ─────────────────────\n");
    env_content.push_str(&format!("UPSTREAM_BASE_URL={}\n", upstream.base_url));
    env_content.push_str(&format!("UPSTREAM_API_KEY={}\n", upstream.api_key));
    env_content.push('\n');

    // Concurrency section
    env_content.push_str("# ─── Concurrency Configuration ──────────────────\n");
    env_content.push_str(&format!(
        "MAX_CONCURRENT_PER_MODEL={}\n",
        server.max_concurrent
    ));
    env_content.push_str(&format!("PERMIT_TIMEOUT_SECS={}\n", server.permit_timeout));
    env_content.push('\n');

    // Model mappings section
    env_content.push_str("# ─── Model Mappings (ClaudeID → upstream:NIMmodel) ─\n");
    for mapping in mappings {
        let env_key = mapping.claude_id.replace('-', "_");
        env_content.push_str(&format!(
            "MODEL_MAP_{}={}:{}\n",
            env_key, mapping.upstream, mapping.nim_model
        ));
    }
    env_content.push('\n');

    // WebFetch section
    env_content.push_str("# ─── WebFetch Configuration ─────────────────────\n");
    env_content.push_str("WEB_FETCH_ENABLED=true\n");
    env_content.push_str("WEB_FETCH_MAX_RETRIES=3\n");
    env_content.push_str("WEB_FETCH_TIMEOUT_SECS=15\n");

    fs::write(&env_path, &env_content)?;

    // FASE 3.4: Set .env file permissions to 600 (owner-only read/write) on Unix
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Err(e) = std::fs::set_permissions(&env_path, std::fs::Permissions::from_mode(0o600))
        {
            tracing::warn!("Failed to set .env permissions to 600: {}", e);
        } else {
            tracing::info!("Set .env permissions to 600 (owner-only read/write)");
        }
    }
    eprintln!(
        "  {} {}",
        style("✅").green(),
        style(env_path.display()).bold()
    );
    eprintln!(
        "  {} entries: {} model mappings, {}ms upstream latency",
        style("📊").dim(),
        mappings.len(),
        upstream.latency_ms
    );

    Ok(env_path)
}

// ─── Phase 6: Install & Verify ───────────────────────────────────────

fn phase6_install_verify(port: u16, _env_path: &PathBuf) -> Result<()> {
    eprintln!(
        "\n{}",
        style("━━━ Phase 6/6: Install & Verify ━━━").yellow().bold()
    );

    // Check if systemd service exists
    let home = std::env::var("HOME").context("HOME not set")?;
    let service_path = PathBuf::from(&home).join(".config/systemd/user/nexus-ai-gateway.service");

    if service_path.exists() {
        eprintln!("  Systemd service found, restarting...");
        let status = std::process::Command::new("systemctl")
            .args(["--user", "restart", "nexus-ai-gateway"])
            .status();

        match status {
            Ok(s) if s.success() => {
                eprintln!("  {} Service restarted", style("✅").green());
                // Wait for service to start
                std::thread::sleep(Duration::from_secs(2));
            }
            _ => {
                eprintln!(
                    "  {} Failed to restart service — you may need to restart manually",
                    style("⚠️").yellow()
                );
            }
        }
    } else {
        eprintln!(
            "  {} No systemd service installed. Run: task service-install",
            style("ℹ️").blue()
        );
    }

    // Health check
    let health_url = format!("http://localhost:{}/health", port);
    eprintln!("  Checking health at {}...", &health_url);

    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()?;

    match client.get(&health_url).send() {
        Ok(resp) if resp.status().is_success() => {
            eprintln!("  {} Proxy is healthy!", style("✅").green());
        }
        Ok(resp) => {
            eprintln!(
                "  {} Proxy responded with {} — may need restart",
                style("⚠️").yellow(),
                resp.status()
            );
        }
        Err(_) => {
            eprintln!(
                "  {} Proxy not reachable — start with: nexus-ai-gateway",
                style("ℹ️").blue()
            );
        }
    }

    Ok(())
}
