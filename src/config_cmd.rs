use crate::cli::ConfigAction;
use anyhow::Result;

/// Handle `nexus-ai-gateway config <action>` subcommands
pub fn handle_config(action: ConfigAction) -> Result<()> {
    match action {
        ConfigAction::Show => config_show(),
        ConfigAction::Set { key, value } => config_set(&key, &value),
        ConfigAction::Test => config_test(),
    }
}

fn config_show() -> Result<()> {
    eprintln!("⚠️  config show: not yet implemented (Phase 4)");
    Ok(())
}

fn config_set(key: &str, value: &str) -> Result<()> {
    eprintln!(
        "⚠️  config set: not yet implemented (Phase 4) — {} = {}",
        key, value
    );
    Ok(())
}

fn config_test() -> Result<()> {
    eprintln!("⚠️  config test: not yet implemented (Phase 4)");
    Ok(())
}
