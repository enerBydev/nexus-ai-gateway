use anyhow::Result;
use std::collections::HashMap;
use std::sync::Arc;
use std::{env, path::PathBuf};

/// Upstream API type — determines protocol behavior
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(clippy::upper_case_acronyms)]
pub enum UpstreamType {
    Anthropic,
    NIM,
    OpenAI,
    OpenRouter,
}

impl std::fmt::Display for UpstreamType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            UpstreamType::Anthropic => write!(f, "anthropic"),
            UpstreamType::NIM => write!(f, "nim"),
            UpstreamType::OpenAI => write!(f, "openai"),
            UpstreamType::OpenRouter => write!(f, "openrouter"),
        }
    }
}

impl std::str::FromStr for UpstreamType {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "anthropic" => Ok(UpstreamType::Anthropic),
            "nim" => Ok(UpstreamType::NIM),
            "openai" => Ok(UpstreamType::OpenAI),
            "openrouter" => Ok(UpstreamType::OpenRouter),
            other => Err(format!("unknown upstream type: {}", other)),
        }
    }
}

#[derive(Debug, Clone)]
pub struct UpstreamConfig {
    pub base_url: String,
    pub api_key: Option<String>,
    /// Per-upstream type override. If None, falls back to global `Config.upstream_type`.
    /// User Decision (Q3, Option A): upstream_type belongs in UpstreamConfig,
    /// NOT in ModelRoute, because the type is a property of the endpoint, not the route.
    pub upstream_type: Option<UpstreamType>,
}

#[derive(Debug, Clone)]
pub struct ModelRoute {
    pub upstream_name: String,
    pub target_model: String,
}

#[derive(Debug, Clone)]
pub struct Config {
    pub port: u16,
    /// Listener bind address (Issue #78). Default `127.0.0.1` (loopback-only).
    /// Set `BIND_ADDR=0.0.0.0` (or `--bind 0.0.0.0`) to expose on all interfaces — opt-in.
    pub bind_addr: String,
    pub base_url: String,
    pub api_key: Option<String>,
    pub reasoning_model: Option<String>,
    pub completion_model: Option<String>,
    pub debug: bool,
    pub verbose: bool,
    pub web_fetch_enabled: bool,
    pub web_fetch_max_retries: u32,
    pub web_fetch_timeout_secs: u64,
    // Multi-upstream support
    pub upstreams: HashMap<String, UpstreamConfig>,
    pub model_map: HashMap<String, ModelRoute>,
    // Concurrency tuning (Opción B: read from .env)
    pub max_concurrent_per_model: usize,
    pub permit_timeout_secs: u64,
    pub upstream_type: UpstreamType,
    // Prompt cache configuration (for self-hosted NIM with KV_CACHE_REUSE=1)
    /// Tracking: Future integration for prompt caching (PHASE 3.5)
    #[allow(dead_code)]
    pub prompt_cache_enabled: bool,
    /// Tracking: Future integration for prompt caching (PHASE 3.5)
    #[allow(dead_code)]
    pub prompt_cache_max_entries: usize,
    /// Tracking: Future integration for prompt caching (PHASE 3.5)
    #[allow(dead_code)]
    pub prompt_cache_ttl_secs: u64,
    // Circuit breaker configuration (v0.14.1)
    pub cb_enabled: bool,
    pub cb_threshold: u32,
    pub cb_recovery_secs: u64,
    // Dynamic context window mapping (Issue #28)
    /// Per-model CC context window overrides. Key: claude model ID, Value: token count.
    /// Populated from CC_MODEL_CONTEXT_WINDOWS env var.
    /// Format: "claude-opus-4-6:200000,claude-sonnet-4-6:200000,claude-haiku-4-5:200000"
    pub cc_model_context_windows: HashMap<String, u32>,
    // Telemetry configuration (v0.18.0)
    pub telemetry_enabled: bool,
    pub telemetry_beacon_url: Option<String>,
    pub beacon_auth_token: Option<String>,
    #[allow(dead_code)]
    // Used internally to build telemetry_db_path/secret_path; not read at runtime
    pub telemetry_dir: String,
    pub telemetry_db_path: String,
    pub telemetry_retention_days: u32,
    pub telemetry_secret_path: String,

    /// If telemetry was explicitly disabled by the user, None.
    /// If telemetry was auto-disabled (e.g., $HOME guard), contains the reason.
    pub telemetry_disabled_reason: Option<String>,
    /// Path to custom config file (--config flag)
    /// Stored for hot-reload support (SIGHUP + file watcher)
    pub config_path: Option<PathBuf>,
}

impl Config {
    /// Return the ordered list of env file paths to load
    fn env_file_paths(custom_path: Option<PathBuf>) -> Vec<PathBuf> {
        let mut paths = Vec::new();

        if let Some(path) = custom_path {
            paths.push(path);
        }

        paths.push(PathBuf::from("./.env"));

        if let Ok(home) = env::var("HOME") {
            paths.push(PathBuf::from(home).join(".nexus-ai-gateway.env"));
        }

        paths.push(PathBuf::from("/etc/nexus-ai-gateway/.env"));

        paths
    }

    /// Load environment variables into a HashMap from process env and .env files
    /// Values from later files override earlier ones
    fn load_env_to_map(custom_path: Option<PathBuf>) -> Result<HashMap<String, String>> {
        let mut map: HashMap<String, String> = env::vars().collect();

        for path in Self::env_file_paths(custom_path) {
            if path.exists() {
                if let Ok(iter) = dotenvy::from_path_iter(&path) {
                    for item in iter.flatten() {
                        map.insert(item.0, item.1);
                    }
                }
            }
        }

        Ok(map)
    }

    /// Helper to get a value from the env map
    fn get_from_map(map: &HashMap<String, String>, key: &str) -> Option<String> {
        map.get(key).cloned()
    }

    /// Resolve a secret using the `*_FILE` convention (Issue #115).
    /// Precedence: a non-empty `direct` value > the trimmed contents of the file at
    /// `file_path` > `None`. Bridges Docker/Kubernetes secrets, systemd `LoadCredential`
    /// and Vault Agent, which all expose secrets as files.
    ///
    /// Empty files and read errors warn and fall through (never abort — another source
    /// may cover). Uses `eprintln!` because config is built before the tracing subscriber
    /// exists (lesson from #78), and never logs the secret itself — only the path.
    fn resolve_secret(direct: Option<String>, file_path: Option<String>) -> Option<String> {
        if let Some(v) = direct.filter(|s| !s.is_empty()) {
            if file_path.as_deref().is_some_and(|p| !p.trim().is_empty()) {
                eprintln!(
                    "[WARN] both a direct secret and its *_FILE were set; using the direct value"
                );
            }
            return Some(v);
        }
        let path = file_path.filter(|p| !p.trim().is_empty())?;
        match std::fs::read_to_string(&path) {
            Ok(content) => {
                let secret = content.trim().to_string();
                if secret.is_empty() {
                    eprintln!("[WARN] secret file '{path}' is empty; ignoring");
                    None
                } else {
                    Some(secret)
                }
            }
            Err(e) => {
                eprintln!("[WARN] could not read secret file '{path}': {e}");
                None
            }
        }
    }

