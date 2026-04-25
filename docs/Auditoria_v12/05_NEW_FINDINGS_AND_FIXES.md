# NEW FINDINGS AND FIXES IMPLEMENTATION PLAN (Auditoria v12)

**Date:** 2026-04-25
**Scope:** New findings from forensic analysis of Sessions 2-4 (Apr 24-25, 2026 UTC)
**Status:** Implementation Ready

---

## EXECUTIVE SUMMARY

This document captures **NEW findings** from the v12 forensic analysis that were **NOT identified in Auditoria_v11**, plus an updated implementation plan consolidating both v11 hardening and new v12 fixes.

**Critical Discovery:** A previously unidentified "Overflow Loop Pattern" causes Claude Code to spin indefinitely at ~139K tokens with GLM5, never triggering auto-compact because the 95% threshold is never reached.

---

## PART 1: NEW FINDINGS (Not in v11)

### C2: Overflow Loop Pattern (CRITICAL)

**Severity:** CRITICAL
**Component:** Proxy Streaming Handler (`src/proxy/streaming.rs`)
**First Seen:** Session 2, 06:15-07:01 UTC (10 cycles)

#### Description

When CC's input_tokens reach approximately 139K with GLM5, the system enters an infinite retry loop:

1. CC sends request with 139K tokens + 64K max_tokens = 203K total
2. Proxy pre-check detects overflow, clamps max_tokens to ~62K
3. Upstream (NIM) still returns 400, proxy extracts safe_max_tokens and clamps to ~31K
4. Request succeeds with 31K output instead of requested 64K
5. CC receives response and adds ~3K tokens (tool_use + thinking) to context
6. Next request: same ~139K input -> same overflow -> same clamp -> repeat
7. **CC NEVER auto-compacts** because 139K/200K = 69.5% (below 95% threshold)
8. Loop continues indefinitely until user manually runs `/compact`

#### Evidence

| Timestamp | Input Tokens | Max Tokens | Clamped To | Duration |
|-----------|-------------|------------|------------|----------|
| 06:15:01 | 139,522 | 65,536 | 30,976 | 45.2s |
| 06:21:12 | 139,751 | 65,536 | 30,976 | 42.8s |
| 06:28:44 | 139,984 | 65,536 | 30,976 | 47.1s |
| 06:35:22 | 140,215 | 65,536 | 30,976 | 44.5s |
| 06:42:18 | 140,448 | 65,536 | 30,976 | 46.9s |
| 06:48:57 | 140,681 | 65,536 | 30,976 | 43.3s |
| 06:55:33 | 140,912 | 65,536 | 30,976 | 45.7s |
| 07:01:45 | 141,143 | 65,536 | 30,976 | 44.2s |
| ... | ... | ... | ... | ... |

**Pattern:** 10 consecutive identical overflow cycles over 46 minutes. Token count growing by ~230 tokens/cycle (not shrinking).

#### Root Cause

CC's auto-compact uses a 95% threshold (190K/200K). At 139K tokens, CC is only at 69.5% utilization. Even with high-context scaling (200K context), the overflow happens at ~64K max_tokens, not because total tokens hit the limit.

The proxy "fixes" the overflow by clamping, but CC sees a "successful" response and doesn't know to compact.

---

### H1: Zero-Output Response Classification (HIGH)

**Severity:** HIGH
**Component:** Proxy/Client Boundary
**Evidence:** 44 responses in Session 3 with `output_tokens=0, stop_reason=?`

#### Description

Zero-output responses are **not all errors**. They fall into three distinct categories:

| Type | Description | Response Body | Actual Meaning |
|------|-------------|---------------|----------------|
| **A** | Streaming intermediate | Has `content` (text/tool_use) | Normal streaming partial |
| **B** | Proxy error | Has `error` field or truncated | Connection reset, timeout, upstream failure |
| **C** | CC retry attempt | Same `input_tokens` as previous CC-sent request | CC manually retrying after perceived failure |

#### Key Insight

Type C indicates CC detected something wrong (likely stream interruption) and resent the same payload. The 44 instances included many Type C (same input_tokens as previous), meaning they were **retries, not upstream errors**.

---

### H2/H3: New Error Types (HIGH)

**Severity:** HIGH
**Component:** Proxy Connection Handling
**First Seen:** Session 3

#### New Error Patterns

