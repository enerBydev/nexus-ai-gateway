//! Streaming FST that normalizes raw NIM reasoning incrementally (ARB · L2, #90-B).
//!
//! The old per-delta logic in `streaming.rs` used `delta.contains("<previous_reasoning")`,
//! which misses a marker split across two SSE deltas (e.g. `…<previous_rea` then
//! `soning>…`). This transducer scans the reasoning stream as a finite-state machine
//! with a bounded tag buffer, so markers spanning delta boundaries are handled, at
//! O(1) amortized work per character and O(max|tag|) memory.
//!
//! It is the single source of truth for reasoning normalization. `normalize_full`
//! drives it over a whole string (the non-streaming path); the streaming path drives
//! `push`/`flush` incrementally. A random-split property test proves split-feeding
//! equals whole-feeding (I4). Note one deliberate correction over the entregable's
//! sketch: `</previous_reasoning>` halts from *any* state — including inside a
//! `<tool_call>` block — matching a batch pass that cuts at it *before* removing tool
//! calls (and avoiding the prior `find`-from-zero infinite loop on out-of-order tags).

/// Longest recognized marker (`</previous_reasoning>` = 21 bytes). A `<…` candidate
/// longer than this cannot be a known marker, so it is flushed as literal text.
const MAX_TAG_LEN: usize = 21;

const HALT: &str = "</previous_reasoning>";
const TOOL_OPEN: &str = "<tool_call>";
const TOOL_CLOSE: &str = "</tool_call>";
/// Stray tags removed in place (emit nothing), matching `sanitize_reasoning` step 3.
const ELIDE: &[&str] =
    &["<previous_reasoning>", "<arg_key>", "</arg_key>", "<arg_value>", "</arg_value>"];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum State {
    /// Emitting reasoning normally.
    #[default]
    Normal,
    /// Inside a `<tool_call>…</tool_call>` block — output suppressed.
    InToolCall,
    /// Saw `</previous_reasoning>` — everything after is dropped.
    Halted,
}

/// Incremental, UTF-8-safe normalizer for a single reasoning stream.
#[derive(Debug, Default)]
pub struct ReasoningTransducer {
    state: State,
    /// `true` while accumulating a `<…>` tag candidate.
    in_tag: bool,
    /// The current `<…` candidate (ASCII markers; arbitrary tags flushed as literal).
    tag_buf: String,
}

fn is_marker_prefix(s: &str) -> bool {
    if HALT.starts_with(s) || TOOL_OPEN.starts_with(s) || TOOL_CLOSE.starts_with(s) {
        return true;
    }
    ELIDE.iter().any(|m| m.starts_with(s))
}

impl ReasoningTransducer {
    pub fn new() -> Self {
        Self::default()
    }

    /// Feed one delta; returns the clean reasoning to emit for it.
    pub fn push(&mut self, delta: &str) -> String {
        let mut out = String::new();
        for c in delta.chars() {
            self.process_char(c, &mut out);
        }
        out
    }

    /// Close the stream: any incomplete `<…` candidate is literal text.
    pub fn flush(&mut self) -> String {
        let mut out = String::new();
        if self.in_tag {
            let buf = std::mem::take(&mut self.tag_buf);
            self.emit_literal(&buf, &mut out);
            self.in_tag = false;
        }
        out
    }

    fn process_char(&mut self, c: char, out: &mut String) {
        if self.state == State::Halted {
            return; // nothing is ever emitted again
        }
        if self.in_tag {
            if c == '<' {
                // The accumulated candidate was not a complete tag; flush it as literal
                // (minus this new `<`) and restart the candidate at the new `<`.
                let prev = std::mem::take(&mut self.tag_buf);
                self.emit_literal(&prev, out);
                self.tag_buf.push('<');
            } else {
                self.tag_buf.push(c);
                if c == '>' {
                    let tag = std::mem::take(&mut self.tag_buf);
                    self.in_tag = false;
                    self.act_on_tag(&tag, out);
                } else if self.tag_buf.len() > MAX_TAG_LEN && !is_marker_prefix(&self.tag_buf) {
                    // Unclosed, over-length, not a known marker -> literal text.
                    let prev = std::mem::take(&mut self.tag_buf);
                    self.emit_literal(&prev, out);
                    self.in_tag = false;
                }
            }
        } else if c == '<' {
            self.in_tag = true;
            self.tag_buf.push('<');
        } else {
            self.emit_char(c, out);
        }
    }

    /// Apply a completed `<…>` tag according to the current state.
    fn act_on_tag(&mut self, tag: &str, out: &mut String) {
        if tag == HALT {
            self.state = State::Halted;
        } else if tag == TOOL_OPEN {
            if self.state == State::Normal {
                self.state = State::InToolCall;
            }
            // Already InToolCall: nested open ignored (matches batch block removal).
        } else if tag == TOOL_CLOSE {
            if self.state == State::InToolCall {
                self.state = State::Normal;
            } else {
                self.emit_literal(tag, out); // stray close in Normal -> literal
            }
        } else if ELIDE.contains(&tag) {
            // Removed in place (emit nothing).
        } else {
            self.emit_literal(tag, out); // unknown tag -> literal (Normal) / suppressed (InToolCall)
        }
    }

    fn emit_char(&self, c: char, out: &mut String) {
        if self.state == State::Normal {
            out.push(c);
        }
    }

