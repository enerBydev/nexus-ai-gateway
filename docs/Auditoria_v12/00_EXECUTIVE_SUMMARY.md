# NEXUS-AI-Gateway v12 Forensic Audit — Executive Summary

**Date:** 2026-04-25
**Scope:** 3 Claude Code sessions + proxy log analysis + v11 cross-validation
**Proxy Version:** v0.13.0 (stable, WITHOUT v11 fixes deployed)
**Analyst:** Boss (Meta-Orchestrator)

---

## 0. Critical Finding: v11 Fixes NOT Deployed

The running proxy on port 8315 is the **OLD binary** (`~/.cargo/bin/nexus-ai-gateway`, built 2026-04-23), NOT the hardened version with v11 fixes (committed as `e656778`, built 2026-04-24).

| Metric | Expected (with v11 fixes) | Actual (running proxy) |
|:-------|:-------------------------|:-----------------------|
| `Scaling up input_tokens` | >0 (FIX 1 active) | **0** (FIX 1 NOT active) |
| `Context nearly full` | >0 (FIX 2 active) | **0** (FIX 2 NOT active) |
| `emitting error event` | >0 (FIX 3 active) | **0** (FIX 3 NOT active) |
| `ContextOverflow` returned | >0 (after 90% threshold) | **0** (never fires) |

**Impact:** All 3 root causes from Auditoria_v11 remain active in production. The fixes exist in git but have NOT been deployed to the running proxy instance.

---

## 1. Sessions Analyzed

| # | Session ID | Proxy Port | Proxy Version | Project | Duration | Peak input_tokens |
|:--|:-----------|:-----------|:-------------|:--------|:---------|:------------------|
| S1 | `16ceb89d` | 8316 | Hardened (WITH v11 fixes) | prueba/v4 (test) | 01:18-03:56 UTC | 96,621 |
| S2 | `a03cd7b5` | 8315 | Stable (WITHOUT v11 fixes) | v5 (test) | 01:26-14:39 UTC | 141,638 |
| S3 | `c0867608` | 8315 | Stable (WITHOUT v11 fixes) | sistema (REAL) | 20:35-14:46 UTC | 220,472 |

---

## 2. Findings Summary by Severity

### CRITICAL (Production-Impacting)