| Error | Source | Description | Count (Session 3) |
|-------|--------|-------------|-------------------|
| "Request timed out" | CC (client-side) | CC internal timeout, NOT proxy stream timeout | 12 |
| "ECONNRESET" | Proxy (reqwest/hyper) | Connection reset by upstream NIM | 8 |
| "stream chunk timeout" | Proxy (120s gap) | SSE stream with 120s+ gap between chunks | 6 |

**Critical Distinction:**

- **CC "Request timed out"** (12 instances): These are synthetic responses from Claude Code's internal HTTP client, not from our proxy. They indicate the proxy took too long to respond (or CC's client timeout is too aggressive).
- **Proxy "stream chunk timeout"** (6 instances): These appear in the proxy logs as warnings with 120000ms gaps in SSE stream. These are genuine upstream stalls.
- **ECONNRESET** (8 instances): Connection forcibly closed by NIM side, typically during high-load periods.

#### Impact

These errors were previously conflated. FIX 3 (Session timeout logging) needs to distinguish them:

1. Log when SSE stream hasn't sent a chunk in 60s/90s/120s
2. Log when upstream disconnects mid-stream (ECONNRESET)
3. Log when CC client times out (connection level, not HTTP 408)

---

### H6: DeepSeek 150K Overflow (HIGH)

**Severity:** HIGH
**Component:** Token Estimation (`src/tokenizer.rs`)
**Evidence:** Session 4, DeepSeek session hit 150K tokens

#### Description

CC reached 150K tokens with DeepSeek (128K context). The `scale_tokens()` function works for DeepSeek (1.56x inflation factor), but CC still reached 150K. This indicates:

1. **tiktoken estimate was too low** — didn't predict overflow accurately
2. **Auto-compact triggered too late** — CC only compacts when *it* thinks tokens are high
3. **Token accumulation was faster than expected** — Tool results + thinking accumulated rapidly

#### Calculation

At 150K real tokens with 1.56x inflation:
- Scaled: 234K tokens
- CC context window: 200K → 128K (DeepSeek), but reports 200K
- Effective overflow: 150K of 128K = 117% of actual limit

**Implication:** `scale_tokens()` prevents some overflows, but CC's token counting is still the primary trigger. The proxy must be the safety net.

---

### M1/M2: 429/502 Error Quantification (MEDIUM)

**Severity:** MEDIUM
**Component:** Rate Limiting, Upstream Health
**Evidence:** Session 2 metrics

| Metric | Count | Rate (per request) |
|--------|-------|-------------------|
| 429 (Too Many Requests) | 173 | ~12% |
| 502 (Bad Gateway) | 141 | ~10% |
| Exhausted retries (all 3 failed) | 15 | ~1% |

#### Analysis

429 errors are NIM's concurrency cap (max concurrent requests per API key). 502 errors are NIM server errors (bad gateway from Nvidia's infrastructure).

15 exhausted retries means the user experienced a complete failure after 3 attempts with exponential backoff. These should be logged as **CRITICAL** proxy events.

---

## PART 2: v11 FIXES STATUS

The following fixes were committed on **e656778** but are **NOT YET DEPLOYED** to the production binary:

| Fix | Description | Status | Committed | Deployed | Binary Location |
|-----|-------------|--------|-----------|----------|-----------------|
| FIX 1 | scale_tokens for high-context models | Code written | YES (e656778) | **NO** | Old binary running |
| FIX 2 | ContextOverflow post-retry check | Code written | YES (e656778) | **NO** | Not in ~/.cargo/bin/nexus-ai-gateway |
| FIX 3 | Stream timeout error event | Code written | YES (e656778) | **NO** | ~~Expired cache~~ |
| CI fix | cargo-audit pin | Replaced | YES (93302e6) | YES | CI only |
| CI fix | Node.js 20 deprecation | Upgraded to v5 | YES (f9bb5e8) | YES | CI only |

**Action Required:** Rebuild `~/.cargo/bin/nexus-ai-gateway` with current code before testing v12 fixes.

---

## PART 3: v12 FIXES (NEW)

### FIX 4: Overflow Loop Detection (CRITICAL)

**Priority:** P0
**File:** `src/proxy/streaming.rs` (or new `src/proxy/retry/overflow_tracker.rs`)
**Depends:** None (can be deployed independently)

#### Concept

