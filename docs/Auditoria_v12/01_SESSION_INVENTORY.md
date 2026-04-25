# Session Inventory: NEXUS-AI-Gateway v12 Forensic Analysis

**Date:** 2026-04-25
**Scope:** 3 Claude Code sessions analyzed in Auditoria_v12
**Analyst:** Boss (Meta-Orchestrator)
**Related Analysis:** Auditoria_v11 (4 sessions)

---

## 1. Session Overview Table

| # | Session ID | Project Path | Proxy Port | Proxy Version | Duration | Peak Tokens | Key Events |
|---|------------|--------------|------------|---------------|----------|-------------|------------|
| S1 | `16ceb89d-f8b9-4046-bfc7-c0bc5e7a027c` | `/home/enerby/Github_Proyectos/Proyectos/prueba/v4` | 8316 | Hardened (WITH v11 fixes) | 2.6 hours (01:18-03:56 UTC) | 96,621 | Tool usage, agent spawning |
| S2 | `a03cd7b5-8e84-4dee-bb0f-1bc37f365d05` | `/home/enerby/Github_Proyectos/Proyectos/v5` | 8315 | Stable (WITHOUT v11 fixes) | 11.2 hours (01:26-14:39 UTC) | 141,638 | Overflow cycles, timeouts |
| S3 | `c0867608-6112-4c14-a173-878613004dc6` | `/home/enerby/Github_Proyectos/Proyectos/Proyectos_Audiovisuales/sistema` | 8315 | Stable (WITHOUT v11 fixes) | 17.2 hours (20:35-14:46 UTC) | 220,472 | Extreme overflow, 44 zero-output events |

---

## 2. Detailed Session Profiles

### Session 1 (S1): Hardened Proxy Test

| Attribute | Value |
|-----------|-------|
| Session ID | `16ceb89d-f8b9-4046-bfc7-c0bc5e7a027c` |
| JSONL File | `/home/enerby/.claude/projects/-home-enerby-Github-Proyectos-Proyectos-prueba-v4/16ceb89d-f8b9-4046-bfc7-c0bc5e7a027c.jsonl` |
| File Size | 547,233 bytes (547 KB) |
| Total Lines | ~98 exchanges |
| Proxy Port | 8316 |
| Proxy Binary | `target/release/nexus-ai-gateway` (built 2026-04-24) |
| Git Commit | e656778 (v11 fixes included) |
| Model | claude-opus-4-6 -> z-ai/glm5 (202K context) |
| Start Time | 2026-04-24 01:18:32 UTC |
| End Time | 2026-04-24 03:56:53 UTC |
| Duration | 2 hours 38 minutes (158 minutes) |
| Peak Input Tokens | 96,621 |
| Average Input Tokens | 59,220 |

#### Key Events

| Timestamp | Event | Details |
|-----------|-------|---------|
| 01:18:32 | Session start | ~45K initial tokens |
| 01:31:02 | First >50K | 52,292 tokens, correlates with TaskUpdate errors |
| 01:31:27-01:33:37 | TaskUpdate error cluster | 14 InputValidationError (missing `taskId`) |
| 02:41:15 | Later error | Additional TaskUpdate failure |
| 03:44:57 | Peak errors | 65,209 tokens during task operations |
| 03:47:31 | High usage | 70,577 tokens during synthesis |
| 03:56:53 | Session end | 96,621 tokens (session maximum) |

#### Tool Usage Distribution

| Stop Reason | Count | Percentage |
|-------------|-------|------------|
| `tool_use` | 51 | 74% |
| `end_turn` | 9 | 13% |
| `stop_sequence` | 2 | 3% |
| None/synthetic | 36 | N/A |

---

### Session 2 (S2): Stable Proxy - Extended Run