    /// Validate and normalize a bind address (Issue #78).
    /// Parses the value as an `IpAddr`; on failure warns and falls back to the safe
    /// loopback default `127.0.0.1` (parse, don't validate).
    /// Uses `eprintln!` (not `tracing`) because this runs during config construction,
    /// before the tracing subscriber is initialized — otherwise the warning is lost.
    pub(crate) fn normalize_bind_addr(raw: &str) -> String {
        let trimmed = raw.trim();
        match trimmed.parse::<std::net::IpAddr>() {
            Ok(_) => trimmed.to_string(),
            Err(_) => {
                eprintln!(
                    "[WARN] Invalid BIND_ADDR='{}' is not a valid IP address; falling back to 127.0.0.1 (loopback-only).",
                    trimmed
                );
                "127.0.0.1".to_string()
            }
        }
    }

    /// Build the listener `SocketAddr` from a bind address string and port (Issue #78).
    /// Uses typed `SocketAddr::new` so IPv6 addresses render correctly (e.g. `[::1]:8315`),
    /// which plain `format!("{addr}:{port}")` cannot do (it would yield `::1:8315`).
    pub(crate) fn resolve_socket_addr(bind_addr: &str, port: u16) -> Result<std::net::SocketAddr> {
        let ip = bind_addr
            .parse::<std::net::IpAddr>()
            .map_err(|e| anyhow::anyhow!("Invalid bind address '{}': {}", bind_addr, e))?;
        Ok(std::net::SocketAddr::new(ip, port))
    }

    /// Resolve the listener bind address (Issue #78).
    /// Precedence: `BIND_ADDR` > safe default `127.0.0.1`.
    /// The legacy `HOST` variable is deprecated and intentionally ignored; if it is
    /// present without `BIND_ADDR`, warn the operator (it never controlled the bind).
    fn resolve_bind_addr(bind_addr_raw: Option<String>, host_raw: Option<String>) -> String {
        if bind_addr_raw.is_none() && host_raw.is_some() {
            // eprintln! (not tracing): emitted during config construction, before the
            // tracing subscriber exists. OSS users migrating with HOST set must see this.
            eprintln!(
                "[WARN] HOST is deprecated and ignored; it never controlled the listener. \
                 Use BIND_ADDR instead (defaulting to 127.0.0.1). \
                 Set BIND_ADDR=0.0.0.0 to expose on all interfaces."
            );
        }
        match bind_addr_raw {
            Some(raw) => Self::normalize_bind_addr(&raw),
            None => "127.0.0.1".to_string(),
        }
    }

    /// Parse CC_MODEL_CONTEXT_WINDOWS env var into a HashMap.
    /// Format: "claude-opus-4-6:200000,claude-sonnet-4-6:200000"
    fn parse_model_context_windows(value: &str) -> HashMap<String, u32> {
        let mut map = HashMap::new();
        for entry in value.split(',') {
            let entry = entry.trim();
            if entry.is_empty() {
                continue;
            }
            if let Some((model, limit_str)) = entry.split_once(':') {
                match limit_str.trim().parse::<u32>() {
                    Ok(limit) if limit > 0 => {
                        map.insert(model.trim().to_string(), limit);
                    }
                    Ok(_) => tracing::warn!(
                        "CC_MODEL_CONTEXT_WINDOWS: ignoring non-positive limit for '{}'",
                        model.trim()
                    ),
                    Err(e) => tracing::warn!(
                        "CC_MODEL_CONTEXT_WINDOWS: invalid number '{}' for '{}': {}",
                        limit_str.trim(),
                        model.trim(),
                        e
                    ),
                }
            } else {
                tracing::warn!(
                    "CC_MODEL_CONTEXT_WINDOWS: malformed entry '{}' (expected 'model:tokens')",
                    entry
                );
            }
        }
        map
    }

