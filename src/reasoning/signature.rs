//! Provenance tokens for thinking blocks synthesized by NEXUS (ARB · L4, Issue #90-B).
//!
//! Anthropic's real thinking `signature` is a cryptographic MAC over the encrypted
//! thinking, verifiable only with Anthropic's private key — NEXUS cannot forge it
//! (EUF-CMA; entregable 07 §5). So when NEXUS synthesizes a thinking block from a NIM
//! model's reasoning it emits a *self-describing provenance token* prefixed
//! `nexus:v1:`, NOT a forged Anthropic signature. The request-side reconciliation
//! (L5, `transform.rs`) recognizes this prefix to revert NEXUS's own synthetic blocks
//! to text while preserving real Anthropic signatures verbatim (the bug documented in
//! entregable 06 §3 / vercel/ai#9351).

use hmac::{Hmac, Mac};
use sha2::Sha256;
use std::sync::OnceLock;

type HmacSha256 = Hmac<Sha256>;

/// Prefix marking a thinking signature as NEXUS-synthesized (never a real Anthropic MAC).
pub const PROV_PREFIX: &str = "nexus:v1:";

/// Signature emission policy for synthesized thinking blocks (`REASONING_SIGNATURE_MODE`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SignatureMode {
    /// Emit a `nexus:v1:` provenance token (default): completes the sub-protocol and
    /// gives Claude Code a well-formed block. Cross-backend is auto-healed by CC's
    /// `strip_retry`.
    SelfProvenance,
    /// Emit no signature at all (CC tolerates an absent signature; token count `?? 0`).
    Omit,
    /// Transport reasoning as text instead of a signed thinking block (handled at the
    /// emission layer; here it behaves as "no synthetic signature").
    Durable,
}

impl SignatureMode {
    /// Read `REASONING_SIGNATURE_MODE` (`self` | `omit` | `durable`; default `self`).
    pub fn from_env() -> Self {
        match std::env::var("REASONING_SIGNATURE_MODE").as_deref() {
            Ok("omit") => SignatureMode::Omit,
            Ok("durable") => SignatureMode::Durable,
            _ => SignatureMode::SelfProvenance,
        }
    }
}

/// Encode bytes as lowercase hex (inline to avoid adding the `hex` crate; mirrors
/// `telemetry::fingerprint::to_hex`).
fn to_hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Process-stable secret for the provenance HMAC. Recognition of a NEXUS token is by
/// the `nexus:v1:` prefix (not by verifying the MAC), so any stable per-process key
/// suffices; the HMAC only makes the token deterministic per (secret, thinking) and
/// opaque. Override with `REASONING_SIGNATURE_SECRET`; otherwise a 32-byte random
/// per-process secret is generated (mirrors the telemetry instance secret).
fn provenance_secret() -> &'static [u8] {
    static SECRET: OnceLock<Vec<u8>> = OnceLock::new();
    SECRET.get_or_init(|| match std::env::var("REASONING_SIGNATURE_SECRET") {
        Ok(s) if !s.is_empty() => s.into_bytes(),
        _ => rand::random::<[u8; 32]>().to_vec(),
    })
}

/// Build a NEXUS provenance token for a synthesized thinking block:
/// `nexus:v1:` ‖ hex(HMAC-SHA256(secret, trim(thinking))). Deterministic for a given
/// process + thinking content; never claims to be an Anthropic signature.
pub fn self_provenance(thinking: &str) -> String {
    let mut mac =
        HmacSha256::new_from_slice(provenance_secret()).expect("HMAC accepts any key length");
    mac.update(thinking.trim().as_bytes());
    format!("{PROV_PREFIX}{}", to_hex(&mac.finalize().into_bytes()))
}

/// `true` if `sig` was synthesized by NEXUS (carries the `nexus:v1:` prefix). Real
/// Anthropic signatures never match, so L5 reconciliation never reverts them.
#[allow(dead_code)] // TODO(F4): wired into request-side reconciliation ρ (transform.rs)
pub fn is_nexus_provenance(sig: &str) -> bool {
    sig.starts_with(PROV_PREFIX)
}

/// Signature to attach to a NEXUS-synthesized thinking block per the configured mode.
/// `None` means "emit no signature": `Omit`, `Durable` (transported as text elsewhere),
/// or empty thinking.
pub fn reasoning_signature(thinking: &str) -> Option<String> {
    if thinking.trim().is_empty() {
        return None;
    }
    match SignatureMode::from_env() {
        SignatureMode::SelfProvenance => Some(self_provenance(thinking)),
        SignatureMode::Omit | SignatureMode::Durable => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provenance_has_prefix_and_is_recognized() {
        let sig = self_provenance("some reasoning");
        assert!(sig.starts_with(PROV_PREFIX));
        assert!(is_nexus_provenance(&sig));
    }

    #[test]
    fn deterministic_for_same_thinking() {
        assert_eq!(self_provenance("abc"), self_provenance("abc"));
    }

    #[test]
    fn differs_for_different_thinking() {
        assert_ne!(self_provenance("abc"), self_provenance("xyz"));
    }

    #[test]
    fn trims_whitespace_canonically() {
        assert_eq!(self_provenance("  hi  "), self_provenance("hi"));
    }

    #[test]
    fn real_anthropic_signature_not_recognized() {
        // A real Anthropic signature is opaque base64 and never carries our prefix.
        let real = "EqMBCkYIBxgCKkBrealLongBase64SignaturePayloadabcdef0123456789";
        assert!(!is_nexus_provenance(real));
    }

    #[test]
    fn token_tail_is_sha256_hex() {
        let sig = self_provenance("x");
        let tail = sig.strip_prefix(PROV_PREFIX).unwrap();
        assert_eq!(tail.len(), 64); // SHA-256 = 32 bytes = 64 hex chars
        assert!(tail.bytes().all(|b| b.is_ascii_hexdigit()));
    }

    #[test]
    fn reasoning_signature_none_for_empty() {
        assert_eq!(reasoning_signature("   "), None);
    }

    #[test]
    fn reasoning_signature_self_by_default() {
        // Default mode is SelfProvenance (env unset in test process).
        let s = reasoning_signature("real reasoning text");
        assert!(s.is_some());
        assert!(is_nexus_provenance(&s.unwrap()));
    }
}