| Attribute | Value |
|-----------|-------|
| Session ID | `a03cd7b5-8e84-4dee-bb0f-1bc37f365d05` |
| JSONL File | `/home/enerby/.claude/projects/-home-enerby-Github-Proyectos-Proyectos-v5/a03cd7b5-8e84-4dee-bb0f-1bc37f365d05.jsonl` |
| File Size | 1,079,464 bytes (1.08 MB) |
| Total Lines | ~100 exchanges |
| Proxy Port | 8315 |
| Proxy Binary | `~/.cargo/bin/nexus-ai-gateway` (built 2026-04-23) |
| Git Commit | Pre-e656778 (v11 fixes NOT included) |
| Model | claude-opus-4-6 -> z-ai/glm5 (202K context) |
| Start Time | 2026-04-24 01:26 UTC |
| End Time | 2026-04-24 14:39 UTC |
| Duration | 13 hours 13 minutes (793 minutes) |
| Peak Input Tokens | 141,638 |
| Overflow Events | Multiple at 138K-142K |

#### Key Events

| Timestamp | Event | Details |
|-----------|-------|---------|
| 01:26 | Session start | ~45K initial tokens |
| 03:26:42 | First >50K | 50,595 tokens |
| 03:34:13 | First >60K | 61,041 tokens |
| 06:10:50 | Peak large output | 79,476 input tokens, 8,306 output tokens |
| 06:15-07:01 | **Overflow loop** | 10 identical overflow cycles (CRITICAL) |
| 08:19:26 | Write error | InputValidationError, missing `file_path` |
| 10:49:18 | Write error | Second occurrence |
| 10:52:48 | Write error | Third occurrence |
| 14:39 | Session end | ~89,500 tokens |

#### Overflow Cycle Timeline (10 identical cycles)

| # | Timestamp | Input Tokens | Max Tokens | Clamped To | Duration |
|---|-----------|--------------|------------|------------|----------|
| 1 | 06:15:23 | 139,729 | 65,536 | 31,383 | 45.2s |
| 2 | 06:20:11 | 139,729 | 65,536 | 31,383 | 42.8s |
| 3 | 06:25:44 | 139,729 | 65,536 | 31,383 | 47.1s |
| 4 | 06:30:52 | 139,729 | 65,536 | 31,383 | 44.5s |
| 5 | 06:35:18 | 139,729 | 65,536 | 31,383 | 46.9s |
| 6 | 06:40:33 | 139,729 | 65,536 | 31,383 | 43.3s |
| 7 | 06:45:09 | 139,729 | 65,536 | 31,383 | 45.7s |
| 8 | 06:50:27 | 139,729 | 65,536 | 31,383 | 44.2s |
| 9 | 06:56:14 | 139,729 | 65,536 | 31,383 | 46.1s |
| 10 | 07:01:58 | 139,729 | 65,536 | 31,383 | 45.5s |

**Total Duration:** 46 minutes of identical overflow behavior
**Impact:** ~50 minutes wasted compute, zero progress

---

### Session 3 (S3): Real Project - Extreme Overflow

| Attribute | Value |
|-----------|-------|
| Session ID | `c0867608-6112-4c14-a173-878613004dc6` |
| JSONL File | `/home/enerby/.claude/projects/-home-enerby-Github-Proyectos-Proyectos-Proyectos-Audiovisuales-sistema/c0867608-6112-4c14-a173-878613004dc6.jsonl` |
| File Size | 3,600,453 bytes (3.6 MB) |
| Total Lines | 265 exchanges |
| Proxy Port | 8315 |
| Proxy Binary | `~/.cargo/bin/nexus-ai-gateway` (built 2026-04-23) |
| Git Commit | Pre-e656778 (v11 fixes NOT included) |
| Model | claude-opus-4-6 -> z-ai/glm5 (202K context) |
| Secondary Model | claude-sonnet-4-6 -> moonshotai/kimi-k2.5 (256K context) |
| Start Time | 2026-04-24 20:35:40 UTC |
| End Time | 2026-04-25 14:49:33 UTC |
| Duration | 17 hours 14 minutes (1,034 minutes) |
| Peak Input Tokens | 220,472 (pre-compact estimate) / 49,750 (post-compact) |
| Zero-Output Events | 44 total |
| Auto-Compact Triggers | 1 (tiktoken estimate at 220K) |

#### Key Events

| Timestamp | Event | Details |
|-----------|-------|---------|
| 20:35:40 | Session start | 48,780 tokens |
| 23:58:51 | ECONNRESET | First overflow at 65K |
| 02:12 UTC | Agent timeouts | Timeout/killed during registry scan |
| 06:09:59 | Timeout | Six-hour stall at 129K |
| 07:06:02 | Second stall | Timeout event |
| 13:36:32 | 220K estimate | Pre-request tiktoken estimate |
| 13:43:45 | 145K actual | Actual API usage (74K difference) |
| 13:46:43 | **AUTO-COMPACT** | Triggered at 167,244 preTokens |
| 13:47:02 | Post-compact | Reset to 47K baseline |
| 14:49:33 | Session end | 49,750 final tokens |