Track consecutive overflow events per model. If 3+ consecutive overflows have the same `input_tokens` (within 5% margin), force `ContextOverflow` return **regardless** of the 90% threshold. This breaks the infinite loop at ~140K tokens instead of letting it continue indefinitely.

#### Implementation

```rust
// src/proxy/retry/overflow_tracker.rs
use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use std::time::Instant;
use tracing;

/// Tracks overflow patterns per model to detect infinite loops
pub struct OverflowLoopTracker;

#[derive(Debug, Clone)]
struct OverflowTracker {
    last_input_tokens: u32,
    consecutive_count: u32,
    last_timestamp: Instant,
}

// Global singleton - initialized on first use
static OVERFLOW_LOOP_TRACKER: OnceLock<Mutex<HashMap<String, OverflowTracker>>> = OnceLock::new();

impl OverflowLoopTracker {
    /// Checks if this overflow is part of a loop
    /// Returns: true if ContextOverflow should be forced
    pub fn check_overflow_loop(model: &str, input_tokens: u32) -> bool {
        let trackers = OVERFLOW_LOOP_TRACKER
            .get_or_init(|| Mutex::new(HashMap::new()));

        let mut map = trackers.lock().unwrap();
        let tracker = map
            .entry(model.to_string())
            .or_insert(OverflowTracker {
                last_input_tokens: 0,
                consecutive_count: 0,
                last_timestamp: Instant::now(),
            });

        // Check if this overflow is "the same" as the last one (within 5%)
        if input_tokens > 0 && tracker.last_input_tokens > 0 {
            let delta = (input_tokens as i64 - tracker.last_input_tokens as i64)
                .unsigned_abs();
            let threshold = (tracker.last_input_tokens as f64 * 0.05) as u32;

            if delta <= threshold {
                // Same overflow level - increment counter
                tracker.consecutive_count += 1;

                if tracker.consecutive_count >= 3 {
                    tracing::warn!(
                        consecutive_count = tracker.consecutive_count,
                        input_tokens = input_tokens,
                        model = model,
                        "🔄 Overflow loop detected: {} consecutive overflows at ~{}K tokens for model {} — forcing ContextOverflow",
                        tracker.consecutive_count,
                        input_tokens / 1000,
                        model
                    );

                    // Reset counter after triggering to avoid spam
                    tracker.consecutive_count = 0;
                    return true; // Force ContextOverflow
                }
            } else {
                // Different token level - reset counter
                tracing::debug!(
                    "Overflow token level changed from {} to {} for model {}, resetting counter",
                    tracker.last_input_tokens, input_tokens, model
                );
                tracker.consecutive_count = 1;
            }
        } else {
            tracker.consecutive_count = 1;
        }

        tracker.last_input_tokens = input_tokens;
        tracker.last_timestamp = Instant::now();
        false
    }

    /// Reset tracker for a model (e.g., after successful non-overflow request)
    pub fn reset_tracker(model: &str) {
        let trackers = OVERFLOW_LOOP_TRACKER
            .get_or_init(|| Mutex::new(HashMap::new()));
        let mut map = trackers.lock().unwrap();

        if let Some(tracker) = map.get_mut(model) {
            if tracker.consecutive_count > 0 {
                tracing::debug!(
                    "Resetting overflow tracker for {} after {} consecutive overflows",
                    model, tracker.consecutive_count
                );
            }
            tracker.consecutive_count = 0;
            tracker.last_input_tokens = 0;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_overflow_loop_detection() {
        // First overflow - no loop
        assert!(!OverflowLoopTracker::check_overflow_loop("glm5", 139000));

        // Same tokens - count=1
        assert!(!OverflowLoopTracker::check_overflow_loop("glm5", 139000));

        // Same tokens - count=2
        assert!(!OverflowLoopTracker::check_overflow_loop("glm5", 139100));

        // Same tokens - count=3 → LOOP DETECTED
        assert!(OverflowLoopTracker::check_overflow_loop("glm5", 139000));

        // After detection, counter reset
        assert!(!OverflowLoopTracker::check_overflow_loop("glm5", 139000));
    }

    #[test]
    fn test_different_models_tracked_separately() {
        OverflowLoopTracker::check_overflow_loop("glm5", 139000);
        OverflowLoopTracker::check_overflow_loop("glm5", 139000);

        // Different model should not trigger
        assert!(!OverflowLoopTracker::check_overflow_loop("other-model", 50000));
    }
}
```

#### Integration

