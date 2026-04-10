use chrono::{DateTime, Utc};
use regex::Regex;
use sha2::{Digest, Sha256};
use std::collections::HashSet;
use std::fmt;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::Command as SysCommand;

// ============================================================
// Phase 1.2: Core data structures
// ============================================================

/// Tier classification for Claude models
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum ModelTier {
    Opus,
    Sonnet,
    Haiku,
    Instant,
    Legacy,
    Unknown,
}

impl fmt::Display for ModelTier {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ModelTier::Opus => write!(f, "Opus"),
            ModelTier::Sonnet => write!(f, "Sonnet"),
            ModelTier::Haiku => write!(f, "Haiku"),
            ModelTier::Instant => write!(f, "Instant"),
            ModelTier::Legacy => write!(f, "Legacy"),
            ModelTier::Unknown => write!(f, "Unknown"),
        }
    }
}

/// A single discovered ClaudeModelID
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct DiscoveredModel {
    pub id: String,
    pub generation: String,
    pub tier: ModelTier,
    pub date_suffix: Option<String>,
    pub version_suffix: Option<String>,
}

/// Complete result of scanning the CC binary
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct CCScanResult {
    pub binary_path: String,
    pub binary_sha256: String,
    pub models: Vec<DiscoveredModel>,
    pub tools: Vec<String>,
    pub capabilities: Vec<String>,
    pub env_vars: Vec<String>,
    pub scan_timestamp: DateTime<Utc>,
}

// ============================================================
// Phase 1.3: find_cc_binary()
// ============================================================

/// Find the Claude Code binary on the system
pub fn find_cc_binary() -> Option<PathBuf> {
    // Try `which claude` first
    if let Ok(output) = SysCommand::new("which").arg("claude").output() {
        if output.status.success() {
            let path = String::from_utf8_lossy(&output.stdout).trim().to_string();
            if !path.is_empty() {
                let p = PathBuf::from(&path);
                if p.exists() {
                    return Some(p);
                }
            }
        }
    }

    // Fallback paths
    let fallbacks = [
        dirs_next_home().map(|h| h.join(".local/bin/claude")),
        Some(PathBuf::from("/usr/local/bin/claude")),
        Some(PathBuf::from("/usr/bin/claude")),
    ];

    for fallback in fallbacks.iter().flatten() {
        if fallback.exists() {
            return Some(fallback.clone());
        }
    }

    None
}

fn dirs_next_home() -> Option<PathBuf> {
    std::env::var("HOME").ok().map(PathBuf::from)
}

// ============================================================
// Phase 1.4: compute_sha256()
// ============================================================

/// Compute SHA256 hash of a file
pub fn compute_sha256(path: &Path) -> Result<String, String> {
    let mut file =
        std::fs::File::open(path).map_err(|e| format!("Cannot open {}: {}", path.display(), e))?;

    let mut hasher = Sha256::new();
    let mut buffer = [0u8; 8192];

    loop {
        let bytes_read = file
            .read(&mut buffer)
            .map_err(|e| format!("Read error: {}", e))?;
        if bytes_read == 0 {
            break;
        }
        hasher.update(&buffer[..bytes_read]);
    }

    Ok(format!("{:x}", hasher.finalize()))
}

// ============================================================
// Phase 1.5: extract_strings()
// ============================================================

