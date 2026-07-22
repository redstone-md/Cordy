//! Context management — deciding when and how to compact conversation history.
//!
//! Auto-compact is per-API-family: providers that manage context server-side (Anthropic context
//! editing, OpenAI Responses compaction — `Caps::native_context_mgmt`) need no client work;
//! everything else (OpenAI Chat, compatible backends) relies on the client-side compactor here.
//! This module owns the *decision* (threshold) and the *plan* (which messages to summarize vs
//! keep). The summary itself is produced by a cheap model via a sub-agent, driven by the caller.

use crate::core::types::Message;
use crate::tools::optimize::estimate_tokens;

/// Rough token estimate for a slice of messages (serialized length / 4).
pub fn estimate_messages_tokens(messages: &[Message]) -> u64 {
    messages
        .iter()
        .map(|m| {
            serde_json::to_string(m)
                .map(|s| estimate_tokens(&s))
                .unwrap_or(0)
        })
        .sum()
}

/// Modes for triggering compaction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompactMode {
    /// Only when the user asks (`/compact`).
    Manual,
    /// Automatically once the threshold is crossed.
    Auto,
}

/// Decides when history should be compacted and how to split it.
pub struct ContextManager {
    pub threshold_tokens: u64,
    pub mode: CompactMode,
    /// When true the provider manages context itself; the client-side compactor stays idle.
    pub native: bool,
    /// Messages always kept verbatim (the most recent turns).
    pub keep_recent: usize,
}

impl ContextManager {
    pub fn new(threshold_tokens: u64, mode: CompactMode, native: bool) -> Self {
        ContextManager {
            threshold_tokens,
            mode,
            native,
            keep_recent: 6,
        }
    }

    /// Whether auto-compaction should fire for the given history.
    pub fn needs_compaction(&self, messages: &[Message]) -> bool {
        if self.native || self.mode != CompactMode::Auto {
            return false;
        }
        estimate_messages_tokens(messages) >= self.threshold_tokens
    }

    /// Split history into `(to_summarize, to_keep)`: the oldest messages are summarized while the
    /// most recent `keep_recent` are preserved verbatim. Returns `None` when there is nothing old
    /// enough to be worth summarizing.
    pub fn plan_compaction<'a>(
        &self,
        messages: &'a [Message],
    ) -> Option<(&'a [Message], &'a [Message])> {
        if messages.len() <= self.keep_recent {
            return None;
        }
        let split = messages.len() - self.keep_recent;
        Some((&messages[..split], &messages[split..]))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::types::ContentBlock;

    fn msgs(n: usize) -> Vec<Message> {
        (0..n)
            .map(|i| Message::user(format!("message number {i} with some words")))
            .collect()
    }

    #[test]
    fn native_provider_never_compacts() {
        let cm = ContextManager::new(1, CompactMode::Auto, true);
        assert!(!cm.needs_compaction(&msgs(100)));
    }

    #[test]
    fn manual_mode_never_auto_compacts() {
        let cm = ContextManager::new(1, CompactMode::Manual, false);
        assert!(!cm.needs_compaction(&msgs(100)));
    }

    #[test]
    fn auto_compacts_over_threshold() {
        let big = ContextManager::new(10, CompactMode::Auto, false);
        assert!(big.needs_compaction(&msgs(50)));
        let high = ContextManager::new(1_000_000, CompactMode::Auto, false);
        assert!(!high.needs_compaction(&msgs(2)));
    }

    #[test]
    fn plan_keeps_recent_and_summarizes_old() {
        let cm = ContextManager::new(10, CompactMode::Auto, false); // keep_recent = 6
        let history = msgs(10);
        let (old, keep) = cm.plan_compaction(&history).unwrap();
        assert_eq!(old.len(), 4);
        assert_eq!(keep.len(), 6);
        // Too short to compact.
        assert!(cm.plan_compaction(&msgs(6)).is_none());
    }

    #[test]
    fn estimate_grows_with_content() {
        let small = estimate_messages_tokens(&[Message::user("hi")]);
        let big = estimate_messages_tokens(&[Message::assistant(vec![ContentBlock::text(
            "a much longer message that clearly has more tokens than the tiny one",
        )])]);
        assert!(big > small);
    }
}
