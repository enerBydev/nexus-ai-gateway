use anyhow::Result;
use std::collections::HashMap;
use std::sync::{Arc, RwLock};
use std::{env, path::PathBuf};

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
}

impl Config {
    fn load_dotenv(custom_path: Option<PathBuf>) -> Option<PathBuf> {
        if let Some(path) = custom_path {
            if path.exists() {
                if let Ok(_) = dotenvy::from_path(&path) {
                    return Some(path);
                }
            }
            eprintln!(
                "⚠️  WARNING: Custom config file not found: {}",
                path.display()
            );
        }

        if let Ok(path) = dotenvy::dotenv() {
            return Some(path);
        }

        if let Some(home) = env::var("HOME").ok() {
            let home_config = PathBuf::from(home).join(".nexus-ai-gateway.env");
            if home_config.exists() {
                if let Ok(_) = dotenvy::from_path(&home_config) {
                    return Some(home_config);
                }
            }
        }

        let etc_config = PathBuf::from("/etc/nexus-ai-gateway/.env");
        if etc_config.exists() {
            if let Ok(_) = dotenvy::from_path(&etc_config) {
                return Some(etc_config);
            }
        }

        None
    }

    pub fn from_env() -> Result<Self> {
        Self::from_env_with_path(None)
    }

    pub fn from_env_with_path(custom_path: Option<PathBuf>) -> Result<Self> {
        if let Some(path) = Self::load_dotenv(custom_path) {
            eprintln!("📄 Loaded config from: {}", path.display());
        } else {
            eprintln!("ℹ️  No .env file found, using environment variables only");
        }

        let port = env::var("PORT")
            .ok()
            .and_then(|p| p.parse().ok())
            .unwrap_or(3000);

        let base_url = env::var("UPSTREAM_BASE_URL")
            .or_else(|_| env::var("NEXUS_BRAIN_BASE_URL"))
            .map_err(|_| {
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

        let debug = env::var("DEBUG")
            .map(|v| v == "1" || v.to_lowercase() == "true")
            .unwrap_or(false);

        let verbose = env::var("VERBOSE")
            .map(|v| v == "1" || v.to_lowercase() == "true")
            .unwrap_or(false);

        if base_url.ends_with("/v1") {
            eprintln!("⚠️  WARNING: UPSTREAM_BASE_URL ends with '/v1'");
            eprintln!(
                "   This will result in URLs like: {}/v1/chat/completions",
                base_url
            );
            eprintln!("   Consider removing '/v1' from UPSTREAM_BASE_URL");
            eprintln!("   Correct: https://openrouter.ai/api");
            eprintln!("   Wrong:   https://openrouter.ai/api/v1");
        }

        let web_fetch_enabled = env::var("WEB_FETCH_ENABLED")
            .map(|v| v != "0" && v.to_lowercase() != "false")
            .unwrap_or(true);

        let web_fetch_max_retries = env::var("WEB_FETCH_MAX_RETRIES")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(3);

        let web_fetch_timeout_secs = env::var("WEB_FETCH_TIMEOUT_SECS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(15);

        // Multi-upstream configuration
        let mut upstreams = HashMap::new();
        upstreams.insert(
            "default".to_string(),
            UpstreamConfig {
                base_url: base_url.clone(),
                api_key: api_key.clone(),
            },
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
                UpstreamConfig {
                    base_url: cf_url,
                    api_key: env::var("UPSTREAM_CF_API_KEY").ok(),
                },
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

        eprintln!(
            "  📊 Upstreams: {}, Model mappings: {}",
            upstreams.len(),
            model_map.len()
        );

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
        })
    }

    #[allow(dead_code)]
    pub fn chat_completions_url(&self) -> String {
        format!(
            "{}/v1/chat/completions",
            self.base_url.trim_end_matches('/')
        )
    }

    pub fn get_upstream_url(&self, upstream_name: &str) -> String {
        let upstream = self
            .upstreams
            .get(upstream_name)
            .or_else(|| self.upstreams.get("default"))
            .expect("default upstream must exist");
        format!(
            "{}/v1/chat/completions",
            upstream.base_url.trim_end_matches('/')
        )
    }

    pub fn get_upstream_key(&self, upstream_name: &str) -> Option<String> {
        self.upstreams
            .get(upstream_name)
            .or_else(|| self.upstreams.get("default"))
            .and_then(|u| u.api_key.clone())
    }

    /// Reload config from environment/dotenv file
    /// Preserves CLI overrides (debug, verbose, port)
    pub fn reload(cli_debug: bool, cli_verbose: bool, cli_port: Option<u16>) -> Result<Self> {
        // Clear env vars set by previous dotenvy load so we get fresh values
        for (key, _) in env::vars() {
            if key.starts_with("MODEL_MAP_") || key.starts_with("UPSTREAM_") {
                env::remove_var(&key);
            }
        }
        let mut config = Self::from_env()?;
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

/// Thread-safe shared config for hot-reload support
pub type SharedConfig = Arc<RwLock<Config>>;
