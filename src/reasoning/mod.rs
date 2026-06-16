//! ARB — Adaptive Reasoning Broker (Issue #90-B).
//!
//! Agnostic, autonomous translation of a NIM model's reasoning into Claude Code's
//! Anthropic thinking protocol. Organized as a streaming pipeline (entregable 07):
//! L0 profile · L1 ingest · L2 normalize (FST τ) · L3 emit · L4 signature σ · L5
//! reconcile ρ. Every artifact NEXUS synthesizes is valid in the Anthropic protocol
//! and deterministically reconcilable; the cryptographic signature is the only hard
//! boundary (not forgeable) and is handled by self-provenance + CC's strip-retry.

pub mod signature;
pub mod transducer;