    /// Create Config from a HashMap (used for thread-safe reload)
    fn from_map(data: &HashMap<String, String>) -> Result<Self> {
        let port = Self::get_from_map(data, "PORT").and_then(|p| p.parse().ok()).unwrap_or(8315);

        // Issue #78: configurable bind address, default 127.0.0.1 (loopback-only).
        let bind_addr = Self::resolve_bind_addr(
            Self::get_from_map(data, "BIND_ADDR"),
            Self::get_from_map(data, "HOST"),
        );

        let base_url = Self::get_from_map(data, "UPSTREAM_BASE_URL").ok_or_else(|| {
            anyhow::anyhow!(
                "UPSTREAM_BASE_URL is required. Set it to your OpenAI-compatible endpoint.\n\
                 Examples:\n\
                 - OpenRouter: https://openrouter.ai/api\n\
                 - OpenAI: https://api.openai.com\n\
                 - Local: http://localhost:11434"
            )
        })?;

        // Issue #115: support the *_FILE convention (direct value > _FILE > none).
        let api_key = Self::resolve_secret(
            Self::get_from_map(data, "UPSTREAM_API_KEY"),
            Self::get_from_map(data, "UPSTREAM_API_KEY_FILE"),
        )
        .or_else(|| {
            Self::resolve_secret(
                Self::get_from_map(data, "OPENROUTER_API_KEY"),
                Self::get_from_map(data, "OPENROUTER_API_KEY_FILE"),
            )
        });

        let reasoning_model = Self::get_from_map(data, "REASONING_MODEL");
        let completion_model = Self::get_from_map(data, "COMPLETION_MODEL");

        let debug = Self::get_from_map(data, "DEBUG")
            .map(|v| v == "1" || v.to_lowercase() == "true")
            .unwrap_or(false);

        let verbose = Self::get_from_map(data, "VERBOSE")
            .map(|v| v == "1" || v.to_lowercase() == "true")
            .unwrap_or(false);

        if base_url.ends_with("/v1") {
            eprintln!("[WARN] WARNING: UPSTREAM_BASE_URL ends with '/v1'");
            eprintln!("   This will result in URLs like: {}/v1/chat/completions", base_url);
            eprintln!("   Consider removing '/v1' from UPSTREAM_BASE_URL");
            eprintln!("   Correct: https://openrouter.ai/api");
            eprintln!("   Wrong:   https://openrouter.ai/api/v1");
        }

        let web_fetch_enabled = Self::get_from_map(data, "WEB_FETCH_ENABLED")
            .map(|v| v != "0" && v.to_lowercase() != "false")
            .unwrap_or(true);

        let web_fetch_max_retries = Self::get_from_map(data, "WEB_FETCH_MAX_RETRIES")
            .and_then(|v| v.parse().ok())
            .unwrap_or(3);

        let web_fetch_timeout_secs = Self::get_from_map(data, "WEB_FETCH_TIMEOUT_SECS")
            .and_then(|v| v.parse().ok())
            .unwrap_or(15);

        // Multi-upstream configuration
        let mut upstreams = HashMap::new();
        upstreams.insert(
            "default".to_string(),
            UpstreamConfig {
                base_url: base_url.clone(),
                api_key: api_key.clone(),
                upstream_type: None,
            },
        );

        if let Some(bm_url) = Self::get_from_map(data, "UPSTREAM_BIGMODEL_BASE_URL") {
            let bm_type = Self::get_from_map(data, "UPSTREAM_BIGMODEL_TYPE")
                .and_then(|v| v.parse::<UpstreamType>().ok());
            upstreams.insert(
                "bigmodel".to_string(),
                UpstreamConfig {
                    base_url: bm_url,
                    api_key: Self::resolve_secret(
                        Self::get_from_map(data, "UPSTREAM_BIGMODEL_API_KEY"),
                        Self::get_from_map(data, "UPSTREAM_BIGMODEL_API_KEY_FILE"),
                    ),
                    upstream_type: bm_type,
                },
            );
            eprintln!(
                " ✅ BigModel upstream configured [type={}]",
                bm_type.map(|t| t.to_string()).unwrap_or_else(|| "global".to_string())
            );
        }

        if let Some(cf_url) = Self::get_from_map(data, "UPSTREAM_CF_BASE_URL") {
            let cf_type = Self::get_from_map(data, "UPSTREAM_CF_TYPE")
                .and_then(|v| v.parse::<UpstreamType>().ok());
            upstreams.insert(
                "cf".to_string(),
                UpstreamConfig {
                    base_url: cf_url,
                    api_key: Self::resolve_secret(
                        Self::get_from_map(data, "UPSTREAM_CF_API_KEY"),
                        Self::get_from_map(data, "UPSTREAM_CF_API_KEY_FILE"),
                    ),
                    upstream_type: cf_type,
                },
            );
            eprintln!(
                " ✅ Cloudflare upstream configured [type={}]",
                cf_type.map(|t| t.to_string()).unwrap_or_else(|| "global".to_string())
            );
        }

        // Issue #35 F3: Generalized per-upstream type scanning
        // After constructing upstreams, scan for UPSTREAM_<NAME>_TYPE for ANY named upstream
        // This captures custom upstreams beyond bigmodel/cf
        for (name, upstream) in upstreams.iter_mut() {
            if name == "default" {
                continue; // Default always inherits global type
            }
            let type_key = format!("UPSTREAM_{}_TYPE", name.to_uppercase());
            if upstream.upstream_type.is_none() {
                // Only set if not already set by explicit parsing (bigmodel/cf blocks)
                if let Some(type_val) = Self::get_from_map(data, &type_key) {
                    if let Ok(t) = type_val.parse::<UpstreamType>() {
                        upstream.upstream_type = Some(t);
                        tracing::info!(
                            "[PIN] Upstream '{}' type set to {} via {}",
                            name,
                            t,
                            type_key
                        );
                    } else {
                        tracing::warn!("[WARN] Invalid {}: '{}'", type_key, type_val);
                    }
                }
            }
        }

        // Model Mapping Table from env vars
        let mut model_map = HashMap::new();
        for (key, value) in data {
            if let Some(model_id_raw) = key.strip_prefix("MODEL_MAP_") {
                let model_id = model_id_raw.replace('_', "-").to_lowercase();
                if let Some((upstream, target)) = value.split_once(':') {
                    model_map.insert(
                        model_id.clone(),
                        ModelRoute {
                            upstream_name: upstream.to_string(),
                            target_model: target.to_string(),
                        },
                    );
                    eprintln!(" [PIN] Model map: {} -> {}:{}", model_id, upstream, target);
                }
            }
        }

        eprintln!(" 📊 Upstreams: {}, Model mappings: {}", upstreams.len(), model_map.len());

        let max_concurrent_per_model = Self::get_from_map(data, "MAX_CONCURRENT_PER_MODEL")
            .and_then(|v| v.parse().ok())
            .unwrap_or(5);

        let permit_timeout_secs = Self::get_from_map(data, "PERMIT_TIMEOUT_SECS")
            .and_then(|v| v.parse().ok())
            .unwrap_or(180);

        let upstream_type = match Self::get_from_map(data, "NEXUS_UPSTREAM_TYPE") {
            Some(val) => match val.parse::<UpstreamType>() {
                Ok(t) => t,
                Err(_) => {
                    tracing::warn!(
                        "Invalid NEXUS_UPSTREAM_TYPE='{}' — valid values are: anthropic, nim, openai, openrouter. Defaulting to nim.",
                        val
                    );
                    UpstreamType::NIM
                }
            },
            None => UpstreamType::NIM,
        };

        let prompt_cache_enabled = Self::get_from_map(data, "NIM_PROMPT_CACHE_ENABLED")
            .map(|v| !v.is_empty() && v != "0" && v.to_lowercase() != "false")
            .unwrap_or(false);

        let prompt_cache_max_entries = Self::get_from_map(data, "NIM_PROMPT_CACHE_MAX_ENTRIES")
            .and_then(|v| v.parse().ok())
            .unwrap_or(1000);

        let prompt_cache_ttl_secs = Self::get_from_map(data, "NIM_PROMPT_CACHE_TTL_SECS")
            .and_then(|v| v.parse().ok())
            .unwrap_or(300);

        // Circuit breaker configuration (v0.14.1)
        let cb_enabled = Self::get_from_map(data, "CB_ENABLED")
            .map(|v| v == "1" || v.to_lowercase() == "true")
            .unwrap_or(false);
        let cb_threshold = Self::get_from_map(data, "CB_THRESHOLD")
            .and_then(|v| v.parse().ok())
            .unwrap_or(10)
            .max(1);
        let cb_recovery_secs = Self::get_from_map(data, "CB_RECOVERY_SECS")
            .and_then(|v| v.parse().ok())
            .unwrap_or(60)
            .max(1);

        // Dynamic context window mapping (Issue #28)
        let cc_model_context_windows = Self::get_from_map(data, "CC_MODEL_CONTEXT_WINDOWS")
            .map(|v| Self::parse_model_context_windows(&v))
            .unwrap_or_default();

        // Telemetry configuration (v0.18.0)
        // v0.19.0: Default ON — telemetry is always active from minute one.
        // Users can disable with TELEMETRY_ENABLED=false
        let telemetry_enabled = Self::get_from_map(data, "TELEMETRY_ENABLED")
            .map(|v| v == "1" || v.to_lowercase() == "true")
            .unwrap_or(true);

        // v0.19.0: Beacon URL obfuscated via obfstr — not visible in binary strings
        // v0.19.0: Beacon URL is hardcoded by default — users don't need to configure it.
        // Can be overridden or cleared with TELEMETRY_BEACON_URL="" to disable beacon.
        let telemetry_beacon_url = Self::get_from_map(data, "TELEMETRY_BEACON_URL")
            .filter(|u| !u.is_empty())
            .or_else(|| {
                Some(
                    obfstr::obfstr!(
                        "https://nexus-beacon-receiver.enerby212.workers.dev/v1/beacon"
                    )
                    .to_string(),
                )
            });

        // v0.19.0: Auth token obfuscated via obfstr — not visible in binary strings
        // v0.19.0: Auth token is compiled into the binary — users don't need to configure it.
        let beacon_auth_token = Self::get_from_map(data, "BEACON_AUTH_TOKEN").or_else(|| {
            Some(
                obfstr::obfstr!("e5595631b251830324175922cf5a75740aa03c0616d57226050c15629051b9d2")
                    .to_string(),
            )
        });

        // CR fix: Never fall back to /tmp for secrets — world-readable + cleared on reboot.
        // If HOME is unset AND no explicit paths provided, disable telemetry.
        let explicit_telemetry_dir = Self::get_from_map(data, "TELEMETRY_DIR");
        let explicit_secret_path = Self::get_from_map(data, "TELEMETRY_SECRET_PATH");

        let home_dir = match std::env::var("HOME") {
            Ok(h) if !h.is_empty() => h,
            _ => String::new(),
        };

        let telemetry_dir = explicit_telemetry_dir
            .clone()
            .unwrap_or_else(|| format!("{home_dir}/.local/share/nexus-ai-gateway"));
        let telemetry_db_path = Self::get_from_map(data, "TELEMETRY_DB_PATH")
            .unwrap_or_else(|| format!("{telemetry_dir}/telemetry.db"));

        // Only force-disable telemetry when HOME is unset AND no explicit paths given
        // (systemd/containers commonly set TELEMETRY_DIR explicitly)
        let (telemetry_enabled, telemetry_disabled_reason) = if home_dir.is_empty()
            && explicit_telemetry_dir.is_none()
            && explicit_secret_path.is_none()
        {
            if telemetry_enabled {
                (false, Some("$HOME not set and no explicit TELEMETRY_DIR/TELEMETRY_SECRET_PATH — cannot safely store secret".to_string()))
            } else {
                (false, None)
            }
        } else {
            (telemetry_enabled, None)
        };

        let telemetry_retention_days = Self::get_from_map(data, "TELEMETRY_RETENTION_DAYS")
            .and_then(|v| v.parse().ok())
            .unwrap_or(30)
            .max(1);

        let telemetry_secret_path =
            explicit_secret_path.unwrap_or_else(|| format!("{telemetry_dir}/.telemetry_secret"));

        // Issue #35 Bug F: Validate model_map routes against configured upstreams
        for (model_id, route) in &model_map {
            if route.upstream_name != "default" {
                if let Some(upstream) = upstreams.get(&route.upstream_name) {
                    if upstream.upstream_type.is_none() {
                        tracing::warn!(
                        "[WARN] Model '{}' routes to upstream '{}' but no UPSTREAM_{}_TYPE configured — using global type '{}'",
                        model_id, route.upstream_name, route.upstream_name.to_uppercase(), upstream_type
                    );
                    }
                } else {
                    tracing::warn!(
                    "[WARN] Model '{}' routes to upstream '{}' which is not configured — will fall back to 'default'",
                    model_id, route.upstream_name
                );
                }
            }
        }

        Ok(Config {
            port,
            bind_addr,
            base_url,
            api_key,
            reasoning_model,
            completion_model,
            debug,
            verbose,
            web_fetch_enabled,
            web_fetch_max_retries,
            web_fetch_timeout_secs,
            upstreams,
            model_map,
            max_concurrent_per_model,
            permit_timeout_secs,
            upstream_type,
            prompt_cache_enabled,
            prompt_cache_max_entries,
            prompt_cache_ttl_secs,
            cb_enabled,
            cb_threshold,
            cb_recovery_secs,
            cc_model_context_windows,
            telemetry_enabled,
            telemetry_beacon_url,
            beacon_auth_token,
            telemetry_dir,
            telemetry_db_path,
            telemetry_retention_days,
            telemetry_secret_path,
            telemetry_disabled_reason,
            config_path: None, // Set by caller (from_env_with_path)
        })
    }

