use clap::{Parser, Subcommand};
use std::path::PathBuf;

#[derive(Parser, Debug)]
#[command(
    name = "nexus-ai-gateway",
    version,
    about = "Proxy Anthropic API requests to OpenAI-compatible endpoints",
    long_about = "A high-performance proxy that translates Anthropic Claude API requests \
                  to OpenAI-compatible endpoints like OpenRouter, allowing you to use \
                  Claude-compatible clients with any OpenAI-compatible API."
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Option<Command>,

    /// Path to custom .env configuration file
    #[arg(short, long, value_name = "FILE")]
    pub config: Option<PathBuf>,

    /// Enable debug logging (same as DEBUG=true)
    #[arg(short, long)]
    pub debug: bool,

    /// Enable verbose logging (logs full request/response bodies)
    #[arg(short, long)]
    pub verbose: bool,

    /// Port to listen on (overrides PORT env var)
    #[arg(short, long, value_name = "PORT")]
    pub port: Option<u16>,

    /// Run as background daemon
    #[arg(long)]
    pub daemon: bool,

    /// PID file path (used with daemon commands)
    #[arg(long, value_name = "FILE", default_value = "/tmp/nexus-ai-gateway.pid")]
    pub pid_file: PathBuf,
}

#[derive(Subcommand, Debug)]
pub enum Command {
    /// Stop running daemon
    Stop {
        /// PID file path
        #[arg(long, value_name = "FILE", default_value = "/tmp/nexus-ai-gateway.pid")]
        pid_file: PathBuf,
    },
    /// Check daemon status
    Status {
        /// PID file path
        #[arg(long, value_name = "FILE", default_value = "/tmp/nexus-ai-gateway.pid")]
        pid_file: PathBuf,
    },
    /// Scan Claude Code binary for model IDs, tools, and capabilities
    Scan {
        /// Generate .env template with model mapping entries
        #[arg(long, conflicts_with_all = ["launcher", "check"])]
        env: bool,
        /// Generate launcher script with symbiont env vars
        #[arg(long, conflicts_with_all = ["env", "check"])]
        launcher: bool,
        /// Only check if CC binary was updated since last scan
        #[arg(long, conflicts_with_all = ["env", "launcher"])]
        check: bool,
    },
    /// Interactive setup wizard for initial configuration
    Setup {
        /// Quick mode: only API key + intelligent defaults (skip interactive prompts)
        #[arg(long)]
        quick: bool,
    },
    /// View or modify configuration
    Config {
        #[command(subcommand)]
        action: ConfigAction,
    },
}

#[derive(Subcommand, Debug)]
pub enum ConfigAction {
    /// Show current configuration (formatted, with masked secrets)
    Show,
    /// Set a configuration value in .env file
    Set {
        /// Configuration key (e.g. PORT, UPSTREAM_API_KEY, MODEL_MAP_claude_opus_4_6)
        key: String,
        /// New value
        value: String,
    },
    /// Test configuration (validate connectivity + model availability)
    Test,
}