In `src/proxy/streaming.rs`, modify the overflow handling logic:

```rust
// Around line 127-140 in current code
if overflow {
    // Check for loop pattern BEFORE attempting retry
    let would_loop = OverflowLoopTracker::check_overflow_loop(
        &transform_request.model,
        input_tokens
    );

    if would_loop {
        // Force ContextOverflow instead of retrying
        return Err(AppError::ContextOverflow {
            model: transform_request.model.clone(),
            input_tokens,
            requested_output: request_body.max_tokens,
            context_limit: cc_context_window,
            message: format!(
                "Infinite overflow loop detected: {} consecutive overflows at ~{}K tokens",
                3, input_tokens / 1000
            ),
        }.into());
    }

    // Original overflow handling continues...
}
```

**Success Criteria:**
- Loop detected after 3 consecutive overflows within 5% token variation
- Forced ContextOverflow returned with clear message
- CC auto-compacts (or user is notified to run `/compact`)

---

### FIX 5: Lower ContextOverflow Threshold (HIGH)

**Priority:** P1
**File:** `src/proxy/streaming.rs`
**Type:** Configuration enhancement + safety improvement
**Depends:** FIX 2 (v11) - modifies the same check

#### Current (v11 FIX 2)

```rust
// src/proxy/streaming.rs:~127
let context_threshold = cc_context_window * 90 / 100; // 180K for 200K window
```

#### Proposed (v12 FIX 5)

```rust
// src/proxy/streaming.rs
use std::env;

/// Returns the context overflow threshold percentage (default: 80%)
fn get_overflow_threshold_pct() -> u32 {
    env::var("CC_OVERFLOW_THRESHOLD_PCT")
        .ok()
        .and_then(|v| v.parse().ok())
        .filter(|&pct| pct >= 50 && pct <= 95) // Sanity check
        .unwrap_or(80)
}

// Usage:
let context_threshold_pct = get_overflow_threshold_pct();
let context_threshold = cc_context_window * context_threshold_pct / 100;
```

#### Rationale

At 139K real tokens with GLM5:
- 1.1x scaling (from FIX 1) = 153K scaled tokens
- 153K/200K = 76.5% (below current 90% threshold)

At 145K real tokens:
- 1.1x scaling = 159.5K scaled = ~80%

**Result:** ContextOverflow fires at ~145K real tokens instead of ~172K (90% threshold).

This provides a **buffer zone** before the proxy overflow loop starts, giving CC time to auto-compact naturally.

#### Configuration

| Threshold | Real Tokens (1.1x) | Real Tokens (1.56x) | Use Case |
|-----------|-------------------|---------------------|----------|
| 90% (old) | 164K | 115K | Maximum compatibility |
| 80% (new default) | 145K | 103K | Safe for GLM5 |
| 70% | 127K | 90K | Aggressive early warning |

Set via environment: `CC_OVERFLOW_THRESHOLD_PCT=85`

---

### FIX 6: Non-Streaming scale_tokens (MEDIUM)

**Priority:** P2
**File:** `src/proxy/non_streaming.rs`
**Type:** Gap fix (v11 FIX 2 missed this path)
**Depends:** FIX 1 (scale_tokens function)

#### Current (v11 FIX 2 - Incomplete)

```rust
// src/proxy/non_streaming.rs
if let Some(ref usage) = anthropic_resp.usage {
    let input_tokens = usage.input_tokens as u32;

    // BUG: Using raw input_tokens, not scaled!
    if input_tokens > context_threshold {
        tracing::info!(
            "BEFORE RETRY: ContextOverflow detected for {}",
            transform_request.model
        );
    }
}
```

#### Proposed (v12 FIX 6)

```rust
// src/proxy/non_streaming.rs
use crate::tokenizer::scale_tokens;

if let Some(ref usage) = anthropic_resp.usage {
    let input_tokens = usage.input_tokens as u32;
    let scaled_input_tokens = scale_tokens(input_tokens); // FIX 6: Apply scaling

    // FIX 6: Use scaled tokens for threshold comparison
    if scaled_input_tokens > context_threshold {
        tracing::info!(
            model = %transform_request.model,
            input_tokens = input_tokens,
            scaled_tokens = scaled_input_tokens,
            threshold = context_threshold,
            "Non-streaming: ContextOverflow detected (scaled)"
        );

        // Return ContextOverflow error
        return Err(AppError::ContextOverflow {
            model: transform_request.model.clone(),
            input_tokens: scaled_input_tokens, // Report scaled to CC
            requested_output: request_body.max_tokens,
            context_limit: cc_context_window,
            message: "Context window exceeded (scaled)".to_string(),
        }.into());
    }
}
```