    fn emit_literal(&self, s: &str, out: &mut String) {
        if self.state == State::Normal {
            out.push_str(s);
        }
    }
}

/// Run the whole transducer over a full string and trim — the batch entry point used
/// by the non-streaming response path (replaces the former `sanitize_reasoning`).
pub fn normalize_full(raw: &str) -> String {
    let mut t = ReasoningTransducer::new();
    let mut s = t.push(raw);
    s.push_str(&t.flush());
    s.trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Drive the transducer over `input` split at the given byte boundaries.
    fn run_split(input: &str, cuts: &[usize]) -> String {
        let mut t = ReasoningTransducer::new();
        let mut out = String::new();
        let mut start = 0;
        let mut bounds: Vec<usize> = cuts.to_vec();
        bounds.push(input.len());
        for &b in &bounds {
            let b = b.min(input.len());
            if b <= start {
                continue;
            }
            // Snap to a char boundary so we never split a UTF-8 sequence.
            let mut end = b;
            while end < input.len() && !input.is_char_boundary(end) {
                end += 1;
            }
            out.push_str(&t.push(&input[start..end]));
            start = end;
        }
        if start < input.len() {
            out.push_str(&t.push(&input[start..]));
        }
        out.push_str(&t.flush());
        out
    }

    fn run_whole(input: &str) -> String {
        run_split(input, &[])
    }

    #[test]
    fn cuts_at_previous_reasoning_close() {
        let input = "keep this</previous_reasoning>drop this";
        assert_eq!(run_whole(input).trim(), "keep this");
    }

    #[test]
    fn strips_tool_call_block() {
        let input = "before<tool_call>{\"x\":1}</tool_call>after";
        assert_eq!(run_whole(input).trim(), "beforeafter");
    }

    #[test]
    fn elides_stray_xml_tags() {
        // Only the tags are removed (matching sanitize_reasoning step 3), not the text
        // between them: the `k`/`v` survive.
        let input = "<previous_reasoning>a<arg_key>k</arg_key>b<arg_value>v</arg_value>c";
        assert_eq!(run_whole(input).trim(), "akbvc");
    }

    #[test]
    fn unclosed_tool_call_truncates() {
        let input = "head<tool_call>no close here";
        assert_eq!(run_whole(input).trim(), "head");
    }

    #[test]
    fn halt_inside_tool_call_matches_batch() {
        // The entregable sketch missed this: the batch cuts at </previous_reasoning>
        // BEFORE removing tool calls, so a close inside a tool_call still halts.
        let input = "keep<tool_call>x</previous_reasoning>y</tool_call>more";
        assert_eq!(run_whole(input).trim(), "keep");
    }

    #[test]
    fn stray_close_before_open_terminates() {
        // Regression: a "</tool_call>" before "<tool_call>" used to spin forever in
        // sanitize_reasoning (the close was found at index 0, string never shrank).
        let input = "</tool_call><tool_call>secret</tool_call>tail";
        assert_eq!(run_whole(input).trim(), "</tool_call>tail");
    }

    #[test]
    fn unknown_tag_is_literal() {
        let input = "a<unknown>b";
        assert_eq!(run_whole(input).trim(), "a<unknown>b");
    }

    #[test]
    fn marker_split_across_deltas() {
        // The exact case the old `.contains()` per-delta logic failed on.
        let input = "keep</previous_reasoning>drop";
        // Split right in the middle of the close marker.
        let cut = "keep</previous_rea".len();
        assert_eq!(run_split(input, &[cut]).trim(), "keep");
    }

    #[test]
    fn tool_call_split_across_deltas() {
        let input = "x<tool_call>SECRET</tool_call>y";
        for cut in 1..input.len() {
            assert_eq!(run_split(input, &[cut]).trim(), "xy", "failed at cut {cut}");
        }
    }

    #[test]
    fn utf8_multibyte_preserved() {
        let input = "café->über</previous_reasoning>π";
        assert_eq!(run_whole(input).trim(), "café->über");
    }

    #[test]
    fn streaming_equals_batch_random_splits() {
        // Property I4: for arbitrary inputs and arbitrary delta boundaries, feeding the
        // transducer split-by-split equals feeding the whole string at once
        // (`normalize_full`). The old per-delta `.contains()` logic lacked this guarantee.
        use rand::RngExt;
        let fragments = [
            "normal reasoning ",
            "<tool_call>",
            "{\"a\": 1}",
            "</tool_call>",
            "</previous_reasoning>",
            "<previous_reasoning>",
            "<arg_key>",
            "</arg_key>",
            "<arg_value>",
            "</arg_value>",
            "more <text> here",
            "café->π ",
            "<",
            ">",
            "</tool_",
            "call>",
            "</prev",
            "ious_reasoning>",
        ];
        let mut rng = rand::rng();
        for _ in 0..300 {
            let n = rng.random_range(0..8);
            let mut input = String::new();
            for _ in 0..n {
                input.push_str(fragments[rng.random_range(0..fragments.len())]);
            }
            // Random split points.
            let mut cuts: Vec<usize> = Vec::new();
            let splits = rng.random_range(0..4);
            for _ in 0..splits {
                if !input.is_empty() {
                    cuts.push(rng.random_range(0..=input.len()));
                }
            }
            cuts.sort_unstable();
            let got = run_split(&input, &cuts);
            assert_eq!(
                got.trim(),
                normalize_full(&input),
                "mismatch for input {input:?} cuts {cuts:?}"
            );
        }
    }
}