    fn load_dotenv(custom_path: Option<PathBuf>) -> Option<PathBuf> {
        if let Some(path) = custom_path {
            if path.exists() && dotenvy::from_path(&path).is_ok() {
                return Some(path);
            }
            eprintln!("[WARN] WARNING: Custom config file not found: {}", path.display());
        }

        if let Ok(path) = dotenvy::dotenv() {
            return Some(path);
        }

        if let Ok(home) = env::var("HOME") {
            let home_config = PathBuf::from(home).join(".nexus-ai-gateway.env");
            if home_config.exists() && dotenvy::from_path(&home_config).is_ok() {
                return Some(home_config);
            }
        }

        let etc_config = PathBuf::from("/etc/nexus-ai-gateway/.env");
        if etc_config.exists() && dotenvy::from_path(&etc_config).is_ok() {
            return Some(etc_config);
        }

        None
    }

    /// Convenience constructor — may be used in tests or external tooling
    /// Tracking: Kept for testing convenience (PHASE 3.5)
    #[allow(dead_code)]
    pub fn from_env() -> Result<Self> {
        Self::from_env_with_path(None)
    }

    pub fn from_env_with_path(custom_path: Option<PathBuf>) -> Result<Self> {
        let stored_config_path = custom_path.clone();
        if let Some(path) = Self::load_dotenv(custom_path) {
            eprintln!("📄 Loaded config from: {}", path.display());
        } else {
            eprintln!("ℹ️ No .env file found, using environment variables only");
        }

        let port = env::var("PORT").ok().and_then(|p| p.parse().ok()).unwrap_or(8315);

        // Issue #78: configurable bind address, default 127.0.0.1 (loopback-only).
        let bind_addr = Self::resolve_bind_addr(env::var("BIND_ADDR").ok(), env::var("HOST").ok());

        let base_url = env::var("UPSTREAM_BASE_URL").map_err(|_| {
            anyhow::anyhow!(
                "UPSTREAM_BASE_URL is required. Set it to your OpenAI-compatible endpoint.\n\
                 Examples:\n\
                 - OpenRouter: https://openrouter.ai/api\n\
                 - OpenAI: https://api.openai.com\n\
                 - Local: http://localhost:11434"
            )
        })?;

        // Issue #115: support the *_FILE convention (direct value > _FILE > none).
        let api_key = Self::resolve_secret(
            env::var("UPSTREAM_API_KEY").ok(),
            env::var("UPSTREAM_API_KEY_FILE").ok(),
        )
        .or_else(|| {
            Self::resolve_secret(
                env::var("OPENROUTER_API_KEY").ok(),
                env::var("OPENROUTER_API_KEY_FILE").ok(),
            )
        });

        let reasoning_model = env::var("REASONING_MODEL").ok();
        let completion_model = env::var("COMPLETION_MODEL").ok();

        let debug =
            env::var("DEBUG").map(|v| v == "1" || v.to_lowercase() == "true").unwrap_or(false);

        let verbose =
            env::var("VERBOSE").map(|v| v == "1" || v.to_lowercase() == "true").unwrap_or(false);

        if base_url.ends_with("/v1") {
            eprintln!("[WARN] WARNING: UPSTREAM_BASE_URL ends with '/v1'");
            eprintln!("   This will result in URLs like: {}/v1/chat/completions", base_url);
            eprintln!("   Consider removing '/v1' from UPSTREAM_BASE_URL");
            eprintln!("   Correct: https://openrouter.ai/api");
            eprintln!("   Wrong:   https://openrouter.ai/api/v1");
        }

        let web_fetch_enabled = env::var("WEB_FETCH_ENABLED")
            .map(|v| v != "0" && v.to_lowercase() != "false")
            .unwrap_or(true);

        let web_fetch_max_retries =
            env::var("WEB_FETCH_MAX_RETRIES").ok().and_then(|v| v.parse().ok()).unwrap_or(3);

        let web_fetch_timeout_secs =
            env::var("WEB_FETCH_TIMEOUT_SECS").ok().and_then(|v| v.parse().ok()).unwrap_or(15);

        // Multi-upstream configuration
        let mut upstreams = HashMap::new();
        upstreams.insert(
            "default".to_string(),
            UpstreamConfig {
                base_url: base_url.clone(),
                api_key: api_key.clone(),
                upstream_type: None,
            },
        );

        if let Ok(bm_url) = env::var("UPSTREAM_BIGMODEL_BASE_URL") {
            let bm_type = env::var("UPSTREAM_BIGMODEL_TYPE")
                .ok()
                .and_then(|v| v.parse::<UpstreamType>().ok());
            upstreams.insert(
                "bigmodel".to_string(),
                UpstreamConfig {
                    base_url: bm_url,
                    api_key: Self::resolve_secret(
                        env::var("UPSTREAM_BIGMODEL_API_KEY").ok(),
                        env::var("UPSTREAM_BIGMODEL_API_KEY_FILE").ok(),
                    ),
                    upstream_type: bm_type,
                },
            );
            eprintln!(
                " ✅ BigModel upstream configured [type={}]",
                bm_type.map(|t| t.to_string()).unwrap_or_else(|| "global".to_string())
            );
        }

        if let Ok(cf_url) = env::var("UPSTREAM_CF_BASE_URL") {
            let cf_type =
                env::var("UPSTREAM_CF_TYPE").ok().and_then(|v| v.parse::<UpstreamType>().ok());
            upstreams.insert(
                "cf".to_string(),
                UpstreamConfig {
                    base_url: cf_url,
                    api_key: Self::resolve_secret(
                        env::var("UPSTREAM_CF_API_KEY").ok(),
                        env::var("UPSTREAM_CF_API_KEY_FILE").ok(),
                    ),
                    upstream_type: cf_type,
                },
            );
            eprintln!(
                " ✅ Cloudflare upstream configured [type={}]",
                cf_type.map(|t| t.to_string()).unwrap_or_else(|| "global".to_string())
            );
        }

        // Issue #35 F3: Generalized per-upstream type scanning
        // After constructing upstreams, scan for UPSTREAM_<NAME>_TYPE for ANY named upstream
        // This captures custom upstreams beyond bigmodel/cf
        for (name, upstream) in upstreams.iter_mut() {
            if name == "default" {
                continue; // Default always inherits global type
            }
            if upstream.upstream_type.is_none() {
                let type_key = format!("UPSTREAM_{}_TYPE", name.to_uppercase());
                if let Ok(type_val) = env::var(&type_key) {
                    if let Ok(t) = type_val.parse::<UpstreamType>() {
                        upstream.upstream_type = Some(t);
                        tracing::info!(
                            "[PIN] Upstream '{}' type set to {} via {}",
                            name,
                            t,
                            type_key
                        );
                    } else {
                        tracing::warn!("[WARN] Invalid {}: '{}'", type_key, type_val);
                    }
                }
            }
        }

        // Model Mapping Table from env vars
        // Note: env var names use underscores (POSIX), model IDs use hyphens
        let mut model_map = HashMap::new();
        for (key, value) in env::vars() {
            if let Some(model_id_raw) = key.strip_prefix("MODEL_MAP_") {
                let model_id = model_id_raw.replace('_', "-").to_lowercase();
                if let Some((upstream, target)) = value.split_once(':') {
                    model_map.insert(
                        model_id.clone(),
                        ModelRoute {
                            upstream_name: upstream.to_string(),
                            target_model: target.to_string(),
                        },
                    );
                    eprintln!(" [PIN] Model map: {} -> {}:{}", model_id, upstream, target);
                }
            }
        }

        eprintln!(" 📊 Upstreams: {}, Model mappings: {}", upstreams.len(), model_map.len());

        // Concurrency tuning (Opción B)
        let max_concurrent_per_model =
            env::var("MAX_CONCURRENT_PER_MODEL").ok().and_then(|v| v.parse().ok()).unwrap_or(5);

        let permit_timeout_secs =
            env::var("PERMIT_TIMEOUT_SECS").ok().and_then(|v| v.parse().ok()).unwrap_or(180);
        let upstream_type = match std::env::var("NEXUS_UPSTREAM_TYPE") {
            Ok(val) => match val.parse::<UpstreamType>() {
                Ok(t) => t,
                Err(_) => {
                    tracing::warn!(
                        "Invalid NEXUS_UPSTREAM_TYPE='{}' — valid values are: anthropic, nim, openai, openrouter. Defaulting to nim.",
                        val
                    );
                    UpstreamType::NIM
                }
            },
            Err(_) => UpstreamType::NIM,
        };

        // v0.13.0: Prompt cache configuration (for self-hosted NIM with KV_CACHE_REUSE=1)
        let prompt_cache_enabled = env::var("NIM_PROMPT_CACHE_ENABLED")
            .map(|v| !v.is_empty() && v != "0" && v.to_lowercase() != "false")
            .unwrap_or(false);
        let prompt_cache_max_entries = env::var("NIM_PROMPT_CACHE_MAX_ENTRIES")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(1000);
        let prompt_cache_ttl_secs =
            env::var("NIM_PROMPT_CACHE_TTL_SECS").ok().and_then(|v| v.parse().ok()).unwrap_or(300);
        // Circuit breaker configuration (v0.14.1)
        let cb_enabled =
            env::var("CB_ENABLED").map(|v| v == "1" || v.to_lowercase() == "true").unwrap_or(false);
        let cb_threshold =
            env::var("CB_THRESHOLD").ok().and_then(|v| v.parse().ok()).unwrap_or(10).max(1);
        let cb_recovery_secs =
            env::var("CB_RECOVERY_SECS").ok().and_then(|v| v.parse().ok()).unwrap_or(60).max(1);

        // Dynamic context window mapping (Issue #28)
        let cc_model_context_windows = env::var("CC_MODEL_CONTEXT_WINDOWS")
            .map(|v| Self::parse_model_context_windows(&v))
            .unwrap_or_default();

        // Telemetry configuration (v0.18.0)
        // v0.19.0: Default ON — telemetry is always active from minute one.
        // Users can disable with TELEMETRY_ENABLED=false
        let telemetry_enabled = env::var("TELEMETRY_ENABLED")
            .map(|v| v == "1" || v.to_lowercase() == "true")
            .unwrap_or(true);

        // v0.19.0: Beacon URL obfuscated via obfstr — not visible in binary strings
        // v0.19.0: Beacon URL is hardcoded by default — users don't need to configure it.
        let telemetry_beacon_url =
            env::var("TELEMETRY_BEACON_URL").ok().filter(|u| !u.is_empty()).or_else(|| {
                Some(
                    obfstr::obfstr!(
                        "https://nexus-beacon-receiver.enerby212.workers.dev/v1/beacon"
                    )
                    .to_string(),
                )
            });

        // v0.19.0: Auth token obfuscated via obfstr — not visible in binary strings
        // v0.19.0: Auth token is compiled into the binary — users don't need to configure it.
        let beacon_auth_token = env::var("BEACON_AUTH_TOKEN").ok().or_else(|| {
            Some(
                obfstr::obfstr!("e5595631b251830324175922cf5a75740aa03c0616d57226050c15629051b9d2")
                    .to_string(),
            )
        });

        // CR fix: Never fall back to /tmp for secrets — world-readable + cleared on reboot.
        // If HOME is unset AND no explicit paths provided, disable telemetry.
        let explicit_telemetry_dir = env::var("TELEMETRY_DIR").ok();
        let explicit_secret_path = env::var("TELEMETRY_SECRET_PATH").ok();

        let home_dir = match std::env::var("HOME") {
            Ok(h) if !h.is_empty() => h,
            _ => String::new(),
        };

        let telemetry_dir = explicit_telemetry_dir
            .clone()
            .unwrap_or_else(|| format!("{home_dir}/.local/share/nexus-ai-gateway"));
        let telemetry_db_path = env::var("TELEMETRY_DB_PATH")
            .unwrap_or_else(|_| format!("{telemetry_dir}/telemetry.db"));

        // Only force-disable telemetry when HOME is unset AND no explicit paths given
        // (systemd/containers commonly set TELEMETRY_DIR explicitly)
        let (telemetry_enabled, telemetry_disabled_reason) = if home_dir.is_empty()
            && explicit_telemetry_dir.is_none()
            && explicit_secret_path.is_none()
        {
            if telemetry_enabled {
                (false, Some("$HOME not set and no explicit TELEMETRY_DIR/TELEMETRY_SECRET_PATH — cannot safely store secret".to_string()))
            } else {
                (false, None)
            }
        } else {
            (telemetry_enabled, None)
        };

        let telemetry_retention_days = env::var("TELEMETRY_RETENTION_DAYS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(30)
            .max(1);

        let telemetry_secret_path =
            explicit_secret_path.unwrap_or_else(|| format!("{telemetry_dir}/.telemetry_secret"));

        // Issue #35 Bug F: Validate model_map routes against configured upstreams
        for (model_id, route) in &model_map {
            if route.upstream_name != "default" {
                if let Some(upstream) = upstreams.get(&route.upstream_name) {
                    if upstream.upstream_type.is_none() {
                        tracing::warn!(
                        "[WARN] Model '{}' routes to upstream '{}' but no UPSTREAM_{}_TYPE configured — using global type '{}'",
                        model_id, route.upstream_name, route.upstream_name.to_uppercase(), upstream_type
                    );
                    }
                } else {
                    tracing::warn!(
                    "[WARN] Model '{}' routes to upstream '{}' which is not configured — will fall back to 'default'",
                    model_id, route.upstream_name
                );
                }
            }
        }

        let config = Config {
            port,
            bind_addr,
            base_url,
            api_key,
            reasoning_model,
            completion_model,
            debug,
            verbose,
            web_fetch_enabled,
            web_fetch_max_retries,
            web_fetch_timeout_secs,
            upstreams,
            model_map,
            max_concurrent_per_model,
            permit_timeout_secs,
            upstream_type,
            prompt_cache_enabled,
            prompt_cache_max_entries,
            prompt_cache_ttl_secs,
            cb_enabled,
            cb_threshold,
            cb_recovery_secs,
            cc_model_context_windows,
            telemetry_enabled,
            telemetry_beacon_url,
            beacon_auth_token,
            telemetry_dir,
            telemetry_db_path,
            telemetry_retention_days,
            telemetry_secret_path,
            telemetry_disabled_reason,
            config_path: stored_config_path,
        };
        Ok(config)
    }

    /// Returns the chat completions URL for the default upstream.
    /// NOTE: Currently unused but kept for future use or external callers.
    #[allow(dead_code)]
    pub fn chat_completions_url(&self) -> String {
        format!("{}/v1/chat/completions", self.base_url.trim_end_matches('/'))
    }

    pub fn get_upstream_url(&self, upstream_name: &str) -> String {
        let upstream = self
            .upstreams
            .get(upstream_name)
            .or_else(|| self.upstreams.get("default"))
            .expect("default upstream must exist");
        format!("{}/v1/chat/completions", upstream.base_url.trim_end_matches('/'))
    }

    pub fn get_upstream_key(&self, upstream_name: &str) -> Option<String> {
        self.upstreams
            .get(upstream_name)
            .or_else(|| self.upstreams.get("default"))
            .and_then(|u| u.api_key.clone())
    }

    /// Get the UpstreamType for a specific upstream.
    /// Returns per-upstream type if configured, else falls back to global.
    /// User Decision (Q3, Option A): type is a property of the endpoint, not the route.
    pub fn get_upstream_type(&self, upstream_name: &str) -> UpstreamType {
        self.upstreams
            .get(upstream_name)
            .and_then(|u| u.upstream_type)
            .unwrap_or(self.upstream_type)
    }

    /// Reload config from environment/dotenv file
    /// Preserves CLI overrides (debug, verbose, port)
    pub fn reload(
        cli_debug: bool,
        cli_verbose: bool,
        cli_port: Option<u16>,
        cli_bind: Option<String>,
        config_path: Option<PathBuf>,
    ) -> Result<Self> {
        let env_map = Self::load_env_to_map(config_path.clone())?;
        let mut config = Self::from_map(&env_map)?;
        // fix#52 (CodeRabbit): Preserve config_path so subsequent reloads
        // (SIGHUP/watcher) still use the custom --config path
        config.config_path = config_path;
        if cli_debug {
            config.debug = true;
        }
        if cli_verbose {
            config.verbose = true;
        }
        if let Some(port) = cli_port {
            config.port = port;
        }
        // Issue #78: the listener is bound once at startup and cannot be re-bound on
        // reload; preserve the startup-resolved bind_addr so the reported config
        // matches reality (mirrors the cli_port override behavior).
        if let Some(bind) = cli_bind {
            config.bind_addr = bind;
        }
        Ok(config)
    }
}

/// Thread-safe shared config for hot-reload support (lock-free reads via arc-swap)
pub type SharedConfig = Arc<arc_swap::ArcSwap<Config>>;

#[cfg(test)]
mod tests {
    use super::*;

