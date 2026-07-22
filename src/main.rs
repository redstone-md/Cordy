//! Cordy — a cross-platform TUI coding agent.
//!
//! Scaffold stage: the canonical model, capability/provider/tool traits, and a minimal
//! agent loop are in place. Wiring (config, UI, providers) lands in later build-order steps.
#![allow(dead_code)] // scaffolding: some trait surface exists before all callers do

mod agents;
mod config;
mod core;
#[cfg(feature = "mcp")]
mod mcp;
mod provider;
mod skills;
mod tools;
mod tui;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();

    if args.first().map(String::as_str) == Some("init") {
        let cwd = std::env::current_dir()?;
        let created = config::init_project(&cwd)?;
        if created.is_empty() {
            println!("cordy: .cordy already initialized");
        } else {
            println!("cordy: created\n  {}", created.join("\n  "));
        }
        return Ok(());
    }

    // `cordy smoke [prompt]` — headless provider smoke test (no TUI); prints the streamed reply.
    if args.first().map(String::as_str) == Some("smoke") {
        let prompt = args
            .get(1)
            .cloned()
            .unwrap_or_else(|| "Say hello in one short sentence.".into());
        return smoke(prompt).await;
    }

    // `--resume [id]` resumes a saved session (no id -> most recent).
    let resume = args.iter().position(|a| a == "--resume").map(|pos| {
        args.get(pos + 1)
            .filter(|s| !s.starts_with('-'))
            .cloned()
            .unwrap_or_default()
    });

    tui::run(resume).await
}

/// Stream one completion through the real provider pipeline and print it. Uses the same env vars
/// as the TUI (`OPENAI_API_KEY`, `CORDY_BASE_URL`, `CORDY_MODEL`).
async fn smoke(prompt: String) -> anyhow::Result<()> {
    use core::types::{ChatRequest, Message, WireEvent};
    use futures::StreamExt;
    use provider::Provider;
    use provider::openai_chat::OpenAiChat;

    let key = std::env::var("OPENAI_API_KEY").unwrap_or_default();
    let model =
        std::env::var("CORDY_MODEL").unwrap_or_else(|_| "meta/llama-3.1-8b-instruct".into());
    let mut p = OpenAiChat::new(key, &model);
    if let Ok(base) = std::env::var("CORDY_BASE_URL") {
        p = p.with_base_url(base);
    }

    let req = ChatRequest {
        model: String::new(),
        system: "You are Cordy, a concise coding assistant.".into(),
        messages: vec![Message::user(prompt)],
        tools: Vec::new(),
        max_tokens: Some(200),
        temperature: None,
    };

    eprintln!("[smoke] model={model} streaming…");
    use std::io::Write;
    let mut stream = p.stream(req).await?;
    let mut deltas = 0usize;
    while let Some(ev) = stream.next().await {
        match ev {
            WireEvent::TextDelta(s) => {
                deltas += 1;
                print!("{s}");
                let _ = std::io::stdout().flush(); // flush so streaming is visible live
            }
            WireEvent::Usage(u) => {
                eprintln!(
                    "\n[smoke] usage: {} in / {} out",
                    u.input_tokens, u.output_tokens
                )
            }
            WireEvent::Error(e) => eprintln!("\n[smoke] error: {e}"),
            WireEvent::Done => break,
            _ => {}
        }
    }
    println!();
    eprintln!("[smoke] text deltas received: {deltas}");
    Ok(())
}
