use crate::cli::ConfigAction;
use crate::config::Config;
use anyhow::{Context, Result};
use console::style;
use std::fs;
use std::path::PathBuf;
use std::time::{Duration, Instant};

/// Handle `nexus-ai-gateway config <action>` subcommands
pub fn handle_config(action: ConfigAction, config_path: Option<PathBuf>) -> Result<()> {
    match action {
        ConfigAction::Show => config_show(config_path),
        ConfigAction::Set { key, value } => config_set(&key, &value, config_path.as_deref()),
        ConfigAction::Test => config_test(config_path),
    }
}

// ─── config show ──────────────────────────────────────────────────────

fn config_show(config_path: Option<PathBuf>) -> Result<()> {
    let config = Config::from_env_with_path(config_path).context("Failed to load config")?;

    eprintln!();
    eprintln!("{}", style("╔══════════════════════════════════════════════════╗").cyan().bold());
    eprintln!("{}", style("║     NEXUS AI Gateway — Current Configuration     ║").cyan().bold());
    eprintln!("{}", style("╚══════════════════════════════════════════════════╝").cyan().bold());

    // Server section
    eprintln!("\n  {}", style("━━━ Server ━━━").yellow().bold());
    eprintln!("    Port:            {}", style(config.port).green());
    eprintln!(
        "    Debug:           {}",
        if config.debug { style("true").yellow() } else { style("false").dim() }
    );
    eprintln!(
        "    Verbose:         {}",
        if config.verbose { style("true").yellow() } else { style("false").dim() }
    );

    // Concurrency section
    eprintln!("\n  {}", style("━━━ Concurrency ━━━").yellow().bold());
    eprintln!("    Max per model:   {}", style(config.max_concurrent_per_model).green());
    eprintln!("    Permit timeout:  {}s", style(config.permit_timeout_secs).green());

    // Upstream section
    eprintln!("\n  {}", style("━━━ Upstream ━━━").yellow().bold());
    eprintln!(" Type: {}", style(&config.upstream_type).green());
    eprintln!("    Base URL:        {}", style(&config.base_url).green());
    eprintln!("    API Key:         {}", mask_key(&config.api_key));

    // Additional upstreams
    for (name, upstream) in &config.upstreams {
        if name != "default" {
            let type_str = upstream
                .upstream_type
                .map(|t| t.to_string())
                .unwrap_or_else(|| format!("{} (global)", config.upstream_type));
            eprintln!("    {} URL:   {}", style(name).bold(), style(&upstream.base_url).green());
            eprintln!("    {} Key:   {}", style(name).bold(), mask_key(&upstream.api_key));
            eprintln!(" {} Type: {}", style(name).bold(), style(&type_str).cyan());
        }
    }

    // Model mappings
    eprintln!("\n  {}", style("━━━ Model Mappings ━━━").yellow().bold());
    if config.model_map.is_empty() {
        eprintln!("    {}", style("(none configured)").dim());
    } else {
        // Sort by key for consistent output
        let mut entries: Vec<_> = config.model_map.iter().collect();
        entries.sort_by_key(|(k, _)| (*k).clone());

        for (claude_id, route) in entries {
            let route_type = config.get_upstream_type(&route.upstream_name);
            eprintln!(
                " {} -> {}:{} [type={}]",
                style(claude_id).dim(),
                style(&route.upstream_name).cyan(),
                style(&route.target_model).green(),
                style(route_type).yellow()
            );
        }
    }

    // WebFetch section
    eprintln!("\n  {}", style("━━━ WebFetch ━━━").yellow().bold());
    eprintln!(
        "    Enabled:         {}",
        if config.web_fetch_enabled { style("true").green() } else { style("false").dim() }
    );
    eprintln!("    Max retries:     {}", style(config.web_fetch_max_retries).dim());
    eprintln!("    Timeout:         {}s", style(config.web_fetch_timeout_secs).dim());

    eprintln!();
    Ok(())
}

fn mask_key(key: &Option<String>) -> console::StyledObject<String> {
    match key {
        Some(k) if !k.is_empty() => {
            let chars_count = k.chars().count();
            let visible = crate::str_utils::safe_truncate_from_end(k, 4);
            let masked = "*".repeat(chars_count.saturating_sub(4));
            style(format!("{}{}", masked, visible)).cyan()
        }
        _ => style("(not set)".to_string()).dim(),
    }
}

// ─── config set ───────────────────────────────────────────────────────

fn config_set(key: &str, value: &str, config_path: Option<&std::path::Path>) -> Result<()> {
    let env_path = match config_path {
        Some(p) => p.to_path_buf(),
        None => find_env_path()?,
    };

    let content = if env_path.exists() { fs::read_to_string(&env_path)? } else { String::new() };

    // Normalize key to uppercase
    let key_upper = key.to_uppercase();

    let mut lines: Vec<String> = content.lines().map(String::from).collect();
    let mut found = false;

    for line in &mut lines {
        // Match KEY= or KEY = (with optional spaces)
        let trimmed = line.trim();
        if trimmed.starts_with(&format!("{}=", key_upper))
            || trimmed.starts_with(&format!("{} =", key_upper))
        {
            *line = format!("{}={}", key_upper, value);
            found = true;
            break;
        }
    }

    if !found {
        // Append to end
        if !lines.is_empty() && !lines.last().is_none_or(|l| l.is_empty()) {
            lines.push(String::new());
        }
        lines.push(format!("{}={}", key_upper, value));
    }

    let new_content = lines.join("\n");
    fs::write(&env_path, &new_content)?;

    eprintln!(
        "{} {} = {} (in {})",
        style("✅").green(),
        style(&key_upper).bold(),
        style(value).green(),
        env_path.display()
    );

    eprintln!("{}", style("   Restart proxy or send SIGHUP for changes to take effect").dim());

    Ok(())
}