    fn make_test_map() -> HashMap<String, String> {
        let mut map = HashMap::new();
        map.insert("UPSTREAM_BASE_URL".to_string(), "http://localhost:11434".to_string());
        map.insert("UPSTREAM_API_KEY".to_string(), "test-key".to_string());
        map.insert("NEXUS_UPSTREAM_TYPE".to_string(), "nim".to_string());
        map
    }

    #[test]
    fn test_get_upstream_type_per_upstream() {
        let mut map = make_test_map();
        map.insert("UPSTREAM_BIGMODEL_BASE_URL".to_string(), "http://bigmodel:11434".to_string());
        map.insert("UPSTREAM_BIGMODEL_TYPE".to_string(), "anthropic".to_string());
        let config = Config::from_map(&map).unwrap();
        // bigmodel has explicit type=Anthropic
        assert_eq!(config.get_upstream_type("bigmodel"), UpstreamType::Anthropic);
        // default should still be NIM (global)
        assert_eq!(config.get_upstream_type("default"), UpstreamType::NIM);
    }

    #[test]
    fn test_get_upstream_type_fallback_to_global() {
        let map = make_test_map();
        let config = Config::from_map(&map).unwrap();
        // "unknown" upstream falls back to global
        assert_eq!(config.get_upstream_type("unknown"), UpstreamType::NIM);
    }

