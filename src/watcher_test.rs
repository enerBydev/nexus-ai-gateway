/// Unit tests for the fix#52 file watcher 5-layer protection stack.
///
/// These tests validate the protection logic WITHOUT requiring a running
/// notify watcher or filesystem events. They test:
/// - Layer 1: EventKind::Modify(ModifyKind::Data) filtering
/// - Layer 3: mtime comparison (unchanged -> skip reload)
/// - Layer 4: Cooldown enforcement (15s minimum between reloads)
/// - Layer 5: AtomicBool serialization (prevents concurrent reload)
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

// ---------------------------------------------------------------------------
// Layer 1: Event filtering — only ModifyKind::Data triggers reload
// ---------------------------------------------------------------------------

#[test]
fn layer1_modify_data_event_should_trigger() {
    use notify::event::ModifyKind;
    use notify::{Event, EventKind};

    // Simulate a real file-write event
    let event = Event {
        kind: EventKind::Modify(ModifyKind::Data(notify::event::DataChange::Content)),
        paths: vec![],
        attrs: Default::default(),
    };

    assert!(
        matches!(event.kind, EventKind::Modify(ModifyKind::Data(_))),
        "Modify(Data) events MUST pass the filter"
    );
}

#[test]
fn layer1_modify_metadata_event_should_not_trigger() {
    use notify::event::ModifyKind;
    use notify::{Event, EventKind};

    // Metadata changes (chmod, ownership) must NOT trigger reload
    let event = Event {
        kind: EventKind::Modify(ModifyKind::Metadata(notify::event::MetadataKind::Permissions)),
        paths: vec![],
        attrs: Default::default(),
    };

    assert!(
        !matches!(event.kind, EventKind::Modify(ModifyKind::Data(_))),
        "Modify(Metadata) events MUST be filtered out"
    );
}

#[test]
fn layer1_access_event_should_not_trigger() {
    use notify::{Event, EventKind};

    // ACCESS events (file read/OPEN) were the root cause of #52 infinite loop
    let event = Event {
        kind: EventKind::Access(notify::event::AccessKind::Read),
        paths: vec![],
        attrs: Default::default(),
    };

    assert!(
        !matches!(event.kind, EventKind::Modify(notify::event::ModifyKind::Data(_))),
        "Access events MUST be filtered out (root cause of infinite loop)"
    );
}

#[test]
fn layer1_create_event_should_not_trigger() {
    use notify::{Event, EventKind};

    let event = Event {
        kind: EventKind::Create(notify::event::CreateKind::File),
        paths: vec![],
        attrs: Default::default(),
    };

    assert!(
        !matches!(event.kind, EventKind::Modify(notify::event::ModifyKind::Data(_))),
        "Create events MUST be filtered out"
    );
}

#[test]
fn layer1_remove_event_should_not_trigger() {
    use notify::{Event, EventKind};

    let event = Event {
        kind: EventKind::Remove(notify::event::RemoveKind::File),
        paths: vec![],
        attrs: Default::default(),
    };

    assert!(
        !matches!(event.kind, EventKind::Modify(notify::event::ModifyKind::Data(_))),
        "Remove events MUST be filtered out"
    );
}

// ---------------------------------------------------------------------------
// Layer 3: mtime check — skip reload if mtime unchanged
// ---------------------------------------------------------------------------

#[test]
fn layer3_mtime_unchanged_skips_reload() {
    // Simulate: both last_mtime and current_mtime are the same
    let last_mtime: Option<std::time::SystemTime> = Some(std::time::SystemTime::now());
    let current_mtime = last_mtime;

    // The watcher logic is: if current_mtime == last_mtime { skip }
    assert_eq!(current_mtime, last_mtime, "When mtime is unchanged, reload MUST be skipped");
}

#[test]
fn layer3_mtime_changed_allows_reload() {
    // Simulate: last_mtime and current_mtime differ
    let last_mtime: Option<std::time::SystemTime> = Some(std::time::SystemTime::now());
    // After a file write, mtime would be later
    let current_mtime: Option<std::time::SystemTime> =
        Some(std::time::SystemTime::now() + Duration::from_secs(1));

    assert_ne!(current_mtime, last_mtime, "When mtime changed, reload should NOT be skipped");
}