#### Zero-Output Response Classification (44 total)

| Type | Description | Count | Evidence |
|------|-------------|-------|----------|
| Type A | Streaming intermediate | 35 | Has `content` (text/tool_use) |
| Type B | Proxy error | 6 | Has `error` field or truncated |
| Type C | CC retry attempt | 3 | Same `input_tokens` as previous |

---

## 3. Session Comparison Matrix

| Metric | Session 1 | Session 2 | Session 3 |
|--------|-----------|-----------|-------------|
| **Proxy Port** | 8316 | 8315 | 8315 |
| **v11 Fixes** | Yes | No | No |
| **Session Type** | Test | Test | Real Project |
| **Duration** | 2.6 hours | 11.2 hours | 17.2 hours |
| **File Size** | 547 KB | 1.08 MB | 3.6 MB |
| **Max Tokens** | 96,621 | 141,638 | 220,472 |
| **Overflow Events** | 0 | 10 cycles | Multiple |
| **Zero-Output** | 0 | ~2 | 44 |
| **Auto-Compact** | N/A (low tokens) | N/A (below threshold) | 1 at 167K |
| **Tool Errors** | 14 TaskUpdate | 3 Write | Varies |
| **Primary Error** | Missing `taskId` | Missing `file_path` | Multiple types |

---

## 4. Proxy Instance Inventory

### Two Simultaneous Proxy Instances

| Attribute | Port 8315 (Stable) | Port 8316 (Hardened) |
|-----------|:-------------------|:---------------------|
| **Binary Path** | `~/.cargo/bin/nexus-ai-gateway` | `target/release/nexus-ai-gateway` |
| **Build Date** | 2026-04-23 | 2026-04-24 |
| **Binary Size** | ~7.5 MB | ~8.2 MB |
| **Git Commit** | Pre-e656778 | e656778 (v11 fixes) |
| **v11 FIX 1** (scale_tokens) | **NOT included** | Included |
| **v11 FIX 2** (ContextOverflow) | **NOT included** | Included |
| **v11 FIX 3** (stream timeout) | **NOT included** | Included |
| **Sessions Served** | S2, S3 | S1 |
| **Peak Input Tokens** | 220,472 (S3) | 96,621 (S1) |
| **Kubernetes Pod** | No | No |
| **Service Unit** | No | No |
| **Auto-Restart** | No | No |
| **Primary Use** | Production traffic | Testing/validation |

### Critical Gap

**v11 fixes committed to git (e656778) but NOT deployed to port 8315.** The production traffic serving S2 and S3 ran the OLD binary without:
- FIX 1: `scale_tokens()` for high-context models
- FIX 2: ContextOverflow post-retry check
- FIX 3: Stream timeout error events

**Result:** S2 and S3 experienced all bugs identified in Auditoria_v11.

---

## 5. Proxy Log File Metadata

| Property | Value |
|----------|-------|
| **File Path** | `/tmp/nexus-ai-gateway.log` |
| **File Size** | 10,230,437 bytes (9.8 MB / 9.1 MiB reported) |
| **Line Count** | 118,161 lines |
| **Coverage Period** | 44 hours, 13 minutes |
| **Start Time** | 2026-04-24 00:00:01 UTC |
| **End Time** | 2026-04-25 20:13:44 UTC |
| **Log Format** | tracing-subscriber with timestamps, levels, modules |
| **Analyzed Sessions** | S1 (via port 8316), S2, S3 (via port 8315) |

### Log Coverage

```
2026-04-24 00:00:01 ────────────────────────────────────── Start
           │
           ├── S1 active (port 8316): 01:18 - 03:56
           │   └── 96,621 peak tokens
           │
           ├── S2 active (port 8315): 01:26 - 14:39
           │   └── 141,638 peak tokens, overflow loop 06:15-07:01
           │
           ├── S1 ends: 03:56
           │
           ├── S2 continues
           │   └── Overflow cycles at 138K-142K
           │
           └── S3 starts: 20:35
               └── 220,472 peak tokens

2026-04-25 14:49:33 ─── S3 ends
           │
           └── Log ends: 20:13:44
```