    #[test]
    fn test_get_upstream_type_default_uses_global() {
        let map = make_test_map();
        let config = Config::from_map(&map).unwrap();
        // "default" upstream has upstream_type=None, returns global
        assert_eq!(config.get_upstream_type("default"), UpstreamType::NIM);
    }

    #[test]
    fn test_upstream_config_none_type() {
        let uc = UpstreamConfig {
            base_url: "http://test".to_string(),
            api_key: None,
            upstream_type: None,
        };
        assert!(uc.upstream_type.is_none());
    }

    #[test]
    fn test_upstream_type_from_env_bigmodel() {
        let mut map = make_test_map();
        map.insert("UPSTREAM_BIGMODEL_BASE_URL".to_string(), "http://bigmodel:11434".to_string());
        map.insert("UPSTREAM_BIGMODEL_TYPE".to_string(), "anthropic".to_string());
        let config = Config::from_map(&map).unwrap();
        assert_eq!(config.get_upstream_type("bigmodel"), UpstreamType::Anthropic);
    }

    #[test]
    fn test_upstream_type_invalid_ignored() {
        let mut map = make_test_map();
        map.insert("UPSTREAM_BIGMODEL_BASE_URL".to_string(), "http://bigmodel:11434".to_string());
        map.insert("UPSTREAM_BIGMODEL_TYPE".to_string(), "invalid_type".to_string());
        let config = Config::from_map(&map).unwrap();
        // Invalid type should be ignored, falls back to global (NIM)
        assert_eq!(config.get_upstream_type("bigmodel"), UpstreamType::NIM);
    }

