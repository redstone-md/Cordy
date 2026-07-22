//! Native token optimizer — the built-in "Rust Token Killer".
//!
//! Compresses tool output (command results) before it enters the model context. A registry of
//! [`OutputCompressor`]s is tried in order; a catch-all generic compressor (dedupe + middle
//! truncation) is always last so unknown commands still shrink. Command-family compressors
//! (git/cargo/grep/...) are added ahead of it in later steps.

/// Result of compressing some output. `saved` is the estimated token reduction.
#[derive(Debug, Clone, PartialEq)]
pub struct Compressed {
    pub text: String,
    pub saved: u64,
}

/// A strategy for shrinking the output of a matching command family.
pub trait OutputCompressor: Send + Sync {
    /// Whether this compressor handles the given command line.
    fn matches(&self, cmd: &str) -> bool;
    /// Compress raw output.
    fn compress(&self, raw: &str) -> Compressed;
}

/// Cheap token estimate (~4 chars/token) used only for the savings metric.
pub fn estimate_tokens(s: &str) -> u64 {
    (s.len() as u64).div_ceil(4)
}

/// Ordered set of compressors with a generic fallback, plus a global on/off toggle.
pub struct Optimizer {
    compressors: Vec<Box<dyn OutputCompressor>>,
    enabled: bool,
}

impl Optimizer {
    /// Build with only the generic fallback. `enabled=false` makes [`apply`](Self::apply) a
    /// passthrough (config `optimize = false`).
    pub fn new(enabled: bool) -> Self {
        Optimizer {
            compressors: vec![Box::new(GenericCompressor::new(200))],
            enabled,
        }
    }

    /// Register a family compressor. It is tried before the generic fallback (which stays last).
    pub fn register(&mut self, c: Box<dyn OutputCompressor>) {
        let fallback_idx = self.compressors.len() - 1;
        self.compressors.insert(fallback_idx, c);
    }

    /// Compress `raw` for the given `cmd`. Passthrough when disabled.
    pub fn apply(&self, cmd: &str, raw: &str) -> Compressed {
        if !self.enabled {
            return Compressed {
                text: raw.to_string(),
                saved: 0,
            };
        }
        for c in &self.compressors {
            if c.matches(cmd) {
                return c.compress(raw);
            }
        }
        Compressed {
            text: raw.to_string(),
            saved: 0,
        }
    }
}

/// Catch-all: collapse consecutive duplicate lines, then keep only the head and tail when the
/// output is very long. Cheap, lossy-but-safe, and applies to any command.
struct GenericCompressor {
    max_lines: usize,
}

impl GenericCompressor {
    fn new(max_lines: usize) -> Self {
        GenericCompressor { max_lines }
    }
}

impl OutputCompressor for GenericCompressor {
    fn matches(&self, _cmd: &str) -> bool {
        true
    }

    fn compress(&self, raw: &str) -> Compressed {
        let deduped = dedupe_consecutive(raw);
        let text = truncate_middle(&deduped, self.max_lines);
        let saved = estimate_tokens(raw).saturating_sub(estimate_tokens(&text));
        Compressed { text, saved }
    }
}

/// Collapse runs of identical lines into one line plus a `... (xN)` marker.
fn dedupe_consecutive(raw: &str) -> String {
    let mut out: Vec<String> = Vec::new();
    let mut prev: Option<&str> = None;
    let mut run = 0usize;

    let flush = |out: &mut Vec<String>, line: &str, run: usize| {
        out.push(line.to_string());
        if run > 1 {
            out.push(format!("... (x{run})"));
        }
    };

    for line in raw.lines() {
        match prev {
            Some(p) if p == line => run += 1,
            Some(p) => {
                flush(&mut out, p, run);
                run = 1;
            }
            None => run = 1,
        }
        prev = Some(line);
    }
    if let Some(p) = prev {
        flush(&mut out, p, run);
    }
    out.join("\n")
}

/// If the text exceeds `max_lines`, keep the first and last halves with an elision marker.
fn truncate_middle(text: &str, max_lines: usize) -> String {
    let lines: Vec<&str> = text.lines().collect();
    if lines.len() <= max_lines {
        return text.to_string();
    }
    let keep = max_lines / 2;
    let omitted = lines.len() - keep * 2;
    let mut out: Vec<String> = Vec::with_capacity(keep * 2 + 1);
    out.extend(lines[..keep].iter().map(|s| s.to_string()));
    out.push(format!("... ({omitted} lines elided by optimizer) ..."));
    out.extend(lines[lines.len() - keep..].iter().map(|s| s.to_string()));
    out.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dedupe_collapses_runs() {
        let raw = "a\nb\nb\nb\nc";
        assert_eq!(dedupe_consecutive(raw), "a\nb\n... (x3)\nc");
    }

    #[test]
    fn generic_compressor_saves_tokens_on_dupes() {
        let raw = "same line\n".repeat(50);
        let opt = Optimizer::new(true);
        let out = opt.apply("some-cmd", &raw);
        assert!(out.saved > 0);
        assert!(out.text.contains("(x"));
    }

    #[test]
    fn disabled_optimizer_is_passthrough() {
        let raw = "x\nx\nx";
        let opt = Optimizer::new(false);
        let out = opt.apply("cmd", raw);
        assert_eq!(out.text, raw);
        assert_eq!(out.saved, 0);
    }

    #[test]
    fn truncate_keeps_head_and_tail() {
        let raw: String = (0..1000).map(|i| format!("line{i}\n")).collect();
        let out = truncate_middle(&raw, 100);
        assert!(out.contains("line0"));
        assert!(out.contains("line999"));
        assert!(out.contains("elided by optimizer"));
    }
}