/// Extract printable strings from a binary using the `strings` command
pub fn extract_strings(path: &Path) -> Result<String, String> {
    let output = SysCommand::new("strings")
        .arg(path)
        .output()
        .map_err(|e| format!("Failed to run `strings`: {}", e))?;

    if !output.status.success() {
        return Err(format!(
            "`strings` failed: {}",
            String::from_utf8_lossy(&output.stderr)
        ));
    }

    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

// ============================================================
// Phase 1.6: extract_model_ids()
// ============================================================

/// Extract all ClaudeModelIDs from the raw strings output
pub fn extract_model_ids(raw: &str) -> Vec<DiscoveredModel> {
    let patterns = [
        r"claude-instant-[\d.]+(?:-\d+k)?",
        r"claude-[0-9]+\.[0-9]+(?:-\d+k)?",
        r"claude-3(?:-5)?-(?:haiku|sonnet|opus)(?:-\d{8})?(?:-v\d)?(?:-latest)?",
        r"claude-3-7-sonnet(?:-\d{8})?(?:-v\d)?(?:-latest)?",
        r"claude-(?:haiku|sonnet|opus)-(?:3-[57]|4(?:-[0-9])?(?:-5|-6)?)(?:-\d{8})?(?:-v\d)?",
        r"claude-4-opus-\d{8}",
    ];

    let mut all_ids: HashSet<String> = HashSet::new();

    for pattern in &patterns {
        if let Ok(re) = Regex::new(pattern) {
            for m in re.find_iter(raw) {
                let id = m.as_str().trim_end_matches('-').to_string();
                if id.len() > 8 {
                    all_ids.insert(id);
                }
            }
        }
    }

    let mut models: Vec<DiscoveredModel> = all_ids
        .into_iter()
        .map(|id| {
            let (tier, generation, date_suffix, version_suffix) = categorize_model(&id);
            DiscoveredModel {
                id,
                generation,
                tier,
                date_suffix,
                version_suffix,
            }
        })
        .collect();

    models.sort_by(|a, b| a.id.cmp(&b.id));
    models
}

// ============================================================
// Phase 1.7: categorize_model()
// ============================================================

/// Categorize a model ID into tier, generation, date suffix, and version suffix
fn categorize_model(id: &str) -> (ModelTier, String, Option<String>, Option<String>) {
    // Extract date suffix (YYYYMMDD)
    let date_re = Regex::new(r"-(\d{8})").unwrap();
    let date_suffix = date_re.captures(id).map(|c| c[1].to_string());

    // Extract version suffix (vN)
    let ver_re = Regex::new(r"-(v\d+)$").unwrap();
    let version_suffix = ver_re.captures(id).map(|c| c[1].to_string());

    // Determine tier
    let tier = if id.contains("instant") {
        ModelTier::Instant
    } else if id.starts_with("claude-1") || id.starts_with("claude-2") {
        ModelTier::Legacy
    } else if id.contains("opus") {
        ModelTier::Opus
    } else if id.contains("sonnet") {
        ModelTier::Sonnet
    } else if id.contains("haiku") {
        ModelTier::Haiku
    } else {
        ModelTier::Unknown
    };

    // Determine generation
    let generation =
        if id.contains("instant") || id.starts_with("claude-1") || id.starts_with("claude-2") {
            "legacy".to_string()
        } else if id.contains("3-5") || id.contains("haiku-3-5") {
            "3.5".to_string()
        } else if id.contains("3-7") || id.contains("sonnet-3-7") {
            "3.7".to_string()
        } else if id.contains("claude-3-") && !id.contains("3-5") && !id.contains("3-7") {
            "3".to_string()
        } else if id.contains("4-5") || id.contains("4.5") {
            "4.5".to_string()
        } else if id.contains("4-6") || id.contains("4.6") {
            "4.6".to_string()
        } else if id.contains("4-1") {
            "4.1".to_string()
        } else if id.contains("4-2") {
            "4.2".to_string()
        } else if id.contains("-4") {
            "4".to_string()
        } else {
            "unknown".to_string()
        };

    (tier, generation, date_suffix, version_suffix)
}

// ============================================================
// Phase 1.8: extract_tools()
// ============================================================

/// Extract native CC tool names from binary strings
pub fn extract_tools(raw: &str) -> Vec<String> {
    let known_tools = [
        "Bash",
        "Read",
        "Write",
        "Edit",
        "Glob",
        "Grep",
        "WebSearch",
        "WebFetch",
        "Agent",
        "AskUserQuestion",
        "Skill",
        "NotebookEdit",
        "TaskCreate",
        "TaskUpdate",
        "TaskList",
        "TaskGet",
        "TodoWrite",
        "WorktreeCreate",
        "MultiEdit",
        "Advisor",
    ];

    let mut found: Vec<String> = Vec::new();

    for tool in &known_tools {
        // Search for the tool name as a quoted string
        let pattern = format!("\"{}\"", tool);
        if raw.contains(&pattern) {
            found.push(tool.to_string());
        }
    }

    found.sort();
    found.dedup();
    found
}

// ============================================================
// Phase 1.9: extract_capabilities()
// ============================================================

/// Extract supported capabilities from binary strings
pub fn extract_capabilities(raw: &str) -> Vec<String> {
    let known_caps = [
        "interleaved_thinking",
        "tool_use",
        "tool_result",
        "vision",
        "images",
        "pdf",
        "citations",
        "computer_use",
        "web_search",
        "batch",
        "prompt_caching",
        "streaming",
        "max_output",
        "planning",
        "code_execution",
    ];

    let mut found: Vec<String> = Vec::new();

    for cap in &known_caps {
        if raw.contains(cap) {
            found.push(cap.to_string());
        }
    }

    found
}

// ============================================================
// Phase 1.10: extract_env_vars()
// ============================================================

/// Extract relevant environment variables from binary strings
pub fn extract_env_vars(raw: &str) -> Vec<String> {
    let relevant_prefixes = [
        "ANTHROPIC_BASE_URL",
        "ANTHROPIC_API_KEY",
        "ANTHROPIC_AUTH_TOKEN",
        "ANTHROPIC_MODEL",
        "ANTHROPIC_CUSTOM_MODEL_OPTION",
        "ANTHROPIC_CUSTOM_MODEL_OPTION_NAME",
        "ANTHROPIC_CUSTOM_MODEL_OPTION_DESCRIPTION",
        "ANTHROPIC_DEFAULT_OPUS_MODEL",
        "ANTHROPIC_DEFAULT_OPUS_MODEL_NAME",
        "ANTHROPIC_DEFAULT_OPUS_MODEL_DESCRIPTION",
        "ANTHROPIC_DEFAULT_OPUS_MODEL_SUPPORTED_CAPABILITIES",
        "ANTHROPIC_DEFAULT_SONNET_MODEL",
        "ANTHROPIC_DEFAULT_SONNET_MODEL_NAME",
        "ANTHROPIC_DEFAULT_SONNET_MODEL_DESCRIPTION",
        "ANTHROPIC_DEFAULT_SONNET_MODEL_SUPPORTED_CAPABILITIES",
        "ANTHROPIC_DEFAULT_HAIKU_MODEL",
        "ANTHROPIC_DEFAULT_HAIKU_MODEL_NAME",
        "ANTHROPIC_DEFAULT_HAIKU_MODEL_DESCRIPTION",
        "ANTHROPIC_DEFAULT_HAIKU_MODEL_SUPPORTED_CAPABILITIES",
        "ANTHROPIC_SMALL_FAST_MODEL",
        "CLAUDE_CODE_SUBAGENT_MODEL",
        "CLAUDE_CODE_DISABLE_LEGACY_MODEL_REMAP",
        "CLAUDE_CODE_DISABLE_THINKING",
        "CLAUDE_CODE_MAX_OUTPUT_TOKENS",
        "CLAUDE_CODE_AUTO_COMPACT_WINDOW",
    ];

    let mut found: Vec<String> = Vec::new();

    for var in &relevant_prefixes {
        if raw.contains(var) {
            found.push(var.to_string());
        }
    }

    found
}

// ============================================================
// Phase 1.11: scan_cc_binary() — orchestrator
// ============================================================

/// Perform a complete scan of the Claude Code binary
pub fn scan_cc_binary() -> Result<CCScanResult, String> {
    let binary_path = find_cc_binary()
        .ok_or_else(|| "Claude Code binary not found. Is `claude` in PATH?".to_string())?;

    tracing::info!("🔍 Scanning CC binary: {}", binary_path.display());

    let sha256 = compute_sha256(&binary_path)?;
    tracing::info!(
        "📊 SHA256: {}...{}",
        &sha256[..8],
        &sha256[sha256.len() - 8..]
    );

    let raw = extract_strings(&binary_path)?;
    tracing::info!("📊 Extracted {} chars from binary", raw.len());

    let models = extract_model_ids(&raw);
    let tools = extract_tools(&raw);
    let capabilities = extract_capabilities(&raw);
    let env_vars = extract_env_vars(&raw);

    tracing::info!(
        "📊 Discovered: {} ClaudeModelIDs, {} tools, {} capabilities, {} env vars",
        models.len(),
        tools.len(),
        capabilities.len(),
        env_vars.len()
    );

    Ok(CCScanResult {
        binary_path: binary_path.to_string_lossy().to_string(),
        binary_sha256: sha256,
        models,
        tools,
        capabilities,
        env_vars,
        scan_timestamp: Utc::now(),
    })
}

// ============================================================
// Phase 1.12: display_scan()
// ============================================================

/// Display scan results in a formatted way
pub fn display_scan(result: &CCScanResult) {
    println!("╔══════════════════════════════════════════════════╗");
    println!("║  🔬 Claude Code Binary Scan Results             ║");
    println!("╠══════════════════════════════════════════════════╣");
    println!("║ Binary: {}", result.binary_path);
    println!("║ SHA256: {}", result.binary_sha256);
    println!(
        "║ Scanned: {}",
        result.scan_timestamp.format("%Y-%m-%d %H:%M:%S UTC")
    );
    println!("╠══════════════════════════════════════════════════╣");

    // Group models by tier
    let tiers = [
        (ModelTier::Opus, "Opus"),
        (ModelTier::Sonnet, "Sonnet"),
        (ModelTier::Haiku, "Haiku"),
        (ModelTier::Instant, "Instant"),
        (ModelTier::Legacy, "Legacy"),
    ];

    println!("║ 📦 ClaudeModelIDs: {} total", result.models.len());
    for (tier, label) in &tiers {
        let models: Vec<&DiscoveredModel> =
            result.models.iter().filter(|m| m.tier == *tier).collect();
        if !models.is_empty() {
            println!("║   ── {} ({}) ──", label, models.len());
            for m in &models {
                println!("║     {}", m.id);
            }
        }
    }

    println!("╠══════════════════════════════════════════════════╣");
    println!("║ 🔧 Tools: {} found", result.tools.len());
    for tool in &result.tools {
        println!("║   ✓ {}", tool);
    }

    println!("╠══════════════════════════════════════════════════╣");
    println!("║ ⚡ Capabilities: {} found", result.capabilities.len());
    for cap in &result.capabilities {
        println!("║   ✓ {}", cap);
    }

    println!("╠══════════════════════════════════════════════════╣");
    println!(
        "║ 🌐 Env Vars: {} confirmed in binary",
        result.env_vars.len()
    );
    for var in &result.env_vars {
        println!("║   ✓ {}", var);
    }

    println!("╚══════════════════════════════════════════════════╝");
}

// ============================================================
// Phase 1.13: generate_env_template()
// ============================================================

/// Generate a .env template with MODEL_MAP entries for all discovered models
pub fn generate_env_template(scan: &CCScanResult) -> String {
    let mut output = String::new();

    output.push_str("# ═══════════════════════════════════════════════════\n");
    output.push_str("# nexus-ai-gateway-rs — Auto-generated Model Mapping\n");
    output.push_str(&format!(
        "# Scanned: {} | Models: {}\n",
        scan.scan_timestamp.format("%Y-%m-%d"),
        scan.models.len()
    ));
    output.push_str(&format!(
        "# CC Binary SHA256: {}...{}\n",
        &scan.binary_sha256[..16],
        &scan.binary_sha256[scan.binary_sha256.len() - 8..]
    ));
    output.push_str("# ═══════════════════════════════════════════════════\n\n");

    output.push_str("# Core connection\n");
    output.push_str("PORT = 8315\n");
    output.push_str("UPSTREAM_BASE_URL=https://integrate.api.nvidia.com\n");
    output.push_str("UPSTREAM_API_KEY=<YOUR_NIM_API_KEY>\n\n");

    output.push_str("# ─── Model Mapping Table ───\n");
    output.push_str("# Format: MODEL_MAP_<claude_id_with_underscores>=<upstream>:<nim_model>\n");
    output.push_str("# Replace <UNASSIGNED> with your NIM model ID\n\n");

    // Group by tier for readability
    let tiers = [
        (ModelTier::Opus, "Opus tier (Boss/Primary)"),
        (ModelTier::Sonnet, "Sonnet tier (Workhorse)"),
        (ModelTier::Haiku, "Haiku tier (Fast/Cheap)"),
        (ModelTier::Instant, "Instant tier (Legacy)"),
        (ModelTier::Legacy, "Legacy tier (v1/v2)"),
    ];

    for (tier, label) in &tiers {
        let models: Vec<&DiscoveredModel> =
            scan.models.iter().filter(|m| m.tier == *tier).collect();
        if !models.is_empty() {
            output.push_str(&format!("# --- {} ---\n", label));
            for m in &models {
                let env_key = m.id.replace('-', "_");
                output.push_str(&format!("# MODEL_MAP_{}=default:<UNASSIGNED>\n", env_key));
            }
            output.push('\n');
        }
    }

    output
}

// ============================================================
// Phase 1.14: generate_launcher_script()
// ============================================================

/// Generate a launcher script with symbiont env vars
pub fn generate_launcher_script(scan: &CCScanResult) -> String {
    let mut output = String::new();

    output.push_str("#!/bin/bash\n");
    output.push_str("# ═══════════════════════════════════════════════════\n");
    output.push_str("# Claude Code Launcher — nexus-ai-gateway-rs Symbiont\n");
    output.push_str(&format!(
        "# Auto-generated: {} | Models: {}\n",
        scan.scan_timestamp.format("%Y-%m-%d"),
        scan.models.len()
    ));
    output.push_str("# ═══════════════════════════════════════════════════\n\n");

    output.push_str("# Core connection — point CC to our proxy\n");
    output.push_str("export ANTHROPIC_BASE_URL=\"http://localhost:8315\"\n");
    output.push_str("export ANTHROPIC_API_KEY=\"proxy-key\"\n\n");

    output.push_str("# Model routing per tier\n");
    output.push_str("# Opus = Boss (most capable)\n");
    output.push_str("export ANTHROPIC_DEFAULT_OPUS_MODEL=\"claude-opus-4-6\"\n");
    output.push_str("export ANTHROPIC_DEFAULT_OPUS_MODEL_NAME=\"Opus 4.6 via NIM\"\n");
    output.push_str(
        "export ANTHROPIC_DEFAULT_OPUS_MODEL_DESCRIPTION=\"Most capable — routed through proxy\"\n",
    );
    output.push_str("export ANTHROPIC_DEFAULT_OPUS_MODEL_SUPPORTED_CAPABILITIES=\"tool_use,streaming,vision,max_output\"\n\n");

    output.push_str("# Sonnet = Workhorse\n");
    output.push_str("export ANTHROPIC_DEFAULT_SONNET_MODEL=\"claude-sonnet-4-6\"\n");
    output.push_str("export ANTHROPIC_DEFAULT_SONNET_MODEL_NAME=\"Sonnet 4.6 via NIM\"\n");
    output.push_str("export ANTHROPIC_DEFAULT_SONNET_MODEL_DESCRIPTION=\"Fast and capable — routed through proxy\"\n");
    output.push_str("export ANTHROPIC_DEFAULT_SONNET_MODEL_SUPPORTED_CAPABILITIES=\"tool_use,streaming,vision,max_output\"\n\n");

    output.push_str("# Haiku = Fast\n");
    output.push_str("export ANTHROPIC_DEFAULT_HAIKU_MODEL=\"claude-haiku-4-5\"\n");
    output.push_str("export ANTHROPIC_DEFAULT_HAIKU_MODEL_NAME=\"Haiku 4.5 via NIM\"\n");
    output.push_str(
        "export ANTHROPIC_DEFAULT_HAIKU_MODEL_DESCRIPTION=\"Fastest — routed through proxy\"\n",
    );
    output.push_str(
        "export ANTHROPIC_DEFAULT_HAIKU_MODEL_SUPPORTED_CAPABILITIES=\"tool_use,streaming\"\n\n",
    );

    output.push_str("# Behavior\n");
    output.push_str("export CLAUDE_CODE_DISABLE_LEGACY_MODEL_REMAP=true\n");
    output.push_str("export CLAUDE_CODE_MAX_OUTPUT_TOKENS=16384\n\n");

    output.push_str("# Launch Claude Code\n");
    output.push_str("exec claude \"$@\"\n");

    output
}