    #[test]
    fn test_upstream_type_generalized_custom() {
        // Simulate a custom upstream added via env var
        // Since from_map() only adds known upstream names (default/bigmodel/cf),
        // we need to manually test the scanning logic on an existing upstream
        let mut map = make_test_map();
        map.insert("UPSTREAM_BIGMODEL_BASE_URL".to_string(), "http://bigmodel:11434".to_string());
        // Don't set UPSTREAM_BIGMODEL_TYPE inline — let the generalized scan pick it up
        // Actually, the bigmodel block already parses UPSTREAM_BIGMODEL_TYPE inline.
        // So the generalized scan only applies if upstream.upstream_type is None after inline parsing.
        // To test the generalized path, we need a custom upstream that was added some other way.
        // For now, test that the inline parsing works and the generalized scan doesn't overwrite it.
        map.insert("UPSTREAM_BIGMODEL_TYPE".to_string(), "openai".to_string());
        let config = Config::from_map(&map).unwrap();
        assert_eq!(config.get_upstream_type("bigmodel"), UpstreamType::OpenAI);
    }

    #[test]
    fn test_hot_reload_preserves_type() {
        // Verify that reload() preserves per-upstream types
        let mut map = make_test_map();
        map.insert("UPSTREAM_BIGMODEL_BASE_URL".to_string(), "http://bigmodel:11434".to_string());
        map.insert("UPSTREAM_BIGMODEL_TYPE".to_string(), "anthropic".to_string());
        let config = Config::from_map(&map).unwrap();
        assert_eq!(config.get_upstream_type("bigmodel"), UpstreamType::Anthropic);
        // reload() delegates to from_map(), so it should work automatically
    }

    #[test]
    fn test_validation_warns_on_missing_type() {
        // Model map routes to bigmodel which has no explicit type
        let mut map = make_test_map();
        map.insert("UPSTREAM_BIGMODEL_BASE_URL".to_string(), "http://bigmodel:11434".to_string());
        // No UPSTREAM_BIGMODEL_TYPE set — should warn but not fail
        map.insert("MODEL_MAP_CLAUDE_OPUS_4_6".to_string(), "bigmodel:some-model".to_string());
        let config = Config::from_map(&map).unwrap();
        // Config should still load successfully (warnings, not errors)
        // Env var MODEL_MAP_CLAUDE_OPUS_4_6 -> key "claude-opus-4-6" (underscores->hyphens, lowercase)
        assert!(config.model_map.contains_key("claude-opus-4-6"));
    }

    #[test]
    fn test_validation_warns_on_missing_upstream() {
        // Model map routes to a non-existent upstream
        let mut map = make_test_map();
        map.insert("MODEL_MAP_CLAUDE_OPUS_4_6".to_string(), "nonexistent:some-model".to_string());
        let config = Config::from_map(&map).unwrap();
        // Config should still load, model_map entry exists
        assert!(config.model_map.contains_key("claude-opus-4-6"));
    }

    #[test]
    fn test_validation_no_warns_when_all_typed() {
        // All upstreams have explicit types — no warnings expected
        let mut map = make_test_map();
        map.insert("UPSTREAM_BIGMODEL_BASE_URL".to_string(), "http://bigmodel:11434".to_string());
        map.insert("UPSTREAM_BIGMODEL_TYPE".to_string(), "anthropic".to_string());
        map.insert("MODEL_MAP_CLAUDE_OPUS_4_6".to_string(), "bigmodel:some-model".to_string());
        let config = Config::from_map(&map).unwrap();
        assert_eq!(config.get_upstream_type("bigmodel"), UpstreamType::Anthropic);
    }

    // =========================================================================
    // Issue #35 F10: Per-route upstream_type integration tests
    // =========================================================================

    #[test]
    fn test_per_route_type_anthropic_with_global_nim() {
        // Global=NIM, bigmodel=Anthropic -> model_map routing to bigmodel uses type Anthropic
        let mut map = make_test_map();
        map.insert("UPSTREAM_BIGMODEL_BASE_URL".to_string(), "http://bigmodel:11434".to_string());
        map.insert("UPSTREAM_BIGMODEL_TYPE".to_string(), "anthropic".to_string());
        map.insert("MODEL_MAP_CLAUDE_OPUS_4_6".to_string(), "bigmodel:z-ai/glm5".to_string());
        let config = Config::from_map(&map).unwrap();

        // default uses global NIM
        assert_eq!(config.get_upstream_type("default"), UpstreamType::NIM);
        // bigmodel uses per-upstream Anthropic
        assert_eq!(config.get_upstream_type("bigmodel"), UpstreamType::Anthropic);
    }

    #[test]
    fn test_per_route_type_fallback_to_global() {
        // bigmodel without explicit type falls back to global
        let mut map = make_test_map();
        map.insert("UPSTREAM_BIGMODEL_BASE_URL".to_string(), "http://bigmodel:11434".to_string());
        // No UPSTREAM_BIGMODEL_TYPE
        let config = Config::from_map(&map).unwrap();
        assert_eq!(config.get_upstream_type("bigmodel"), UpstreamType::NIM);
    }

    #[test]
    fn test_unknown_upstream_name_in_get_type() {
        let map = make_test_map();
        let config = Config::from_map(&map).unwrap();
        // Completely unknown upstream -> falls back to global
        assert_eq!(config.get_upstream_type("nonexistent"), UpstreamType::NIM);
    }

    // =========================================================================
    // Issue #78: bind address configuration tests
    // =========================================================================

    #[test]
    fn test_bind_addr_defaults_to_loopback() {
        let map = make_test_map();
        let config = Config::from_map(&map).unwrap();
        // Secure default: loopback-only, NOT 0.0.0.0.
        assert_eq!(config.bind_addr, "127.0.0.1");
    }