**Impact:** Non-streaming responses now receive the same token scaling protection as streaming.

---

## PART 4: IMPLEMENTATION ORDER

### Phase 1: Deploy v11 Fixes (Required First)

1. **Verify cargo version** and environment
   ```bash
   rustc --version  # Should be 1.75+
   cargo --version  # Should be 1.75+
   ```

2. **Rebuild production binary**
   ```bash
   cd /path/to/nexus-ai-gateway
   cargo build --release
   ```

3. **Verify version sync**
   ```bash
   task version-check
   ```

4. **Deploy to ~/.cargo/bin**
   ```bash
   cp target/release/nexus-ai-gateway ~/.cargo/bin/nexus-ai-gateway
   chmod +x ~/.cargo/bin/nexus-ai-gateway
   ```

5. **Verify deployment**
   ```bash
   nexus-ai-gateway --version
   file ~/.cargo/bin/nexus-ai-gateway
   ls -la ~/.cargo/bin/nexus-ai-gateway
   ```

### Phase 2: Implement v12 Fixes

**Order matters** for testing convenience:

| Step | Fix | File(s) | Lines Added | Test Strategy |
|------|-----|---------|-------------|---------------|
| 1 | FIX 6 | non_streaming.rs | ~10 | Unit test scale_tokens integration |
| 2 | FIX 5 | streaming.rs | ~20 | Env var config test |
| 3 | FIX 4 | overflow_tracker.rs (new) | ~120 + unit tests | Mock overflow loop scenario |
| 4 | Integration | streaming.rs | ~15 | E2E test with overflow simulation |

#### Detailed Steps

**Step 1: FIX 6 (Non-streaming scale_tokens)**
```bash
# Create branch
git checkout -b fix/v12-non-streaming-scale

# Edit src/proxy/non_streaming.rs
# … apply changes above …

# Test
cargo test non_streaming -- --nocapture
```

**Step 2: FIX 5 (Lower threshold)**
```bash
git checkout -b fix/v12-threshold-config

# Edit src/proxy/streaming.rs
# Add env::var handling
# Update context_threshold calculation

# Test
cargo test tokenizer -- --nocapture
CC_OVERFLOW_THRESHOLD_PCT=75 cargo test -- --nocapture
```

**Step 3: FIX 4 (Loop detector)**
```bash
git checkout -b fix/v12-overflow-loop

# Create src/proxy/retry/overflow_tracker.rs
# Add module declaration in src/lib.rs or src/proxy/mod.rs

# Test
cargo test -- --nocapture  # All existing tests must pass
```

**Step 4: Integration**
```bash
# Merge all branches
git checkout feature/v12-fixes
git merge fix/v12-non-streaming-scale
git merge fix/v12-threshold-config
git merge fix/v12-overflow-loop

# Full test suite
cargo test --all-features
```

**Step 5: Build and Deploy**
```bash
cargo build --release
task version-check
cp target/release/nexus-ai-gateway ~/.cargo/bin/nexus-ai-gateway
```

### Phase 3: Validation

1. **Port 8315 health check**
   ```bash
   curl http://localhost:8315/health
   # Expected: OK
   ```

2. **Test overflow loop scenario** (manual)
   - Start CC session with GLM5
   - Build context to ~140K tokens
   - Verify loop detection triggers after 3 overflows
   - Verify CC receives ContextOverflow

3. **Verify threshold config**
   ```bash
   CC_OVERFLOW_THRESHOLD_PCT=85 nexus-ai-gateway
   # Check logs: "Using custom overflow threshold: 85%"
   ```

---

## PART 5: TEST STRATEGY

### Unit Tests

| Fix | Test Module | Test Cases |
|-----|-------------|------------|
| FIX 4 | overflow_tracker.rs | test_overflow_loop_detection, test_different_models_tracked_separately, test_reset_after_success |
| FIX 5 | streaming.rs | test_threshold_config_parsing, test_threshold_sanity_checks |
| FIX 6 | non_streaming.rs | test_scale_tokens_integration |

### Integration Tests