---

## 6. JSONL File Paths for Reproducibility

```bash
# Session 1 (Port 8316, hardened)
/home/enerby/.claude/projects/-home-enerby-Github-Proyectos-Proyectos-prueba-v4/16ceb89d-f8b9-4046-bfc7-c0bc5e7a027c.jsonl

# Session 2 (Port 8315, stable)
/home/enerby/.claude/projects/-home-enerby-Github-Proyectos-Proyectos-v5/a03cd7b5-8e84-4dee-bb0f-1bc37f365d05.jsonl

# Session 3 (Port 8315, stable - real project)
/home/enerby/.claude/projects/-home-enerby-Github-Proyectos-Proyectos-Proyectos-Audiovisuales-sistema/c0867608-6112-4c14-a173-878613004dc6.jsonl

# Proxy Logs
/tmp/nexus-ai-gateway.log
```

### File Size Summary

| File | Size | Lines | Tokens/Evidence |
|------|------|-------|-----------------|
| S1 JSONL | 547,233 bytes | ~98 | 96,621 peak |
| S2 JSONL | 1,079,464 bytes | ~100 | 141,638 peak, 10 overflow cycles |
| S3 JSONL | 3,600,453 bytes | 265 | 220,472 peak, 44 zero-output events |
| Proxy Log | 10,230,437 bytes | 118,161 | Complete trace across all sessions |

---

## 7. Key Findings by Session

### Session 1: Insufficient Test Coverage

- **v11 Fix Status:** Deployed (port 8316)
- **Peak Tokens:** Only 96,621 (48% of 200K context)
- **Validation Gap:** Insufficient to test FIX 1 (scale_tokens) and FIX 2 (ContextOverflow)
- **Kimi Testing:** Never tested (256K context model entirely untested)
- **Conclusion:** Session 1 validated basic functionality but did NOT validate v11 fixes at high token levels

### Session 2: Overflow Loop Discovery Site

- **v11 Fix Status:** NOT deployed
- **Peak Tokens:** 141,638 (71% of 200K / 70% of 202K context)
- **Major Finding:** First observation of "Overflow Loop Pattern" at ~139K tokens
- **10 Identical Cycles:** 46 minutes of wasted compute
- **Conclusion:** S2 is where the overflow loop pattern was first quantified

### Session 3: Full Range Testing

- **v11 Fix Status:** NOT deployed
- **Peak Tokens:** 220,472 estimate (CC threshold trigger)
- **Major Findings:**
  - 44 zero-output responses (new classification)
  - ECONNRESET error type identified
  - DeepSeek overflow at 150K tokens (cross-model issue)
  - Auto-compact triggered ONE time by tiktoken estimate
- **Conclusion:** S3 provided the full range of failure modes

---

## 8. Validation Status Summary

| v11 Fix | Session 1 (Port 8316) | Session 2 (Port 8315) | Session 3 (Port 8315) |
|:--------|:--------------------|:--------------------|:----------------------|
| FIX 1: scale_tokens | 96K insufficient | **142K would test** (FIX NOT deployed) | 220K would test (FIX NOT deployed) |
| FIX 2: ContextOverflow | Below 90% threshold | **Would fire at 142K** (FIX NOT deployed) | Would fire at 198K (FIX NOT deployed) |
| FIX 3: Stream timeout | No timeouts observed | 6 timeouts (FIX NOT deployed) | Timeouts observed (FIX NOT deployed) |

**Critical Gap:** No session tested v11 fixes at sufficient token levels (140K+) WHILE the fixes were deployed.

---

## 9. References

- See `02_SESSION3_DEEP_ANALYSIS.md` for Session 3 detailed forensics
- See `04_PROXY_LOG_FORENSICS.md` for log analysis
- See `05_NEW_FINDINGS_AND_FIXES.md` for fix implementation plan
- See `06_CROSS_V11_VALIDATION.md` for v11 findings cross-reference

---

*Document generated by Writer Agent*
*Last Updated: 2026-04-25*
*Status: Complete*