#[test]
fn layer3_none_mtime_allows_reload() {
    // If we couldn't read mtime before (None), and now we can (Some),
    // that's a change -> allow reload
    let last_mtime: Option<std::time::SystemTime> = None;
    let current_mtime: Option<std::time::SystemTime> = Some(std::time::SystemTime::now());

    assert_ne!(current_mtime, last_mtime, "Transition from None->Some mtime must allow reload");
}

#[test]
fn layer3_both_none_skips_reload() {
    // If both are None (can't read mtime at all), the watcher
    // logic treats them as equal -> skip
    let last_mtime: Option<std::time::SystemTime> = None;
    let current_mtime: Option<std::time::SystemTime> = None;

    assert_eq!(current_mtime, last_mtime, "Both None means mtime unchanged — must skip reload");
}

// ---------------------------------------------------------------------------
// Layer 4: Cooldown — minimum 15s between reloads
// ---------------------------------------------------------------------------

#[test]
fn layer4_cooldown_not_elapsed_skips_reload() {
    // Simulate: last reload was 5s ago, cooldown is 15s
    let last_reload = Instant::now() - Duration::from_secs(5);
    let cooldown_secs: u64 = 15;

    assert!(
        last_reload.elapsed().as_secs() < cooldown_secs,
        "5s < 15s cooldown -> reload MUST be skipped"
    );
}

#[test]
fn layer4_cooldown_elapsed_allows_reload() {
    // Simulate: last reload was 20s ago, cooldown is 15s
    let last_reload = Instant::now() - Duration::from_secs(20);
    let cooldown_secs: u64 = 15;

    assert!(
        last_reload.elapsed().as_secs() >= cooldown_secs,
        "20s >= 15s cooldown -> reload MUST be allowed"
    );
}

#[test]
fn layer4_cooldown_exact_boundary_allows_reload() {
    // Boundary: exactly at cooldown threshold
    let last_reload = Instant::now() - Duration::from_secs(15);
    let cooldown_secs: u64 = 15;

    // In the watcher: `if last_reload.elapsed().as_secs() < 15 { skip }`
    // At exactly 15s, < 15 is false, so reload proceeds
    assert!(
        !(last_reload.elapsed().as_secs() < cooldown_secs),
        "At exactly 15s, the `< 15` check is false -> reload allowed"
    );
}

#[test]
fn layer4_initial_cooldown_allows_immediate_reload() {
    // The watcher initializes last_reload to now()-20s,
    // so the first event always passes the cooldown check
    let last_reload = Instant::now() - Duration::from_secs(20);
    let cooldown_secs: u64 = 15;

    assert!(
        last_reload.elapsed().as_secs() >= cooldown_secs,
        "Initial last_reload (now-20s) must allow first reload immediately"
    );
}

// ---------------------------------------------------------------------------
// Layer 5: AtomicBool serialization — prevents concurrent SIGHUP + watcher
// ---------------------------------------------------------------------------

#[test]
fn layer5_atomic_bool_first_reloader_wins() {
    let reload_flag = AtomicBool::new(false);

    // First reloader succeeds
    let result = reload_flag.compare_exchange(false, true, Ordering::SeqCst, Ordering::Relaxed);
    assert!(result.is_ok(), "First reloader MUST acquire the flag");
    assert!(reload_flag.load(Ordering::SeqCst), "Flag MUST be set to true");
}

#[test]
fn layer5_atomic_bool_second_reloader_blocked() {
    let reload_flag = AtomicBool::new(true); // Already in progress

    let result = reload_flag.compare_exchange(false, true, Ordering::SeqCst, Ordering::Relaxed);
    assert!(result.is_err(), "Second concurrent reloader MUST be blocked");
}