**Test: Overflow Loop Scenario**
```rust
#[tokio::test]
async fn test_overflow_loop_detection_integration() {
    // Mock request that triggers overflow 3 times
    let mut last_response = None;

    for i in 0..4 {
        let resp = send_mock_request_with_tokens(139000 + i * 100).await;

        if i < 3 {
            assert!(matches!(resp, RetryResult::RetryWithClampedTokens));
        } else {
            // 4th request should trigger ContextOverflow
            assert!(matches!(resp, RetryResult::ContextOverflow));
        }

        last_response = Some(resp);
    }
}
```

### E2E Validation

**Manual Testing Procedure:**

1. **Setup**
   ```bash
   # Terminal 1: Start proxy with debug logging
   RUST_LOG=debug nexus-ai-gateway

   # Terminal 2: Monitor logs
   tail -f ~/.local/share/nexus-ai-gateway/logs/proxy.log
   ```

2. **CC Session**
   ```bash
   # Start Claude Code with GLM5
   claude config set -g model glm5
   cd /path/to/large-repo
   claude
   ```

3. **Build Context**
   - Ask CC to read multiple large files
   - Monitor token count in CC status bar
   - Wait until ~140K tokens

4. **Trigger Loop**
   - Ask for a large response (e.g., "refactor this entire module")
   - Observe proxy logs for "Overflow loop detected"
   - Verify CC receives "context window exceeded" error

5. **Success Criteria**
   - [ ] Loop detected within 3 overflow cycles
   - [ ] ContextOverflow returned to CC
   - [ ] CC stops retrying (no infinite loop)
   - [ ] User can successfully run `/compact` and continue

---

## PART 6: ROLLBACK PLAN

If v12 fixes cause issues:

1. **Immediate**
   ```bash
   # Kill running proxy
   pkill nexus-ai-gateway

   # Restore v11 binary (if backed up)
   cp ~/.cargo/bin/nexus-ai-gateway.bak.v11 ~/.cargo/bin/nexus-ai-gateway
   ```

2. **Configuration override**
   ```bash
   # Disable FIX 5 (threshold change)
   export CC_OVERFLOW_THRESHOLD_PCT=90  # Restore v11 behavior

   # Restart proxy
   nexus-ai-gateway
   ```

3. **Git rollback**
   ```bash
   git checkout main
   git branch -D feature/v12-fixes
   cargo build --release
   cp target/release/nexus-ai-gateway ~/.cargo/bin/
   ```

---

## SUMMARY

### New v12 Findings
| ID | Severity | Description | Root Cause |
|----|----------|-------------|------------|
| C2 | CRITICAL | Overflow Loop Pattern at ~139K tokens | CC compact threshold (95%) never reached |
| H1 | HIGH | Zero-output responses misclassified | Three distinct types not distinguished |
| H2/H3 | HIGH | New error types (CC timeout, ECONNRESET) | Connection vs stream timeouts conflated |
| H6 | HIGH | DeepSeek 150K overflow | scale_tokens insufficient at extreme volumes |
| M1/M2 | MEDIUM | 429/502 rates quantified | NIM concurrency limits and server errors |

### v12 Fixes
| Fix | Priority | Description | Files |
|-----|----------|-------------|-------|
| FIX 4 | P0 | Overflow loop detector | overflow_tracker.rs (new), streaming.rs |
| FIX 5 | P1 | Configurable threshold (default 80%) | streaming.rs |
| FIX 6 | P2 | Non-streaming scale_tokens | non_streaming.rs |

### Deployment Status
- **v11 fixes:** Code written, NOT deployed → Deploy first
- **v12 fixes:** Implementation ready → Deploy after v11
- **Target:** All fixes in `~/.cargo/bin/nexus-ai-gateway` by EOD

### Success Metrics
- [ ] No infinite loops at 139K tokens (FIX 4)
- [ ] ContextOverflow triggers at ~145K tokens (FIX 5)
- [ ] Non-streaming path uses scaled tokens (FIX 6)
- [ ] All 32 unit tests pass + 3 new v12 tests
- [ ] E2E validation with GLM5 successful

---

**Document:** `/home/enerby/Github_Proyectos/MyCode/Rust_Proyects/NEXUS-AI-Gateway/docs/Auditoria_v12/05_NEW_FINDINGS_AND_FIXES.md`
**Last Updated:** 2026-04-25 14:00 UTC
**Author:** Security Auditor / SRE Team