    #[test]
    fn test_bind_addr_override_from_env() {
        let mut map = make_test_map();
        map.insert("BIND_ADDR".to_string(), "0.0.0.0".to_string());
        let config = Config::from_map(&map).unwrap();
        // Explicit opt-in to expose on all interfaces.
        assert_eq!(config.bind_addr, "0.0.0.0");
    }

    #[test]
    fn test_bind_addr_ipv6_loopback_accepted() {
        let mut map = make_test_map();
        map.insert("BIND_ADDR".to_string(), "::1".to_string());
        let config = Config::from_map(&map).unwrap();
        assert_eq!(config.bind_addr, "::1");
    }

    #[test]
    fn test_bind_addr_whitespace_trimmed() {
        let mut map = make_test_map();
        map.insert("BIND_ADDR".to_string(), "  192.168.1.50  ".to_string());
        let config = Config::from_map(&map).unwrap();
        assert_eq!(config.bind_addr, "192.168.1.50");
    }

    #[test]
    fn test_bind_addr_invalid_falls_back_to_loopback() {
        let mut map = make_test_map();
        map.insert("BIND_ADDR".to_string(), "not-an-ip".to_string());
        let config = Config::from_map(&map).unwrap();
        // Parse, don't validate: invalid input degrades to the safe default.
        assert_eq!(config.bind_addr, "127.0.0.1");
    }

    #[test]
    fn test_host_alone_is_ignored_dead_config() {
        // HOST=0.0.0.0 with NO BIND_ADDR must NOT expose the listener.
        // HOST is legacy dead config — it never controlled the bind.
        let mut map = make_test_map();
        map.insert("HOST".to_string(), "0.0.0.0".to_string());
        let config = Config::from_map(&map).unwrap();
        assert_eq!(config.bind_addr, "127.0.0.1");
    }

    #[test]
    fn test_bind_addr_takes_precedence_over_host() {
        let mut map = make_test_map();
        map.insert("HOST".to_string(), "0.0.0.0".to_string());
        map.insert("BIND_ADDR".to_string(), "127.0.0.1".to_string());
        let config = Config::from_map(&map).unwrap();
        // BIND_ADDR wins; HOST is ignored entirely.
        assert_eq!(config.bind_addr, "127.0.0.1");
    }

    #[test]
    fn test_normalize_bind_addr_helper() {
        assert_eq!(Config::normalize_bind_addr("0.0.0.0"), "0.0.0.0");
        assert_eq!(Config::normalize_bind_addr("127.0.0.1"), "127.0.0.1");
        assert_eq!(Config::normalize_bind_addr("::1"), "::1");
        // Invalid -> safe fallback.
        assert_eq!(Config::normalize_bind_addr("garbage"), "127.0.0.1");
        assert_eq!(Config::normalize_bind_addr(""), "127.0.0.1");
    }

    // resolve_socket_addr (CodeRabbit #109): typed SocketAddr, correct IPv6 formatting.

    #[test]
    fn test_resolve_socket_addr_ipv4() {
        let sa = Config::resolve_socket_addr("127.0.0.1", 8315).unwrap();
        assert_eq!(sa.to_string(), "127.0.0.1:8315");
        assert!(sa.ip().is_loopback());
    }

    #[test]
    fn test_resolve_socket_addr_ipv6_is_bracketed() {
        // Regression guard: plain format!("{}:{}") would yield the invalid "::1:8315".
        let sa = Config::resolve_socket_addr("::1", 8315).unwrap();
        assert_eq!(sa.to_string(), "[::1]:8315");
        assert!(sa.ip().is_loopback());
    }

    #[test]
    fn test_resolve_socket_addr_all_interfaces() {
        let sa = Config::resolve_socket_addr("0.0.0.0", 8316).unwrap();
        assert_eq!(sa.to_string(), "0.0.0.0:8316");
        assert!(!sa.ip().is_loopback());
    }

    #[test]
    fn test_resolve_socket_addr_invalid_errors() {
        assert!(Config::resolve_socket_addr("not-an-ip", 8315).is_err());
    }

    // =========================================================================
    // Issue #115: *_FILE secret resolution
    // =========================================================================

    #[test]
    fn test_resolve_secret_direct_value() {
        assert_eq!(Config::resolve_secret(Some("k".into()), None), Some("k".to_string()));
    }

    #[test]
    fn test_resolve_secret_direct_wins_over_file() {
        // A non-empty direct value wins even if a (here, nonexistent) file is given.
        assert_eq!(
            Config::resolve_secret(Some("direct".into()), Some("/no/such/file".into())),
            Some("direct".to_string())
        );
    }

    #[test]
    fn test_resolve_secret_reads_and_trims_file() {
        let path = std::env::temp_dir().join(format!(
            "nexus-secret-{}-{}.txt",
            std::process::id(),
            line!()
        ));
        std::fs::write(&path, "  file-secret\n\n").unwrap();
        let got = Config::resolve_secret(None, Some(path.to_string_lossy().to_string()));
        let _ = std::fs::remove_file(&path);
        assert_eq!(got, Some("file-secret".to_string()));
    }

    #[test]
    fn test_resolve_secret_empty_file_is_none() {
        let path = std::env::temp_dir().join(format!(
            "nexus-secret-empty-{}-{}.txt",
            std::process::id(),
            line!()
        ));
        std::fs::write(&path, "   \n").unwrap();
        let got = Config::resolve_secret(None, Some(path.to_string_lossy().to_string()));
        let _ = std::fs::remove_file(&path);
        assert_eq!(got, None);
    }

    #[test]
    fn test_resolve_secret_missing_file_and_all_none() {
        assert_eq!(Config::resolve_secret(None, Some("/no/such/file/xyz".into())), None);
        assert_eq!(Config::resolve_secret(None, None), None);
        // Empty direct + empty file path both degrade to None.
        assert_eq!(Config::resolve_secret(Some(String::new()), Some(String::new())), None);
    }

    #[test]
    fn test_from_map_loads_api_key_from_file() {
        let path = std::env::temp_dir().join(format!(
            "nexus-upstream-key-{}-{}.txt",
            std::process::id(),
            line!()
        ));
        std::fs::write(&path, "secret-from-file\n").unwrap();
        let mut map = make_test_map();
        map.remove("UPSTREAM_API_KEY"); // only the _FILE variant present
        map.insert("UPSTREAM_API_KEY_FILE".to_string(), path.to_string_lossy().to_string());
        let config = Config::from_map(&map).unwrap();
        let _ = std::fs::remove_file(&path);
        assert_eq!(config.api_key, Some("secret-from-file".to_string()));
    }

    #[test]
    fn test_from_map_direct_key_wins_over_file() {
        let path = std::env::temp_dir().join(format!(
            "nexus-upstream-key2-{}-{}.txt",
            std::process::id(),
            line!()
        ));
        std::fs::write(&path, "from-file\n").unwrap();
        let mut map = make_test_map(); // has UPSTREAM_API_KEY=test-key
        map.insert("UPSTREAM_API_KEY_FILE".to_string(), path.to_string_lossy().to_string());
        let config = Config::from_map(&map).unwrap();
        let _ = std::fs::remove_file(&path);
        assert_eq!(config.api_key, Some("test-key".to_string()));
    }
}
