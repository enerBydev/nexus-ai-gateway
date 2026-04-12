//! Integration tests for NEXUS-AI-Gateway
//!
//! These tests verify core functionality without requiring
//! a running upstream server.

use nexus_ai_gateway::VERSION;

#[test]
fn version_is_valid_semver() {
    let parts: Vec<&str> = VERSION.split('.').collect();
    assert_eq!(parts.len(), 3, "VERSION must be X.Y.Z format");
    for part in &parts {
        part.parse::<u32>()
            .unwrap_or_else(|_| panic!("VERSION component '{}' is not a valid number", part));
    }
}

#[test]
fn version_matches_cargo_toml() {
    let cargo_version = env!("CARGO_PKG_VERSION");
    assert_eq!(
        VERSION, cargo_version,
        "lib.rs VERSION ({}) must match Cargo.toml version ({})",
        VERSION, cargo_version
    );
}

#[test]
fn version_file_matches() {
    let version_file = include_str!("../VERSION").trim();
    assert_eq!(
        VERSION, version_file,
        "lib.rs VERSION ({}) must match VERSION file ({})",
        VERSION, version_file
    );
}
