use anyhow::Result;

/// Run the interactive setup wizard
pub fn run_setup(quick: bool) -> Result<()> {
    if quick {
        eprintln!("⚠️  setup --quick: not yet implemented (Phase 3)");
    } else {
        eprintln!("⚠️  setup: not yet implemented (Phase 3)");
    }
    Ok(())
}
