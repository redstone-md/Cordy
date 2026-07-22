//! Retry wrapper — transparently retries a provider's initial connection on transient errors
//! with exponential backoff + jitter.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use futures::stream::BoxStream;

use crate::core::types::{Caps, ChatRequest, WireEvent};
use crate::provider::Provider;

/// Wraps any [`Provider`], retrying `stream()` on error up to `max_retries` times.
pub struct RetryProvider {
    inner: Arc<dyn Provider>,
    max_retries: u32,
}

impl RetryProvider {
    pub fn new(inner: Arc<dyn Provider>, max_retries: u32) -> Self {
        RetryProvider { inner, max_retries }
    }
}

#[async_trait]
impl Provider for RetryProvider {
    async fn stream(&self, req: ChatRequest) -> anyhow::Result<BoxStream<'static, WireEvent>> {
        let mut attempt = 0u32;
        loop {
            match self.inner.stream(req.clone()).await {
                Ok(stream) => return Ok(stream),
                Err(e) => {
                    // Client errors (4xx: bad model, auth, request) are permanent — fail fast.
                    if attempt >= self.max_retries || is_client_error(&e) {
                        return Err(e);
                    }
                    tokio::time::sleep(backoff_delay(attempt, jitter_ms())).await;
                    attempt += 1;
                }
            }
        }
    }

    fn caps(&self) -> Caps {
        self.inner.caps()
    }
}

/// Whether an error is a permanent 4xx client error (don't retry those). Detects both a raw
/// `reqwest::Error` status and adapters that format the reason as "HTTP client error ...".
fn is_client_error(e: &anyhow::Error) -> bool {
    if e.downcast_ref::<reqwest::Error>()
        .and_then(|re| re.status())
        .is_some_and(|s| s.is_client_error())
    {
        return true;
    }
    e.to_string().to_lowercase().contains("client error")
}

/// Exponential backoff (500ms * 2^attempt, capped) plus a jitter offset.
fn backoff_delay(attempt: u32, jitter_ms: u64) -> Duration {
    let base = 500u64.saturating_mul(1u64 << attempt.min(5));
    Duration::from_millis(base + jitter_ms)
}

/// A small random jitter in [0, 250) ms.
fn jitter_ms() -> u64 {
    let mut buf = [0u8; 2];
    let _ = getrandom::fill(&mut buf);
    (u16::from_le_bytes(buf) as u64) % 250
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_grows_exponentially() {
        assert_eq!(backoff_delay(0, 0), Duration::from_millis(500));
        assert_eq!(backoff_delay(1, 0), Duration::from_millis(1000));
        assert_eq!(backoff_delay(3, 10), Duration::from_millis(4010));
        // Shift is capped so it never overflows.
        assert_eq!(backoff_delay(50, 0), Duration::from_millis(500 * 32));
    }
}
