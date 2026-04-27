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
}

#[derive(Debug, Clone)]
pub struct ModelRoute {
    pub upstream_name: String,
    pub target_model: String,
}

#[derive(Debug, Clone)]
pub struct Config {
    pub port: u16,
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

    /// Parse CC_MODEL_CONTEXT_WINDOWS env var into a HashMap.
    /// Format: "claude-opus-4-6:200000,claude-sonnet-4-6:200000"
    fn parse_model_context_windows(value: &str) -> HashMap<String, u32> {
        let mut map = HashMap::new();
        for entry in value.split(',') {
            let entry = entry.trim();
            if let Some((model, limit_str)) = entry.split_once(':') {
                if let Ok(limit) = limit_str.trim().parse::<u32>() {
                    if limit > 0 {
                        map.insert(model.trim().to_string(), limit);
                    }
                }
            }
        }
        map
    }

    /// Create Config from a HashMap (used for thread-safe reload)
    fn from_map(data: &HashMap<String, String>) -> Result<Self> {
        let port = Self::get_from_map(data, "PORT").and_then(|p| p.parse().ok()).unwrap_or(8315);

        let base_url = Self::get_from_map(data, "UPSTREAM_BASE_URL").ok_or_else(|| {
            anyhow::anyhow!(
                "UPSTREAM_BASE_URL is required. Set it to your OpenAI-compatible endpoint.\n\
                Examples:\n\
                - OpenRouter: https://openrouter.ai/api\n\
                - OpenAI: https://api.openai.com\n\
                - Local: http://localhost:11434"
            )
        })?;

        let api_key = Self::get_from_map(data, "UPSTREAM_API_KEY")
            .or_else(|| Self::get_from_map(data, "OPENROUTER_API_KEY"))
            .filter(|k| !k.is_empty());

        let reasoning_model = Self::get_from_map(data, "REASONING_MODEL");
        let completion_model = Self::get_from_map(data, "COMPLETION_MODEL");

        let debug = Self::get_from_map(data, "DEBUG")
            .map(|v| v == "1" || v.to_lowercase() == "true")
            .unwrap_or(false);

        let verbose = Self::get_from_map(data, "VERBOSE")
            .map(|v| v == "1" || v.to_lowercase() == "true")
            .unwrap_or(false);

        if base_url.ends_with("/v1") {
            eprintln!("⚠️ WARNING: UPSTREAM_BASE_URL ends with '/v1'");
            eprintln!(" This will result in URLs like: {}/v1/chat/completions", base_url);
            eprintln!(" Consider removing '/v1' from UPSTREAM_BASE_URL");
            eprintln!(" Correct: https://openrouter.ai/api");
            eprintln!(" Wrong: https://openrouter.ai/api/v1");
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
            UpstreamConfig { base_url: base_url.clone(), api_key: api_key.clone() },
        );

        if let Some(bm_url) = Self::get_from_map(data, "UPSTREAM_BIGMODEL_BASE_URL") {
            upstreams.insert(
                "bigmodel".to_string(),
                UpstreamConfig {
                    base_url: bm_url,
                    api_key: Self::get_from_map(data, "UPSTREAM_BIGMODEL_API_KEY"),
                },
            );
            eprintln!(" ✅ BigModel upstream configured");
        }

        if let Some(cf_url) = Self::get_from_map(data, "UPSTREAM_CF_BASE_URL") {
            upstreams.insert(
                "cf".to_string(),
                UpstreamConfig {
                    base_url: cf_url,
                    api_key: Self::get_from_map(data, "UPSTREAM_CF_API_KEY"),
                },
            );
            eprintln!(" ✅ Cloudflare upstream configured");
        }

        // Model Mapping Table from env vars
        let mut model_map = HashMap::new();
        for (key, value) in data {
            if let Some(model_id_raw) = key.strip_prefix("MODEL_MAP_") {
                let model_id = model_id_raw.replace('_', "-");
                if let Some((upstream, target)) = value.split_once(':') {
                    model_map.insert(
                        model_id.clone(),
                        ModelRoute {
                            upstream_name: upstream.to_string(),
                            target_model: target.to_string(),
                        },
                    );
                    eprintln!(" 📍 Model map: {} → {}:{}", model_id, upstream, target);
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

        Ok(Config {
            port,
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
        })
    }

    fn load_dotenv(custom_path: Option<PathBuf>) -> Option<PathBuf> {
        if let Some(path) = custom_path {
            if path.exists() && dotenvy::from_path(&path).is_ok() {
                return Some(path);
            }
            eprintln!("⚠️  WARNING: Custom config file not found: {}", path.display());
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
        if let Some(path) = Self::load_dotenv(custom_path) {
            eprintln!("📄 Loaded config from: {}", path.display());
        } else {
            eprintln!("ℹ️  No .env file found, using environment variables only");
        }

        let port = env::var("PORT").ok().and_then(|p| p.parse().ok()).unwrap_or(8315);

        let base_url = env::var("UPSTREAM_BASE_URL").map_err(|_| {
            anyhow::anyhow!(
                "UPSTREAM_BASE_URL is required. Set it to your OpenAI-compatible endpoint.\n\
                Examples:\n\
                  - OpenRouter: https://openrouter.ai/api\n\
                  - OpenAI: https://api.openai.com\n\
                  - Local: http://localhost:11434"
            )
        })?;

        let api_key = env::var("UPSTREAM_API_KEY")
            .or_else(|_| env::var("OPENROUTER_API_KEY"))
            .ok()
            .filter(|k| !k.is_empty());

        let reasoning_model = env::var("REASONING_MODEL").ok();
        let completion_model = env::var("COMPLETION_MODEL").ok();

        let debug =
            env::var("DEBUG").map(|v| v == "1" || v.to_lowercase() == "true").unwrap_or(false);

        let verbose =
            env::var("VERBOSE").map(|v| v == "1" || v.to_lowercase() == "true").unwrap_or(false);

        if base_url.ends_with("/v1") {
            eprintln!("⚠️  WARNING: UPSTREAM_BASE_URL ends with '/v1'");
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
            UpstreamConfig { base_url: base_url.clone(), api_key: api_key.clone() },
        );

        if let Ok(bm_url) = env::var("UPSTREAM_BIGMODEL_BASE_URL") {
            upstreams.insert(
                "bigmodel".to_string(),
                UpstreamConfig {
                    base_url: bm_url,
                    api_key: env::var("UPSTREAM_BIGMODEL_API_KEY").ok(),
                },
            );
            eprintln!("  ✅ BigModel upstream configured");
        }

        if let Ok(cf_url) = env::var("UPSTREAM_CF_BASE_URL") {
            upstreams.insert(
                "cf".to_string(),
                UpstreamConfig { base_url: cf_url, api_key: env::var("UPSTREAM_CF_API_KEY").ok() },
            );
            eprintln!("  ✅ Cloudflare upstream configured");
        }

        // Model Mapping Table from env vars
        // Note: env var names use underscores (POSIX), model IDs use hyphens
        let mut model_map = HashMap::new();
        for (key, value) in env::vars() {
            if let Some(model_id_raw) = key.strip_prefix("MODEL_MAP_") {
                let model_id = model_id_raw.replace('_', "-");
                if let Some((upstream, target)) = value.split_once(':') {
                    model_map.insert(
                        model_id.clone(),
                        ModelRoute {
                            upstream_name: upstream.to_string(),
                            target_model: target.to_string(),
                        },
                    );
                    eprintln!("  📍 Model map: {} → {}:{}", model_id, upstream, target);
                }
            }
        }

        eprintln!("  📊 Upstreams: {}, Model mappings: {}", upstreams.len(), model_map.len());

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

        Ok(Config {
            port,
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
        })
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
    /// Currently returns the global upstream_type (all upstreams must be same type).
    /// Future: support per-upstream type configuration.
    pub fn get_upstream_type(&self, _upstream_name: &str) -> UpstreamType {
        self.upstream_type
    }

    /// Reload config from environment/dotenv file
    /// Preserves CLI overrides (debug, verbose, port)
    pub fn reload(cli_debug: bool, cli_verbose: bool, cli_port: Option<u16>) -> Result<Self> {
        let env_map = Self::load_env_to_map(None)?;
        let mut config = Self::from_map(&env_map)?;
        if cli_debug {
            config.debug = true;
        }
        if cli_verbose {
            config.verbose = true;
        }
        if let Some(port) = cli_port {
            config.port = port;
        }
        Ok(config)
    }
}

/// Thread-safe shared config for hot-reload support (lock-free reads via arc-swap)
pub type SharedConfig = Arc<arc_swap::ArcSwap<Config>>;
