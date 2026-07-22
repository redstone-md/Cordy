//! Provider layer: one trait, one adapter file per API family.
//!
//! Each adapter translates the canonical [`ChatRequest`] into its wire body and normalizes the
//! response SSE into [`WireEvent`]s. Adding a provider is one more file implementing
//! [`Provider`]; the agent loop is untouched. Concrete adapters (openai_chat first) land in the
//! next build-order step.

use async_trait::async_trait;
use futures::stream::BoxStream;

use crate::core::types::{Caps, ChatRequest, WireEvent};

pub mod anthropic_messages;
pub mod models_list;
pub mod openai_chat;
pub mod openai_compatible;
pub mod openai_responses;
pub mod retry;

#[async_trait]
pub trait Provider: Send + Sync {
    /// Start a streaming completion, yielding normalized events.
    async fn stream(&self, req: ChatRequest) -> anyhow::Result<BoxStream<'static, WireEvent>>;

    /// What this provider/model supports.
    fn caps(&self) -> Caps;
}
