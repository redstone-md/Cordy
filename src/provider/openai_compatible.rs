//! OpenAI-compatible endpoints (ollama, vllm, openrouter, groq, llama.cpp, ...).
//!
//! These speak the exact Chat Completions wire format, so this is a thin constructor over
//! [`OpenAiChat`] with the base URL set and capabilities relaxed (local models often lack image
//! support). No duplicated streaming/rendering logic — DRY over the reference adapter.

use crate::core::types::Caps;
use crate::provider::openai_chat::OpenAiChat;

/// Build a provider for an OpenAI-compatible server. `api_key` may be any non-empty string for
/// servers that don't authenticate.
pub fn openai_compatible(
    base_url: impl Into<String>,
    api_key: impl Into<String>,
    model: impl Into<String>,
) -> OpenAiChat {
    OpenAiChat::new(api_key, model)
        .with_base_url(base_url)
        .with_caps(Caps {
            thinking: false,
            tools: true,
            images: false, // conservative default; override per model
            prompt_cache: false,
            native_context_mgmt: false,
        })
}