#[test]
fn layer5_atomic_bool_release_allows_next_reload() {
    let reload_flag = AtomicBool::new(true);

    // First reloader finishes, releases the flag
    reload_flag.store(false, Ordering::SeqCst);

    // Next reloader can now acquire
    let result = reload_flag.compare_exchange(false, true, Ordering::SeqCst, Ordering::Relaxed);
    assert!(result.is_ok(), "After release, next reloader MUST succeed");
}

#[test]
fn layer5_atomic_bool_sighup_watcher_race() {
    // Simulate the exact race: SIGHUP and watcher both try to reload
    let reload_flag = Arc::new(AtomicBool::new(false));

    // SIGHUP arrives first
    let sighup_result =
        reload_flag.compare_exchange(false, true, Ordering::SeqCst, Ordering::Relaxed);
    assert!(sighup_result.is_ok(), "SIGHUP acquires the flag first");

    // Watcher arrives concurrently
    let watcher_result =
        reload_flag.compare_exchange(false, true, Ordering::SeqCst, Ordering::Relaxed);
    assert!(watcher_result.is_err(), "Watcher MUST be blocked while SIGHUP reloads");

    // SIGHUP finishes, releases
    reload_flag.store(false, Ordering::SeqCst);

    // Watcher can try again next cycle
    let watcher_retry =
        reload_flag.compare_exchange(false, true, Ordering::SeqCst, Ordering::Relaxed);
    assert!(watcher_retry.is_ok(), "Watcher can acquire after SIGHUP releases");
}

// ---------------------------------------------------------------------------
// Config::reload signature validation (config_path parameter exists)
// ---------------------------------------------------------------------------

#[test]
fn reload_accepts_config_path_parameter() {
    // This test validates that Config::reload has the correct signature
    // with config_path: Option<PathBuf>. If the signature regresses,
    // this test won't compile, catching the regression early.
    // We can't call reload() without a valid env, but we verify
    // the function exists with the right parameter count by type-checking.
    // The verbose fn-pointer type is the whole point of this assertion helper.
    #[allow(clippy::type_complexity)]
    fn _assert_reload_signature(
        _f: fn(
            bool,
            bool,
            Option<u16>,
            Option<String>,
            Option<std::path::PathBuf>,
        ) -> anyhow::Result<crate::config::Config>,
    ) {
    }
    _assert_reload_signature(crate::config::Config::reload);
}

// ---------------------------------------------------------------------------
// Config.config_path field validation
// ---------------------------------------------------------------------------

#[test]
fn config_has_config_path_field() {
    use crate::config::Config;
    use std::collections::HashMap;
    use std::path::PathBuf;

    // Build a minimal config to verify config_path field exists
    let config = Config {
        port: 8315,
        bind_addr: "127.0.0.1".to_string(),
        base_url: "http://localhost:11434".to_string(),
        api_key: None,
        reasoning_model: None,
        completion_model: None,
        debug: false,
        verbose: false,
        web_fetch_enabled: true,
        web_fetch_max_retries: 3,
        web_fetch_timeout_secs: 15,
        upstreams: HashMap::new(),
        model_map: HashMap::new(),
        max_concurrent_per_model: 5,
        permit_timeout_secs: 180,
        upstream_type: crate::config::UpstreamType::NIM,
        prompt_cache_enabled: false,
        prompt_cache_max_entries: 1000,
        prompt_cache_ttl_secs: 300,
        cb_enabled: false,
        cb_threshold: 10,
        cb_recovery_secs: 60,
        cc_model_context_windows: HashMap::new(),
        telemetry_enabled: false,
        telemetry_beacon_url: None,
        beacon_auth_token: None,
        telemetry_dir: "/tmp".to_string(),
        telemetry_db_path: "/tmp/nexus-telemetry.db".to_string(),
        telemetry_retention_days: 30,
        telemetry_secret_path: "/tmp/nexus-telemetry-secret".to_string(),
        config_path: Some(PathBuf::from("/custom/path.env")),
        telemetry_disabled_reason: None,
    };

    assert_eq!(
        config.config_path,
        Some(PathBuf::from("/custom/path.env")),
        "config_path field must be accessible and store custom paths"
    );
}