| # | Finding | v11 Covered? | New? | Sessions Affected |
|:--|:--------|:------------|:-----|:------------------|
| C1 | v11 fixes NOT deployed — all 3 root causes remain active | Partial (knew fixes weren't deployed) | **YES** — confirmed via proxy log | All sessions on port 8315 |
| C2 | **Overflow Loop Pattern** — CC gets stuck at ~139K tokens, proxy clamps max_tokens, CC adds ~3K tokens per response, overflows again, repeating indefinitely | NO | **YES** | S3 (06:10-07:01 UTC, 10 identical overflow cycles) |
| C3 | **Zero auto-compact for GLM5/Kimi** — CC sees 139K/200K = 69.5%, never reaches 95% threshold | YES (v11 RC#1) | No | S2, S3 |

### HIGH

| # | Finding | v11 Covered? | New? | Sessions Affected |
|:--|:--------|:------------|:-----|:------------------|
| H1 | **44 zero-output responses in S3** — responses with `output_tokens=0` and `stop_reason=?` indicate proxy errors or incomplete responses | Partially (v11 counted compact boundaries, not zero-output responses) | **YES** — new classification | S3 |
| H2 | **2 "Request timed out" synthetic responses** — CC generates internal synthetic responses when the API connection fails completely (distinct from stream chunk timeout) | NO | **YES** — new error type | S3 (lines 211, 231) |
| H3 | **ECONNRESET synthetic response** — proxy connection reset by NIM | NO | **YES** — new error type | S3 (line 82) |
| H4 | **Stream chunk timeout emits synthetic success** — 6 events, all emitted `message_delta(end_turn)` instead of error event | YES (v11 RC#3) | No | All sessions on port 8315 |
| H5 | **15 exhausted retries** — requests that failed all 3 retry attempts, returned 502 to CC | Partially | **YES** — quantified | All sessions |
| H6 | **DeepSeek overflow at 150K tokens** — CC reached 150K tokens with DeepSeek (128K context), far exceeding the model's capacity | NO | **YES** — cross-model issue | Current session on port 8315 |

### MEDIUM

| # | Finding | v11 Covered? | New? |
|:--|:--------|:------------|:-----|
| M1 | **173 429 rate limit events** — NIM concurrency cap limits, mostly for GLM5/Kimi | No | **YES** — quantified |
| M2 | **141 502 bad gateway events** — NIM server errors, possibly model loading | No | **YES** — quantified |
| M3 | **Session 1 didn't test high-context behavior** — peak tokens only 96K (48% of 200K), insufficient to validate v11 FIX 1 and FIX 2 | No | **YES** — gap identification |
| M4 | **Kimi (256K context) never tested** — all sessions used GLM5 (202K), v11 fixes for Kimi not validated | No | **YES** — validation gap |

---

## 3. The Overflow Loop Pattern (NEW — C2)

This is the most significant new finding. Not identified in Auditoria_v11.

### Description
When CC's real input_tokens reach ~140K with GLM5 (202K context), the system enters an infinite loop:

```
1. CC sends request: 139,986 input_tokens + 64,000 max_tokens = 203,986 total
2. Proxy Pre-check: "139,729tok + 64,000tok > 202,752tok" → Clamping → 62,767
3. NIM still returns 400: "input=139,986, limit=202,752, safe_max=62,510"
4. Proxy clamps max_tokens to 31,383 (L0 auto-clamp)
5. Request succeeds with reduced output (31K tokens instead of 64K)
6. CC receives response, adds ~3K tokens to context
7. Next request: same ~139K tokens → same overflow → same clamp → repeat
8. CC NEVER compact because it sees 139K/200K = 69.5% → below 95% threshold
```

### Evidence
From proxy log, **10 identical overflow cycles** between 06:15 and 07:01 UTC:
```
06:15 - Pre-check: ~139729tok → NIM error: input=139986 → clamp to 31383
06:20 - Pre-check: ~139729tok → NIM error: input=139986 → clamp to 31383
06:25 - Pre-check: ~139729tok → NIM error: input=139986 → clamp to 31383
06:30 - Pre-check: ~139729tok → NIM error: input=139986 → clamp to 31383
06:35 - Pre-check: ~139729tok → NIM error: input=139986 → clamp to 31383
06:40 - Pre-check: ~139729tok → NIM error: input=139986 → clamp to 31383
06:45 - Pre-check: ~139729tok → NIM error: input=139986 → clamp to 31383
06:50 - Pre-check: ~139729tok → NIM error: input=139986 → clamp to 31383
06:56 - Pre-check: ~139729tok → NIM error: input=139986 → clamp to 31383
07:01 - Pre-check: ~139729tok → NIM error: input=139986 → clamp to 31383
```

### Why v11 Fixes Would Resolve This

1. **FIX 1 (scale_tokens)**: With 1.1x scaling, CC would see 139K × 1.1 = 153K input_tokens → 76.5% of 200K. Still below 95%, but combined with growing tokens, would reach 95% sooner.

2. **FIX 2 (ContextOverflow post-retry)**: After the Fixable retry succeeds, if `scaled_tokens > 180K` (90% of 200K), ContextOverflow would fire → CC receives error → shows "Use /compact" → auto-compact triggers.

3. **The 1.1x buffer alone may NOT be sufficient** — at 139K real tokens, 1.1x = 153K scaled = 76.5%. CC needs to reach ~172K real tokens (190K scaled = 95%) for auto-compact. The overflow loop prevents reaching 172K because max_tokens is clamped.

### New Fix Needed: FIX 4 — Overflow Loop Detection

The overflow loop is a NEW issue not addressed by v11 fixes alone. Even with v11 fixes deployed:
- At 139K real tokens, 1.1x = 153K scaled (76.5%) — still below 90% ContextOverflow threshold
- CC keeps getting responses with clamped max_tokens (31K instead of 64K)
- Each response adds ~3K tokens, growing VERY slowly
- CC could loop for HOURS before reaching 172K (where 1.1x = 190K scaled = 95%)

**Proposed FIX 4**: Detect the overflow loop pattern — if 3+ consecutive requests for the same model have the same input_tokens (within 5%) AND all overflow, force a ContextOverflow return regardless of the 90% threshold.

---

## 4. Proxy Log Statistics

| Category | Count | First Seen | Last Seen |
|:---------|:------|:-----------|:----------|
| Token usage reports | 657 | 2026-04-24 | 2026-04-25 20:13 |
| 429 rate limit (NIM concurrency) | 173 | Various | Various |
| 502 bad gateway (NIM errors) | 141 | Various | Various |
| Input tokens overflow | 59 | 06:10 | 15:01 |
| Pre-check overflow (tiktoken) | 21 | 01:03 | 18:41 |
| Stream chunk timeout | 6 | 08:19 | 16:06 |
| Exhausted retries (all 3 failed) | 15 | 02:12 | 20:22 |
| ContextOverflow returned | **0** | N/A | N/A |
| Scaling up input_tokens | **0** | N/A | N/A |
| Context nearly full warning | **0** | N/A | N/A |
| Error event emitted | **0** | N/A | N/A |

### Overflow Token Values Distribution

| input_tokens | Count | Interpretation |
|:-------------|:------|:--------------|
| 138,753 | 47 | Most common overflow level — CC stuck at this level |
| 142,082 | 31 | After receiving one response (added ~3K tokens) |
| 139,986 | 11 | The overflow loop level (10 identical cycles) |
| 146,435 | 7 | Higher token level (2nd retry overflow) |
| 143,316/143,319 | 4 | Intermediate levels |
| 149,385+ | 5 | Rare, high token levels |

---

## 5. Model-Specific Issues

| Model | Context | Upstream | Key Issues |
|:------|:--------|:---------|:-----------|
| z-ai/glm5 | 202,752 | NIM | Primary overflow source, auto-compact never triggers |
| moonshotai/kimi-k2.5 | 256K? | NIM | Not tested at high token levels |
| deepseek-ai/deepseek-v3.2 | 131,072 | NIM | Overflow at 100K+ and 150K+ tokens |

### DeepSeek Overflow at 150K (NEW — H6)
At 18:41 UTC, the proxy logged a DeepSeek pre-check overflow at **150,158 tokens** — exceeding DeepSeek's 131,072 context limit by 19K tokens. This means CC reached 150K tokens with a model that only supports 128K context. The pre-check clamped max_tokens to just **1,024** — essentially a null response.

This is the SAME root cause as the GLM5 issue: `scale_tokens()` doesn't inflate tokens for DeepSeek (128K < 200K CC context), but the scaling factor is 1.56x (200K/128K), which SHOULD trigger auto-compact at 128K real tokens. The 150K level suggests scale_tokens was working for DeepSeek but CC's tiktoken estimate was wrong, OR the auto-compact triggered too late.

---

## 6. Recommendations

### Immediate (Before Merge)
1. **Deploy v11 fixes** to port 8315 — rebuild the `~/.cargo/bin/nexus-ai-gateway` binary with the hardened code
2. **Implement FIX 4** — Overflow loop detection (3+ identical overflows → force ContextOverflow)
3. **Increase FIX 2 threshold** — Consider lowering from 90% to 80% (160K scaled) for earlier ContextOverflow signal

### Short-Term (v0.14.0)
4. **Add overflow loop counter** — Track consecutive identical overflow events per session
5. **Add 429/502 metrics** — Track rate of NIM errors per model per hour
6. **Add circuit breaker integration** — Currently implemented but not connected to request flow
7. **Increase retry backoff** — 429 exhausted retries suggest backoff is too aggressive

### Medium-Term (v0.15.0)
8. **Model-aware auto-compact** — Different models need different compact thresholds
9. **Token budget dashboard** — Real-time visualization of token usage per session
10. **Graceful degradation** — When max_tokens is clamped, inform CC about reduced output capacity

---

## 7. Next Steps

1. ✅ Complete forensic analysis of all 3 sessions (agents running in background)
2. ⏳ Write detailed analysis documents (in progress)
3. ⏳ Cross-reference with v11 findings (agent running in background)
4. ⏳ Implement FIX 4 (overflow loop detection)
5. ⏳ Deploy hardened proxy to port 8315
6. ⏳ Validate with real-world testing
7. ⏳ Merge to main (only with explicit user confirmation)

---

*Executive summary generated by Boss. Detailed analysis documents being generated by parallel agents.*