// ─── config test ──────────────────────────────────────────────────────

fn config_test(config_path: Option<PathBuf>) -> Result<()> {
    eprintln!();
    eprintln!("{}", style("━━━ Configuration Test ━━━").yellow().bold());

    let config = Config::from_env_with_path(config_path);

    // Test 1: Config loads
    let config = match config {
        Ok(c) => {
            eprintln!("  {} Configuration loaded successfully", style("✅").green());
            c
        }
        Err(e) => {
            eprintln!("  {} Configuration error: {}", style("❌").red(), e);
            return Ok(());
        }
    };

    // Test 2: Upstream connectivity
    eprintln!("  Testing upstream connectivity...");
    let client = reqwest::blocking::Client::builder().timeout(Duration::from_secs(15)).build()?;

    for (name, upstream) in &config.upstreams {
        let url = format!("{}/v1/models", upstream.base_url.trim_end_matches('/'));
        let start = Instant::now();

        let mut req = client.get(&url);
        if let Some(ref key) = upstream.api_key {
            req = req.header("Authorization", format!("Bearer {}", key));
        }

        match req.send() {
            Ok(resp) if resp.status().is_success() => {
                let ms = start.elapsed().as_millis();
                let body: serde_json::Value = resp.json().unwrap_or_default();
                let model_count =
                    body.get("data").and_then(|d| d.as_array()).map(|a| a.len()).unwrap_or(0);
                eprintln!(
                    "  {} Upstream '{}': {}ms, {} models",
                    style("✅").green(),
                    name,
                    ms,
                    model_count
                );
            }
            Ok(resp) => {
                eprintln!("  {} Upstream '{}': HTTP {}", style("❌").red(), name, resp.status());
            }
            Err(e) => {
                eprintln!("  {} Upstream '{}': {}", style("❌").red(), name, e);
            }
        }
    }

    // Test 3: CC binary scan
    eprintln!("  Scanning Claude Code binary...");
    match crate::scan::scan_cc_binary() {
        Ok(result) => {
            eprintln!(
                "  {} CC binary: {} models, {} tools",
                style("✅").green(),
                result.models.len(),
                result.tools.len()
            );
        }
        Err(e) => {
            eprintln!("  {} CC binary: {}", style("[WARN]").yellow(), e);
        }
    }

    // Test 4: Local health check
    eprintln!("  Checking local proxy health...");
    let health_url = format!("http://localhost:{}/health", config.port);
    match client.get(&health_url).timeout(Duration::from_secs(3)).send() {
        Ok(resp) if resp.status().is_success() => {
            eprintln!("  {} Proxy healthy at port {}", style("✅").green(), config.port);
        }
        Ok(resp) => {
            eprintln!("  {} Proxy responded: {}", style("[WARN]").yellow(), resp.status());
        }
        Err(_) => {
            eprintln!("  {} Proxy not running on port {}", style("ℹ️").blue(), config.port);
        }
    }

    // Test 5: Model mapping coverage
    if !config.model_map.is_empty() {
        eprintln!("  {} {} model mappings configured", style("✅").green(), config.model_map.len());
    } else {
        eprintln!("  {} No model mappings — run: nexus-ai-gateway setup", style("[WARN]").yellow());
    }

    eprintln!();
    Ok(())
}

fn find_env_path() -> Result<PathBuf> {
    let home = std::env::var("HOME").context("HOME not set")?;
    Ok(PathBuf::from(home).join(".nexus-ai-gateway.env"))
}

#[cfg(test)]
mod mask_key_tests {
    use super::*;

    #[test]
    fn test_mask_key_shows_last_four() {
        let key = Some("sk-abcdef1234567890".to_string());
        // Should not panic - that's the main test
        let _result = mask_key(&key);
    }

    #[test]
    fn test_mask_key_short_key() {
        let key = Some("abc".to_string());
        // Short key: should still work without panic
        let _result = mask_key(&key);
    }

    #[test]
    fn test_mask_key_empty() {
        let key: Option<String> = None;
        // Should show "(not set)" - should not panic
        let _result = mask_key(&key);
    }

    #[test]
    fn test_mask_key_exactly_four_chars() {
        let key = Some("1234".to_string());
        // Exactly 4 chars - last 4 should be visible
        let _result = mask_key(&key);
    }

    #[test]
    fn test_mask_key_five_chars() {
        let key = Some("12345".to_string());
        // 5 chars, 1 hidden, 4 visible
        let _result = mask_key(&key);
    }

    #[test]
    fn test_mask_key_empty_string() {
        let key = Some("".to_string());
        // Empty string should be treated as not set
        let _result = mask_key(&key);
    }
}