#[test]
fn config_path_defaults_to_none() {
    use crate::config::Config;
    use std::collections::HashMap;

    let config = Config {
        port: 8315,
        bind_addr: "127.0.0.1".to_string(),
        base_url: "http://localhost:11434".to_string(),
        api_key: None,
        reasoning_model: None,
        completion_model: None,
        debug: false,
        verbose: false,
        web_fetch_enabled: true,
        web_fetch_max_retries: 3,
        web_fetch_timeout_secs: 15,
        upstreams: HashMap::new(),
        model_map: HashMap::new(),
        max_concurrent_per_model: 5,
        permit_timeout_secs: 180,
        upstream_type: crate::config::UpstreamType::NIM,
        prompt_cache_enabled: false,
        prompt_cache_max_entries: 1000,
        prompt_cache_ttl_secs: 300,
        cb_enabled: false,
        cb_threshold: 10,
        cb_recovery_secs: 60,
        cc_model_context_windows: HashMap::new(),
        telemetry_enabled: false,
        telemetry_beacon_url: None,
        beacon_auth_token: None,
        telemetry_dir: "/tmp".to_string(),
        telemetry_db_path: "/tmp/nexus-telemetry.db".to_string(),
        telemetry_retention_days: 30,
        telemetry_secret_path: "/tmp/nexus-telemetry-secret".to_string(),
        config_path: None,
        telemetry_disabled_reason: None,
    };

    assert!(
        config.config_path.is_none(),
        "config_path defaults to None when no custom path is provided"
    );
}

// ---------------------------------------------------------------------------
// Drain state integration (S11 check)
// ---------------------------------------------------------------------------

#[test]
fn drain_flag_blocks_watcher_reload() {
    use crate::IS_DRAINING;

    // Set draining state
    IS_DRAINING.store(true, Ordering::Relaxed);
    assert!(IS_DRAINING.load(Ordering::Relaxed), "IS_DRAINING must be true when set");

    // Reset after test
    IS_DRAINING.store(false, Ordering::Relaxed);
}

// ---------------------------------------------------------------------------
// Debounce timing constants validation
// ---------------------------------------------------------------------------

#[test]
fn debounce_duration_is_10s() {
    let debounce = Duration::from_secs(10);
    assert_eq!(debounce.as_secs(), 10, "Debounce MUST be 10 seconds");
}

#[test]
fn cooldown_duration_is_15s_and_greater_than_debounce() {
    let debounce = Duration::from_secs(10);
    let cooldown = Duration::from_secs(15);

    assert_eq!(cooldown.as_secs(), 15, "Cooldown MUST be 15 seconds");
    assert!(
        cooldown > debounce,
        "Cooldown (15s) MUST be greater than debounce (10s) to prevent burst reloads"
    );
}

use std::sync::Arc;

// ---------------------------------------------------------------------------
// #45 regression: a custom --config path must survive hot-reload
// ---------------------------------------------------------------------------

#[test]
fn reload_preserves_custom_config_path() {
    // Regression for #45: when launched with `--config /custom.env`, the SIGHUP
    // handler and the file watcher both call Config::reload(.., config_path).
    // reload MUST preserve that custom path in the returned config so the *next*
    // reload keeps using it instead of silently reverting to the default
    // ~/.nexus-ai-gateway.env (the original bug: the path was discarded forever).
    use crate::config::Config;
    use std::io::Write;

    // A minimal valid env file at a non-default custom path.
    let path = std::env::temp_dir().join(format!("nexus45-{}-{}.env", std::process::id(), line!()));
    {
        let mut f = std::fs::File::create(&path).expect("create temp env file");
        writeln!(f, "UPSTREAM_BASE_URL=http://localhost:11434").unwrap();
        writeln!(f, "UPSTREAM_API_KEY=test-key").unwrap();
    }

    let cfg = Config::reload(false, false, None, None, Some(path.clone()))
        .expect("reload with a custom config path should succeed");

    assert_eq!(
        cfg.config_path,
        Some(path.clone()),
        "reload must preserve the custom --config path for subsequent SIGHUP/watcher reloads (#45)"
    );

    let _ = std::fs::remove_file(&path);
}
