//! TUI runtime: owns the terminal, spawns the agent driver, and pumps input/agent/permission
//! events through the pure [`update`](super::update). Not unit-tested (interactive); the state
//! logic it calls is tested in the parent module.

use std::io;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use ratatui::Frame;
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::crossterm::event::{
    self, DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
    Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEventKind,
};
use ratatui::crossterm::execute;
use ratatui::crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Block, Borders, Clear, Padding, Paragraph, Scrollbar, ScrollbarOrientation, ScrollbarState,
    Wrap,
};
use tokio::sync::{mpsc, oneshot};

use std::path::PathBuf;

use super::theme::{THEME_NAMES, Theme, theme_by_name};
use super::{Connect, ConnectStep, Effect, Entry, Model, Msg, update};
use crate::agents::{AgentRegistry, load_agents};
use crate::config::Config;
use crate::core::agent::{AgentEvent, AgentLoop, Session, assemble};
use crate::core::auth::ApiKeyStore;
use crate::core::autonomous::{GoalStore, Guardrails, cordy_dir, is_done, iteration_prompt};
use crate::core::capability::CapabilitySource;
use crate::core::context::{CompactMode, ContextManager};
use crate::core::models_dev::{self, Catalog};
use crate::core::permission::{PermissionEngine, Rule};
use crate::core::prompt::{
    PromptContext, build_system_prompt, load_project_context, load_system_append,
    load_system_override,
};
use crate::core::session_store::{SessionMeta, SessionStore, SessionSummary, now_unix};
use crate::core::types::{ChatRequest, ContentBlock, Message, Role, Usage};
use crate::provider::Provider;
use crate::provider::anthropic_messages::Anthropic;
use crate::provider::models_list::list_models;
use crate::provider::openai_chat::OpenAiChat;
use crate::provider::openai_compatible::openai_compatible;
use crate::provider::openai_responses::OpenAiResponses;
use crate::provider::retry::RetryProvider;
use crate::skills::{SkillSet, load_skills};
use crate::tools::builtins::BuiltinTools;
use crate::tools::optimize::Optimizer;
use crate::tools::subagent::SubAgentTool;
use crate::tools::{Permission, PermissionRequest, Registry, Tool, ToolCtx};
use tokio::sync::Semaphore;
use tokio_util::sync::CancellationToken;

/// Shared handle to the current turn's cancellation token, so the UI can interrupt it.
type CancelSlot = Arc<Mutex<Option<CancellationToken>>>;

struct PermissionAsk {
    summary: String,
    /// Tool name + request key, so the UI can build an "allow always" rule.
    tool: String,
    key: String,
    reply: oneshot::Sender<bool>,
}

/// Control messages to the agent driver task.
enum Control {
    SwitchModel(String),
    SwitchProvider(String, String),
    StartRalph,
    Compact,
    NewSession,
    LoadSession(String),
    /// Drop / restore the last user+assistant exchange from the live session.
    UndoMessage,
    RedoMessage,
    /// Rewind the live session to just before the Nth (0-based) user message — truncates the
    /// conversation there so a resend replaces everything after it (click-to-rewind).
    RewindTo(usize),
}

/// What a submitted keypress resolved to.
enum KeyAction {
    Prompt(String),
    SwitchModel(String),
    /// Switch to a provider already saved in config (resolve base URL + key, then hot-swap).
    SwitchSavedProvider(String),
    /// Finish the `/connect` wizard: persist the provider + key, then activate it.
    ConnectProvider {
        name: String,
        kind: String,
        base_url: String,
        key: String,
    },
    SetGoal(String),
    StartRalph,
    Compact,
    Interrupt,
    NewSession,
    OpenSessions,
    LoadSession(String),
    /// Apply the theme at this index in `THEME_NAMES` (theme picker).
    SetTheme(usize),
    /// Open $EDITOR on the input buffer (<leader>e).
    OpenEditor,
    /// Export the transcript to a markdown file (<leader>x).
    ExportSession,
    /// Copy the last assistant message to the system clipboard via OSC 52 (<leader>y).
    CopyLast,
    /// Remove / restore the last user+assistant exchange (<leader>u / <leader>r).
    MessagesUndo,
    MessagesRedo,
    /// Cycle to the next/previous recently-used model (F2 / shift+F2).
    CycleRecent(i8),
    /// Toggle the active model as a favorite (ctrl+f), persisted to ~/.cordy.
    ToggleFavorite,
    /// Rename / delete / fork a session (ctrl+r · picker `d` · picker `f`).
    RenameSession(String),
    DeleteSession(String),
    ForkSession(String),
    /// From the permission modal: approve and never ask for this tool again (persisted).
    AllowAlways,
    /// Show the active permission rules (`/permissions`).
    ShowPermissions,
    /// Toggle mouse capture — off lets the terminal select/copy text natively (`/mouse`).
    ToggleMouse,
    /// Open the provider manager (`/providers`).
    OpenProviders,
    /// Delete a saved provider by id (from the manager).
    DeleteProvider(String),
    /// Paste from the system clipboard: an image is saved + attached as `@image`, else text.
    PasteClipboard,
}

/// Permission gate backed by the UI: forwards a request to the event loop and awaits the user's
/// y/n decision.
struct TuiPermission {
    tx: mpsc::UnboundedSender<PermissionAsk>,
}

#[async_trait]
impl Permission for TuiPermission {
    async fn request(&self, req: PermissionRequest<'_>) -> bool {
        let (reply, rx) = oneshot::channel();
        if self
            .tx
            .send(PermissionAsk {
                summary: req.summary.to_string(),
                tool: req.tool.to_string(),
                key: req.key.to_string(),
                reply,
            })
            .is_err()
        {
            return false;
        }
        rx.await.unwrap_or(false)
    }
}

/// A provider preset offered by the `/connect` wizard. `base_url = None` means the endpoint is
/// typed by the user (the "Custom…" row).
struct Preset {
    label: &'static str,
    name: &'static str,
    kind: &'static str,
    base_url: Option<&'static str>,
}

/// The provider presets shown in `/connect`, mirroring OpenCode's connect list.
const PRESETS: [Preset; 7] = [
    Preset {
        label: "OpenAI",
        name: "openai",
        kind: "openai-chat",
        base_url: Some("https://api.openai.com/v1"),
    },
    Preset {
        label: "Anthropic (Claude)",
        name: "anthropic",
        kind: "anthropic",
        base_url: Some("https://api.anthropic.com"),
    },
    Preset {
        label: "OpenRouter",
        name: "openrouter",
        kind: "openai-compatible",
        base_url: Some("https://openrouter.ai/api/v1"),
    },
    Preset {
        label: "Groq",
        name: "groq",
        kind: "openai-compatible",
        base_url: Some("https://api.groq.com/openai/v1"),
    },
    Preset {
        label: "NVIDIA",
        name: "nvidia",
        kind: "openai-compatible",
        base_url: Some("https://integrate.api.nvidia.com/v1"),
    },
    Preset {
        label: "Ollama (local)",
        name: "ollama",
        kind: "openai-compatible",
        base_url: Some("http://localhost:11434/v1"),
    },
    Preset {
        label: "Custom…",
        name: "custom",
        kind: "openai-compatible",
        base_url: None,
    },
];

/// The env var holding the API key for a provider family.
fn key_env_for(kind: &str) -> &'static str {
    match kind {
        "anthropic" => "ANTHROPIC_API_KEY",
        _ => "OPENAI_API_KEY",
    }
}

/// Point the process env at a provider endpoint so the (env-driven) provider builder picks it up
/// on the next hot-swap. Setting env is `unsafe` in edition 2024; this is the single writer and
/// runs on the UI thread before the driver rebuilds its provider, so there is no concurrent read.
fn activate_endpoint(kind: &str, base_url: &str, key: &str) {
    unsafe {
        if !base_url.is_empty() {
            std::env::set_var("CORDY_BASE_URL", base_url);
        }
        if !key.is_empty() {
            std::env::set_var(key_env_for(kind), key);
        }
    }
}

/// The user-level API key store (`~/.cordy/keys.json`).
fn key_store(cwd: &std::path::Path) -> ApiKeyStore {
    let path = user_cordy_dir()
        .unwrap_or_else(|| cwd.join(".cordy"))
        .join("keys.json");
    ApiKeyStore::new(path)
}

/// Parse a `tool:glob` rule spec (bare glob → any tool).
fn parse_rule_spec(spec: &str, allow: bool) -> Rule {
    match spec.split_once(':') {
        Some((tool, glob)) => Rule::new(tool.trim(), glob.trim(), allow),
        None => Rule::new("*", spec.trim(), allow),
    }
}

/// Build permission rules from config (deny/allow/`auto`) plus the runtime "allow always" store
/// (`~/.cordy/permissions.json`), so choices persist across launches.
fn permission_rules(config: &Config, cwd: &std::path::Path) -> Vec<Rule> {
    let p = &config.permissions;
    let mut rules: Vec<Rule> = Vec::new();
    for d in &p.deny {
        rules.push(parse_rule_spec(d, false));
    }
    for a in &p.allow {
        rules.push(parse_rule_spec(a, true));
    }
    for a in load_perm_allows(cwd) {
        rules.push(parse_rule_spec(&a, true));
    }
    if p.mode.as_deref() == Some("auto") {
        rules.push(Rule::new("*", "*", true)); // approve everything not explicitly denied
    }
    rules
}

/// The runtime allow-always store (`~/.cordy/permissions.json`), a list of `tool:glob` specs.
fn perm_store_path(cwd: &std::path::Path) -> PathBuf {
    user_cordy_dir()
        .unwrap_or_else(|| cwd.join(".cordy"))
        .join("permissions.json")
}

fn load_perm_allows(cwd: &std::path::Path) -> Vec<String> {
    std::fs::read_to_string(perm_store_path(cwd))
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

/// Append an allow spec to the store (deduped).
fn add_perm_allow(cwd: &std::path::Path, spec: &str) {
    let mut list = load_perm_allows(cwd);
    if list.iter().any(|s| s == spec) {
        return;
    }
    list.push(spec.to_string());
    let path = perm_store_path(cwd);
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(json) = serde_json::to_string_pretty(&list) {
        let _ = std::fs::write(path, json);
    }
}

/// The last-active provider, remembered across launches (`~/.cordy/active.json`).
struct ActiveProvider {
    name: String,
    kind: String,
    base_url: String,
    model: String,
}

fn active_path(cwd: &std::path::Path) -> PathBuf {
    user_cordy_dir()
        .unwrap_or_else(|| cwd.join(".cordy"))
        .join("active.json")
}

/// Load the remembered active provider, if any.
fn load_active(cwd: &std::path::Path) -> Option<ActiveProvider> {
    let text = std::fs::read_to_string(active_path(cwd)).ok()?;
    let v: serde_json::Value = serde_json::from_str(&text).ok()?;
    Some(ActiveProvider {
        name: v["name"].as_str()?.to_string(),
        kind: v["kind"]
            .as_str()
            .unwrap_or("openai-compatible")
            .to_string(),
        base_url: v["base_url"].as_str().unwrap_or("").to_string(),
        model: v["model"].as_str().unwrap_or("").to_string(),
    })
}

/// Persist the active provider so the next launch starts on it.
fn save_active(cwd: &std::path::Path, name: &str, kind: &str, base_url: &str, model: &str) {
    let path = active_path(cwd);
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let v = serde_json::json!({ "name": name, "kind": kind, "base_url": base_url, "model": model });
    if let Ok(json) = serde_json::to_string_pretty(&v) {
        let _ = std::fs::write(path, json);
    }
}

/// Update just the model of the remembered active provider (on a mid-session model switch).
fn update_active_model(cwd: &std::path::Path, model: &str) {
    if let Some(act) = load_active(cwd) {
        save_active(cwd, &act.name, &act.kind, &act.base_url, model);
    }
}

/// Path to the favorited-models list (`~/.cordy/favorites.json`).
fn favorites_path(cwd: &std::path::Path) -> PathBuf {
    user_cordy_dir()
        .unwrap_or_else(|| cwd.join(".cordy"))
        .join("favorites.json")
}

/// Load the favorited-models list.
fn load_favorites(cwd: &std::path::Path) -> Vec<String> {
    std::fs::read_to_string(favorites_path(cwd))
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

/// Persist the favorited-models list.
fn save_favorites(cwd: &std::path::Path, favs: &[String]) {
    let path = favorites_path(cwd);
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(json) = serde_json::to_string_pretty(favs) {
        let _ = std::fs::write(path, json);
    }
}

/// Path to the recently-used-models list (`~/.cordy/recents.json`).
fn recents_path(cwd: &std::path::Path) -> PathBuf {
    user_cordy_dir()
        .unwrap_or_else(|| cwd.join(".cordy"))
        .join("recents.json")
}

/// Load the recently-used-models list.
fn load_recents(cwd: &std::path::Path) -> Vec<String> {
    std::fs::read_to_string(recents_path(cwd))
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

/// Record a model as recently used (newest first, deduped, capped) and persist the list.
fn push_recent(model: &mut Model, name: &str, cwd: &std::path::Path) {
    model.recents.retain(|m| m != name);
    model.recents.insert(0, name.to_string());
    model.recents.truncate(12);
    let path = recents_path(cwd);
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(json) = serde_json::to_string_pretty(&model.recents) {
        let _ = std::fs::write(path, json);
    }
}

/// Where sessions are stored: under `~/.cordy`, namespaced per project path so different
/// checkouts keep separate history without polluting the project directory.
fn session_store_dir(cwd: &std::path::Path) -> PathBuf {
    user_cordy_dir()
        .unwrap_or_else(|| cwd.join(".cordy"))
        .join("sessions")
        .join(project_slug(cwd))
}

/// A stable, filesystem-safe id for a project path: a readable tail plus an FNV-1a hash of the
/// full path (so distinct paths that sanitize alike never collide).
fn project_slug(cwd: &std::path::Path) -> String {
    let full = cwd.to_string_lossy();
    let mut hash: u64 = 0xcbf29ce484222325;
    for b in full.bytes() {
        hash ^= b as u64;
        hash = hash.wrapping_mul(0x100000001b3);
    }
    let tail: String = cwd
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "root".into())
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .take(24)
        .collect();
    let tail = if tail.is_empty() { "root".into() } else { tail };
    format!("{tail}-{hash:016x}")
}

/// Build the active provider from env. `CORDY_PROVIDER` selects the API family.
fn build_provider(model_name: &str) -> Arc<dyn Provider> {
    let kind = std::env::var("CORDY_PROVIDER").unwrap_or_else(|_| "openai-chat".into());
    build_provider_for(&kind, model_name)
}

/// Build a provider for an explicit API family + model (used for mid-session hot-swap). The
/// result is wrapped with retry/backoff for transient failures.
fn build_provider_for(kind: &str, model_name: &str) -> Arc<dyn Provider> {
    let base = std::env::var("CORDY_BASE_URL").ok();
    let inner: Arc<dyn Provider> = match kind {
        "anthropic" => {
            let key = std::env::var("ANTHROPIC_API_KEY").unwrap_or_default();
            let mut p = Anthropic::new(key, model_name);
            if let Some(b) = base {
                p = p.with_base_url(b);
            }
            Arc::new(p)
        }
        "openai-responses" => {
            let key = std::env::var("OPENAI_API_KEY").unwrap_or_default();
            let mut p = OpenAiResponses::new(key, model_name);
            if let Some(b) = base {
                p = p.with_base_url(b);
            }
            Arc::new(p)
        }
        "openai-compatible" => {
            let key = std::env::var("OPENAI_API_KEY").unwrap_or_else(|_| "sk-local".into());
            Arc::new(openai_compatible(
                base.unwrap_or_else(|| "http://localhost:11434/v1".into()),
                key,
                model_name,
            ))
        }
        _ => {
            let key = std::env::var("OPENAI_API_KEY").unwrap_or_default();
            let mut p = OpenAiChat::new(key, model_name);
            if let Some(b) = base {
                p = p.with_base_url(b);
            }
            Arc::new(p)
        }
    };
    Arc::new(RetryProvider::new(inner, 3))
}

/// Path to the user-level config (`~/.cordy/config.toml`), if a home dir is known.
fn user_config_path() -> Option<PathBuf> {
    user_cordy_dir().map(|d| d.join("config.toml"))
}

/// `~/.cordy` — the home for all of the agent's user-level settings (config, key store, model
/// cache), if a home dir is known.
fn user_cordy_dir() -> Option<PathBuf> {
    let home = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE"))?;
    Some(PathBuf::from(home).join(".cordy"))
}

/// The OpenAI-compatible base URL + bearer key for the active provider, for listing `/models`.
/// Returns `None` for providers without an OpenAI-style list endpoint (e.g. Anthropic).
fn endpoint_for(kind: &str) -> Option<(String, String)> {
    let base = std::env::var("CORDY_BASE_URL").ok();
    match kind {
        "anthropic" => None,
        "openai-compatible" => Some((
            base.unwrap_or_else(|| "http://localhost:11434/v1".into()),
            std::env::var("OPENAI_API_KEY").unwrap_or_default(),
        )),
        _ => Some((
            base.unwrap_or_else(|| "https://api.openai.com/v1".into()),
            std::env::var("OPENAI_API_KEY").unwrap_or_default(),
        )),
    }
}

/// Fetch the live `/models` list for the active endpoint plus every configured provider,
/// concurrently, and merge into one deduplicated list. This front-loads model discovery so the
/// picker shows models without first switching to each provider. Degrades to empty per-endpoint.
async fn fetch_all_models(
    config: &Config,
    active_kind: &str,
    cwd: &std::path::Path,
) -> Vec<String> {
    let mut endpoints: Vec<(String, String)> = Vec::new();
    if let Some((base, key)) = endpoint_for(active_kind) {
        endpoints.push((base, key));
    }
    for p in &config.providers {
        if p.kind == "anthropic" {
            continue; // Anthropic has no OpenAI-style /models endpoint
        }
        let base = p.base_url.clone().unwrap_or_default();
        if base.is_empty() {
            continue;
        }
        let key = p
            .api_key_env
            .as_ref()
            .and_then(|e| std::env::var(e).ok())
            .or_else(|| key_store(cwd).get(&p.name))
            .unwrap_or_default();
        endpoints.push((base, key));
    }
    // Dedup endpoints by base URL so a provider that shares the active URL isn't fetched twice.
    endpoints.sort_by(|a, b| a.0.cmp(&b.0));
    endpoints.dedup_by(|a, b| a.0 == b.0);

    let results = futures::future::join_all(
        endpoints
            .into_iter()
            .map(|(base, key)| async move { list_models(&base, &key).await.unwrap_or_default() }),
    )
    .await;

    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    for list in results {
        for m in list {
            if seen.insert(m.clone()) {
                out.push(m);
            }
        }
    }
    out
}

/// Resolve a model's price (per million in/out): config override wins, else models.dev.
fn resolve_price(catalog: &Catalog, config: &Config, name: &str) -> (Option<f64>, Option<f64>) {
    let cfg = config.model(name);
    let cat = catalog.get(name);
    (
        cfg.and_then(|m| m.price_in)
            .or_else(|| cat.and_then(|m| m.price_in)),
        cfg.and_then(|m| m.price_out)
            .or_else(|| cat.and_then(|m| m.price_out)),
    )
}

/// Combined last-modified signature of the config files (for hot-reload polling).
fn config_mtime(paths: &[PathBuf]) -> u64 {
    paths
        .iter()
        .filter_map(|p| std::fs::metadata(p).ok()?.modified().ok())
        .filter_map(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
        .max()
        .unwrap_or(0)
}

/// Build the active theme: a named preset with any `[colors]` hex overrides from config applied.
fn build_theme(name: &str, config: &Config) -> Theme {
    let c = &config.colors;
    theme_by_name(name).with_overrides(&super::theme::ColorOverrides {
        user: c.user.clone(),
        assistant: c.assistant.clone(),
        tool: c.tool.clone(),
        system: c.system.clone(),
        dim: c.dim.clone(),
        accent: c.accent.clone(),
        border: c.border.clone(),
        surface: c.surface.clone(),
        base: c.base.clone(),
    })
}

/// Resolve a model's context window: config override wins, else models.dev.
fn resolve_context(catalog: &Catalog, config: &Config, name: &str) -> Option<u64> {
    config
        .model(name)
        .and_then(|m| m.context_window)
        .or_else(|| catalog.get(name).and_then(|m| m.context))
}

/// A short palette hint for a model: context size, price, and tool support (from models.dev).
fn model_hint(catalog: &Catalog, config: &Config, name: &str) -> String {
    let ctx = config
        .model(name)
        .and_then(|m| m.context_window)
        .or_else(|| catalog.get(name).and_then(|m| m.context));
    let (pin, pout) = resolve_price(catalog, config, name);
    let mut parts: Vec<String> = Vec::new();
    if let Some(c) = ctx {
        parts.push(format!("{} ctx", models_dev::fmt_context(c)));
    }
    if let (Some(i), Some(o)) = (pin, pout) {
        parts.push(format!("${i}/${o}"));
    }
    if catalog.get(name).is_some_and(|m| m.tool_call) {
        parts.push("tools".into());
    }
    if parts.is_empty() {
        "hot-swap model".into()
    } else {
        parts.join(" · ")
    }
}

/// Connect to enabled stdio MCP servers, registering each as a capability source. Returns the
/// live connections, which must be kept alive for the tools to work.
#[cfg(feature = "mcp")]
async fn connect_mcp(
    config: &Config,
    sources: &mut Vec<Arc<dyn CapabilitySource>>,
    statuses: &mut Vec<(String, String)>,
) -> Vec<crate::mcp::McpConnection> {
    use crate::mcp::{McpCapability, connect_http, connect_stdio};
    let mut conns = Vec::new();
    for srv in &config.mcp_servers {
        if !srv.enabled {
            continue;
        }
        let connected = match srv.transport.as_str() {
            "stdio" => match &srv.command {
                Some(cmdline) => {
                    let mut parts = cmdline.split_whitespace();
                    match parts.next() {
                        Some(cmd) => {
                            let args: Vec<String> = parts.map(str::to_string).collect();
                            connect_stdio(&srv.name, cmd, &args, &[]).await
                        }
                        None => continue,
                    }
                }
                None => continue,
            },
            "http" | "streamable-http" | "sse" => match &srv.url {
                Some(url) => connect_http(&srv.name, url).await,
                None => continue,
            },
            _ => continue,
        };
        match connected {
            Ok(conn) => {
                statuses.push((
                    srv.name.clone(),
                    format!("Connected · {} tools", conn.tools.len()),
                ));
                sources.push(Arc::new(McpCapability::new(
                    srv.name.clone(),
                    conn.tools.clone(),
                )));
                conns.push(conn);
            }
            Err(e) => {
                let msg: String = e.to_string().chars().take(60).collect();
                statuses.push((srv.name.clone(), format!("error: {msg}")));
            }
        }
    }
    conns
}

/// Resolve the session to use: resume an existing one (`Some(id)`; empty id = latest) or start
/// fresh. Returns the id, preloaded history, and — for a *new* session — the header to write
/// lazily on the first message (so empty sessions never touch disk).
fn open_session(
    store: &SessionStore,
    resume: &Option<String>,
    provider_kind: &str,
    model: &str,
    cwd: &std::path::Path,
) -> (String, Vec<Message>, Option<SessionMeta>) {
    if let Some(sel) = resume {
        let id = if sel.is_empty() {
            store.latest()
        } else {
            Some(sel.clone())
        };
        if let Some(id) = id
            && let Ok((meta, msgs)) = store.load(&id)
        {
            return (meta.id, msgs, None); // already on disk
        }
    }
    let id = SessionStore::new_id();
    let meta = SessionMeta {
        id: id.clone(),
        provider: provider_kind.to_string(),
        model: model.to_string(),
        cwd: cwd.display().to_string(),
        created_at: now_unix(),
        title: String::new(),
    };
    (id, Vec::new(), Some(meta)) // created lazily on first append
}

/// Launch the interactive TUI. Reads `CORDY_PROVIDER`, `CORDY_MODEL`, `CORDY_BASE_URL`, and the
/// relevant `*_API_KEY` from env. `resume` optionally continues a saved session.
pub async fn run(resume: Option<String>) -> anyhow::Result<()> {
    let cwd = std::env::current_dir()?;

    // Config: project `.cordy/config.toml` merged over the user config.
    let mut config = Config::load(
        user_config_path().as_deref(),
        &cwd.join(".cordy/config.toml"),
    );

    let mut model_name = std::env::var("CORDY_MODEL").unwrap_or_else(|_| "gpt-4o-mini".into());
    let mut provider_kind =
        std::env::var("CORDY_PROVIDER").unwrap_or_else(|_| "openai-chat".into());

    // Restore the last-connected provider (from /connect) so it is the default on launch —
    // unless the user explicitly points the env at an endpoint.
    if std::env::var("CORDY_BASE_URL").is_err()
        && let Some(act) = load_active(&cwd)
    {
        let key = config
            .providers
            .iter()
            .find(|p| p.name == act.name)
            .and_then(|p| p.api_key_env.as_ref())
            .and_then(|e| std::env::var(e).ok())
            .or_else(|| key_store(&cwd).get(&act.name))
            .unwrap_or_default();
        activate_endpoint(&act.kind, &act.base_url, &key);
        // SAFETY: single writer on the UI thread before any provider is built.
        unsafe { std::env::set_var("CORDY_PROVIDER", &act.kind) };
        provider_kind = act.kind.clone();
        if !act.model.is_empty() {
            model_name = act.model.clone();
        }
    }

    let provider = build_provider(&model_name);

    let (perm_tx, mut perm_rx) = mpsc::unbounded_channel::<PermissionAsk>();
    let permission = Arc::new(
        PermissionEngine::new(Arc::new(TuiPermission { tx: perm_tx }))
            .with_sandbox(cwd.clone())
            .with_rules(permission_rules(&config, &cwd)),
    );
    let perm_handle = permission.clone(); // for runtime "allow always" + /permissions
    let ctx = ToolCtx::with_permission(cwd.clone(), permission);

    // UI event channel (created early so sub-agents can surface their activity to it).
    let (agent_tx, mut agent_rx) = mpsc::unbounded_channel::<AgentEvent>();

    // Capability sources: builtin tools + sub-agents (task tool) + skills (+ MCP under feature).
    // All register into one registry and contribute system-prompt fragments.
    let optimizer = Arc::new(Optimizer::new(config.optimize_enabled()));
    // Shared background-job registry; the UI keeps a handle to show a running-jobs count.
    let bg = crate::tools::builtins::BgRegistry::default();
    let base_tools: Arc<dyn CapabilitySource> =
        Arc::new(BuiltinTools::with_bg(optimizer, bg.clone()));
    // Agents: project `.cordy/agents` plus global `~/.cordy/agents` (project wins by name).
    let mut agent_vec = load_agents(&cwd.join(".cordy/agents"));
    if let Some(ud) = user_cordy_dir() {
        let have: std::collections::HashSet<String> =
            agent_vec.iter().map(|d| d.name.clone()).collect();
        for a in load_agents(&ud.join("agents")) {
            if !have.contains(&a.name) {
                agent_vec.push(a);
            }
        }
    }
    let agent_defs = Arc::new(agent_vec);
    let agent_names: Vec<String> = agent_defs.iter().map(|d| d.name.clone()).collect();
    // Sub-agent concurrency cap; the UI derives an "active sub-agents" count from its free permits.
    const SUBAGENT_SLOTS: usize = 4;
    let subagent_sema = Arc::new(Semaphore::new(SUBAGENT_SLOTS));
    let task_tool: Arc<dyn Tool> = Arc::new(SubAgentTool::new(
        provider.clone(),
        base_tools.clone(),
        ctx.clone(),
        subagent_sema.clone(),
        agent_defs.clone(),
        model_name.clone(),
        agent_tx.clone(),
    ));
    // Skills: project `.cordy/skills` plus global `~/.cordy/skills` (project wins by name).
    let mut skills = load_skills(&cwd.join(".cordy/skills"));
    if let Some(ud) = user_cordy_dir() {
        let have: std::collections::HashSet<String> =
            skills.iter().map(|s| s.name.clone()).collect();
        for s in load_skills(&ud.join("skills")) {
            if !have.contains(&s.name) {
                skills.push(s);
            }
        }
    }
    let skill_names: Vec<String> = skills.iter().map(|s| s.name.clone()).collect();
    #[cfg_attr(not(feature = "mcp"), allow(unused_mut))]
    let mut sources: Vec<Arc<dyn CapabilitySource>> = vec![
        base_tools.clone(),
        Arc::new(AgentRegistry::new(agent_defs, task_tool)),
        Arc::new(SkillSet::new(skills)),
    ];

    // MCP servers (feature-gated). Connections are kept alive for the app's lifetime; each
    // server's real status (connected + tool count, or the error) is shown in the panel.
    let mut mcp_names: Vec<(String, String)> = Vec::new();
    #[cfg(feature = "mcp")]
    let _mcp_connections = connect_mcp(&config, &mut sources, &mut mcp_names).await;
    #[cfg(not(feature = "mcp"))]
    for s in config.mcp_servers.iter().filter(|s| s.enabled) {
        mcp_names.push((s.name.clone(), "needs --features mcp".into()));
    }

    let mut reg = Registry::new();
    let mut fragments: Vec<String> = Vec::new();
    for src in &sources {
        for t in src.tools() {
            reg.register(t);
        }
        if let Some(f) = src.prompt_fragment() {
            fragments.push(f);
        }
    }
    let registry = Arc::new(reg);

    // Assemble the system prompt (pi-style): SYSTEM.md override or default base, tools, env,
    // <project_context> files, capability fragments, then APPEND_SYSTEM.md — project overrides
    // and appends live in `.cordy/`, global ones in `~/.cordy/`.
    let tool_names: Vec<String> = registry.specs().into_iter().map(|s| s.name).collect();
    let user_dir = user_cordy_dir();
    let capabilities = if fragments.is_empty() {
        None
    } else {
        Some(fragments.join("\n"))
    };
    let system = build_system_prompt(&PromptContext {
        cwd: &cwd,
        os: std::env::consts::OS,
        tool_names: &tool_names,
        project_context: load_project_context(&cwd),
        custom_prompt: load_system_override(&cwd, user_dir.as_deref()),
        append_prompt: load_system_append(&cwd, user_dir.as_deref()),
        capabilities,
        cognitive_core: config
            .model(&model_name)
            .map(|m| m.cognitive_core)
            .unwrap_or(false),
    });

    // Open (or resume) the persisted session (stored under ~/.cordy, namespaced by project).
    let store = SessionStore::new(session_store_dir(&cwd));
    let (session_id, initial_messages, pending_meta) =
        open_session(&store, &resume, &provider_kind, &model_name, &cwd);
    let mut current_session = session_id.clone(); // tracked in the UI for rename/fork
    let resumed_count = initial_messages.len();
    let subtitle = format!("{provider_kind} · {model_name}");
    let footer = cwd.display().to_string();

    // Agent driver task: owns the session and the (swappable) agent. History persists across
    // turns; a `/model` switch rebuilds the provider while keeping the canonical message history
    // intact — the hot-swap headline feature.
    let (prompt_tx, mut prompt_rx) = mpsc::unbounded_channel::<Vec<ContentBlock>>();
    let (control_tx, mut control_rx) = mpsc::unbounded_channel::<Control>();
    let cancel_slot: CancelSlot = Arc::new(Mutex::new(None));
    {
        let provider_kind = provider_kind.clone();
        let init_model = model_name.clone();
        let ralph_cwd = cwd.clone();
        let cancel_slot = cancel_slot.clone();
        let (price_in, price_out) = config
            .model(&model_name)
            .map(|m| (m.price_in, m.price_out))
            .unwrap_or((None, None));
        let compact_threshold = config.compact_threshold.unwrap_or(100_000);
        let compact_auto = config.compact_mode.as_deref() == Some("auto");
        tokio::spawn(async move {
            let mut agent = AgentLoop::new(provider, registry.clone(), ctx.clone());
            let mut session = Session::new(system, init_model);
            session.messages = initial_messages;
            let mut persisted = session.messages.len();
            let mut session_id = session_id;
            let mut pending_meta = pending_meta; // header written lazily on first message
            let mut redo_groups: Vec<Vec<Message>> = Vec::new();
            loop {
                tokio::select! {
                    maybe = prompt_rx.recv() => {
                        let Some(content) = maybe else { break };
                        session.push_user_content(content);
                        let turn_cancel = CancellationToken::new();
                        if let Ok(mut slot) = cancel_slot.lock() {
                            *slot = Some(turn_cancel.clone());
                        }
                        if let Err(e) = agent.run_turn(&mut session, &agent_tx, &turn_cancel).await {
                            let _ = agent_tx.send(AgentEvent::Error(e.to_string()));
                        }
                        if let Ok(mut slot) = cancel_slot.lock() {
                            *slot = None;
                        }
                        // Create the session file lazily on the first real message so empty
                        // sessions never hit disk.
                        if persisted < session.messages.len()
                            && let Some(meta) = pending_meta.take()
                        {
                            let _ = store.create(&meta);
                        }
                        for m in &session.messages[persisted..] {
                            let _ = store.append(&session_id, m);
                        }
                        persisted = session.messages.len();

                        // Auto-compact when the client-side manager says so.
                        if compact_auto {
                            let native = agent.provider.caps().native_context_mgmt;
                            let cm = ContextManager::new(compact_threshold, CompactMode::Auto, native);
                            if cm.needs_compaction(&session.messages) {
                                compact_session(&agent, &mut session, compact_threshold, &agent_tx).await;
                                persisted = persisted.min(session.messages.len());
                            }
                        }
                    }
                    maybe = control_rx.recv() => {
                        match maybe {
                            Some(Control::SwitchModel(name)) => {
                                let p = build_provider_for(&provider_kind, &name);
                                agent = AgentLoop::new(p, registry.clone(), ctx.clone());
                                session.model = name;
                            }
                            Some(Control::SwitchProvider(kind, name)) => {
                                let p = build_provider_for(&kind, &name);
                                agent = AgentLoop::new(p, registry.clone(), ctx.clone());
                                session.model = name;
                            }
                            Some(Control::StartRalph) => {
                                run_ralph(
                                    &agent,
                                    &mut session,
                                    &ralph_cwd,
                                    price_in,
                                    price_out,
                                    &agent_tx,
                                )
                                .await;
                            }
                            Some(Control::Compact) => {
                                compact_session(&agent, &mut session, compact_threshold, &agent_tx).await;
                                persisted = persisted.min(session.messages.len());
                            }
                            Some(Control::NewSession) => {
                                session.messages.clear();
                                persisted = 0;
                                redo_groups.clear();
                            }
                            Some(Control::UndoMessage) => {
                                if let Some(pos) =
                                    session.messages.iter().rposition(|m| m.role == Role::User)
                                {
                                    let removed = session.messages.split_off(pos);
                                    redo_groups.push(removed);
                                    // Disk is an append-only raw log; don't rewrite it.
                                    persisted = session.messages.len();
                                }
                            }
                            Some(Control::RedoMessage) => {
                                if let Some(group) = redo_groups.pop() {
                                    session.messages.extend(group);
                                    persisted = session.messages.len();
                                }
                            }
                            Some(Control::RewindTo(n)) => {
                                // Truncate at the Nth (0-based) user message so a resend replaces
                                // everything from that message onward.
                                if let Some(pos) = session
                                    .messages
                                    .iter()
                                    .enumerate()
                                    .filter(|(_, m)| m.role == Role::User)
                                    .nth(n)
                                    .map(|(i, _)| i)
                                {
                                    session.messages.truncate(pos);
                                    // Disk is an append-only raw log; don't rewrite it.
                                    persisted = session.messages.len();
                                    redo_groups.clear();
                                }
                            }
                            Some(Control::LoadSession(id)) => {
                                if let Ok((_m, msgs)) = store.load(&id) {
                                    session.messages = msgs;
                                    persisted = session.messages.len();
                                    session_id = id;
                                }
                            }
                            None => {}
                        }
                    }
                }
            }
        });
    }

    // Terminal setup.
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(
        stdout,
        EnterAlternateScreen,
        EnableMouseCapture,
        EnableBracketedPaste
    )?;
    let mut terminal = Terminal::new(CrosstermBackend::new(stdout))?;

    let (input_tx, mut input_rx) = mpsc::unbounded_channel::<Event>();
    spawn_input_reader(input_tx);

    let modes = {
        let mut v = vec!["build".to_string(), "plan".to_string()];
        v.extend(agent_names);
        v
    };

    // Model metadata: the endpoint's live `/v1/models` list + the models.dev catalog (cached),
    // fetched concurrently. Both degrade gracefully to empty on failure.
    let cache_dir = user_cordy_dir()
        .map(|d| d.join("cache"))
        .unwrap_or_else(|| cwd.join(".cordy/cache"));
    // Fetch every configured provider's live model list up-front (concurrently) so the picker
    // shows models without first switching to that provider; merge into one deduplicated list.
    let live_fut = fetch_all_models(&config, &provider_kind, &cwd);
    let update_fut = crate::core::update::check(&cache_dir);
    let (live_models, catalog, latest_version) =
        tokio::join!(live_fut, models_dev::load_catalog(&cache_dir), update_fut);

    let palette = build_palette(
        &config,
        &catalog,
        &live_models,
        &modes,
        &provider_kind,
        &model_name,
    );
    let (init_pin, init_pout) = resolve_price(&catalog, &config, &model_name);
    let mut model = Model {
        status: "ready".into(),
        subtitle,
        footer,
        modes,
        palette,
        show_mascot: config.mascot_enabled(),
        show_thinking: true,
        price_in: init_pin,
        price_out: init_pout,
        context_window: resolve_context(&catalog, &config, &model_name),
        model_name: model_name.clone(),
        provider_kind: provider_kind.clone(),
        statusline: config.statusline.clone(),
        favorites: load_favorites(&cwd),
        recents: {
            let mut r = load_recents(&cwd);
            r.retain(|m| *m != model_name);
            r.insert(0, model_name.clone());
            r.truncate(12);
            r
        },
        animations: config.mascot_enabled(),
        show_tool_output: true,
        skill_names,
        mcp_names,
        theme_name: config
            .theme
            .clone()
            .filter(|t| THEME_NAMES.contains(&t.as_str()))
            .unwrap_or_else(|| "mono".into()),
        latest_version,
        ..Default::default()
    };
    if let Some(v) = &model.latest_version {
        model.transcript.push(Entry::System(format!(
            "↑ cordy v{v} is available (you have v{}) — run: cargo install cordy",
            env!("CARGO_PKG_VERSION")
        )));
    }
    // No intro line here: an empty transcript shows the splash; the input placeholder + hints
    // already guide first use. Any pushed entry (a message or command output) reveals the log.
    if resumed_count > 0 {
        model.transcript.push(Entry::System(format!(
            "resumed session ({resumed_count} messages)"
        )));
    }
    let mut theme_idx = config
        .theme
        .as_deref()
        .and_then(|t| THEME_NAMES.iter().position(|x| *x == t))
        .unwrap_or_else(|| THEME_NAMES.iter().position(|x| *x == "mono").unwrap_or(0));
    let mut theme = build_theme(THEME_NAMES[theme_idx], &config);
    let ui_store = SessionStore::new(session_store_dir(&cwd));
    let mut pending_reply: Option<oneshot::Sender<bool>> = None;
    let mut pending_perm: Option<(String, String)> = None; // (tool, key) for allow-always
    let mut turn_start: Option<std::time::Instant> = None; // for the completion-footer duration
    let mut mouse_on = true; // capture on; toggle off to select/copy natively
    // Config hot-reload: poll the config files' mtime and re-apply live.
    let config_paths: Vec<PathBuf> = [user_config_path(), Some(cwd.join(".cordy/config.toml"))]
        .into_iter()
        .flatten()
        .collect();
    let mut config_sig = config_mtime(&config_paths);
    let mut reload_tick = 0u8;
    let mut ticker = tokio::time::interval(Duration::from_millis(120));

    // `dirty` gates redraws: we only repaint on real changes (or while animating). When mouse
    // capture is off, idle ticks don't repaint, so the terminal's native text selection isn't
    // wiped by our constant redraws.
    let mut dirty = true;
    let result = loop {
        if dirty {
            model.bg_count = bg.running_count();
            model.subagent_count = SUBAGENT_SLOTS.saturating_sub(subagent_sema.available_permits());
            if let Err(e) = terminal.draw(|f| view(f, &mut model, &theme)) {
                break Err(anyhow::Error::from(e));
            }
            dirty = false;
        }
        if model.should_quit {
            break Ok(());
        }

        tokio::select! {
            Some(ev) = input_rx.recv() => {
                dirty = true;
                // Mouse wheel: move the selection in an open picker, else scroll the transcript.
                if let Event::Mouse(m) = &ev {
                    let up = matches!(m.kind, MouseEventKind::ScrollUp);
                    let down = matches!(m.kind, MouseEventKind::ScrollDown);
                    if up || down {
                        let step = |sel: &mut usize, len: usize| {
                            if up {
                                *sel = sel.saturating_sub(1);
                            } else if len > 0 {
                                *sel = (*sel + 1).min(len - 1);
                            }
                        };
                        if model.palette_open {
                            let n = model.palette_filtered().len();
                            step(&mut model.palette_sel, n);
                        } else if model.sessions_open {
                            let n = model.sessions.len();
                            step(&mut model.sessions_sel, n);
                        } else if model.providers_open {
                            let n = model.providers.len();
                            step(&mut model.providers_sel, n);
                        } else if model.theme_open {
                            step(&mut model.theme_sel, THEME_NAMES.len());
                        } else if let Some(c) = &mut model.connect {
                            if matches!(c.step, ConnectStep::Pick) {
                                step(&mut c.sel, PRESETS.len());
                            }
                        } else if up {
                            model.scroll = model.scroll.saturating_add(3);
                        } else {
                            model.scroll = model.scroll.saturating_sub(3);
                        }
                    }
                    // Left-click a past user message to rewind: drop it and everything after from
                    // the display and the live conversation, and reload its text for editing.
                    if let MouseEventKind::Down(MouseButton::Left) = m.kind
                        && !model.busy
                        && no_overlay(&model)
                        && let Some(idx) = entry_at_click(&model, m.column, m.row)
                        && let Some(Entry::User(text)) = model.transcript.get(idx).cloned()
                    {
                        let ordinal = model.transcript[..idx]
                            .iter()
                            .filter(|e| matches!(e, Entry::User(_)))
                            .count();
                        model.transcript.truncate(idx);
                        model.msg_redo.clear();
                        model.input = text;
                        model.cursor = model.input.len();
                        model.anchor = None;
                        model.scroll = 0;
                        model
                            .transcript
                            .push(Entry::System("↩ rewound — edit and press Enter to resend".into()));
                        let _ = control_tx.send(Control::RewindTo(ordinal));
                    }
                }
                // Bracketed paste (or a coalesced key-burst): insert at the cursor, collapsing
                // large blobs to a placeholder. Newlines never submit here.
                if let Event::Paste(text) = &ev {
                    let text = text.clone();
                    insert_paste(&mut model, &text);
                    model.suggestions = compute_suggestions(&model.input, &cwd);
                    model.suggestion_sel = 0;
                }
                if let Event::Key(k) = ev
                    && k.kind != KeyEventKind::Release
                {
                    // ctrl+l forces a full repaint — recovers the screen if a child process wrote
                    // straight to the console (e.g. a stray shell prompt) and desynced the TUI.
                    if k.modifiers.contains(KeyModifiers::CONTROL)
                        && matches!(k.code, KeyCode::Char('l'))
                    {
                        let _ = terminal.clear();
                    }
                    match handle_key(&mut model, k, &mut pending_reply) {
                        Some(KeyAction::Prompt(text)) => {
                            let text = expand_pastes(&mut model, &text);
                            let content = match super::input::parse_user_input(&text, &cwd) {
                                Ok(blocks) => blocks,
                                Err(e) => {
                                    update(&mut model, Msg::Agent(AgentEvent::Error(e)));
                                    vec![ContentBlock::text(text)]
                                }
                            };
                            turn_start = Some(std::time::Instant::now());
                            let _ = prompt_tx.send(content);
                        }
                        Some(KeyAction::SwitchModel(name)) => {
                            model.transcript.push(Entry::System(format!(
                                "switched to model {name} (context kept)"
                            )));
                            (model.price_in, model.price_out) = resolve_price(&catalog, &config, &name);
                            model.context_window = resolve_context(&catalog, &config, &name);
                            model.model_name = name.clone();
                            model.subtitle = format!("{provider_kind} · {name}");
                            push_recent(&mut model, &name, &cwd);
                            update_active_model(&cwd, &name);
                            let _ = control_tx.send(Control::SwitchModel(name));
                        }
                        Some(KeyAction::CycleRecent(dir)) => {
                            if model.recents.len() > 1 {
                                let cur = model
                                    .recents
                                    .iter()
                                    .position(|m| *m == model.model_name)
                                    .unwrap_or(0);
                                let n = model.recents.len() as i32;
                                let idx = (cur as i32 + dir as i32).rem_euclid(n) as usize;
                                let name = model.recents[idx].clone();
                                if name != model.model_name {
                                    (model.price_in, model.price_out) =
                                        resolve_price(&catalog, &config, &name);
                                    model.context_window =
                                        resolve_context(&catalog, &config, &name);
                                    model.model_name = name.clone();
                                    model.subtitle = format!("{provider_kind} · {name}");
                                    push_recent(&mut model, &name, &cwd);
                                    update_active_model(&cwd, &name);
                                    model.transcript.push(Entry::System(format!(
                                        "model → {name} (recent)"
                                    )));
                                    let _ = control_tx.send(Control::SwitchModel(name));
                                }
                            }
                        }
                        Some(KeyAction::ToggleFavorite) => {
                            let name = model.model_name.clone();
                            if let Some(pos) = model.favorites.iter().position(|m| *m == name) {
                                model.favorites.remove(pos);
                                model
                                    .transcript
                                    .push(Entry::System(format!("unfavorited {name}")));
                            } else {
                                model.favorites.push(name.clone());
                                model
                                    .transcript
                                    .push(Entry::System(format!("★ favorited {name}")));
                            }
                            save_favorites(&cwd, &model.favorites);
                        }
                        Some(KeyAction::RenameSession(title)) => {
                            match ui_store.rename(&current_session, title.trim()) {
                                Ok(()) => model
                                    .transcript
                                    .push(Entry::System(format!("session renamed: {}", title.trim()))),
                                Err(e) => model
                                    .transcript
                                    .push(Entry::System(format!("rename failed: {e}"))),
                            }
                        }
                        Some(KeyAction::DeleteSession(id)) => {
                            let _ = ui_store.delete(&id);
                            let now = now_unix();
                            model.sessions = ui_store
                                .list_summaries()
                                .into_iter()
                                .filter(|s| s.messages > 0)
                                .map(|s| (s.meta.id.clone(), summary_label(&s, now)))
                                .collect();
                            model.sessions_sel = model
                                .sessions_sel
                                .min(model.sessions.len().saturating_sub(1));
                            model.transcript.push(Entry::System(format!("deleted session {id}")));
                        }
                        Some(KeyAction::ForkSession(id)) => {
                            match ui_store.fork(&id) {
                                Ok(new_id) => {
                                    if let Ok((_m, msgs)) = ui_store.load(&new_id) {
                                        model.transcript = transcript_from(&msgs);
                                        model.transcript.push(Entry::System(format!(
                                            "forked → {new_id}"
                                        )));
                                        current_session = new_id.clone();
                                        let _ = control_tx.send(Control::LoadSession(new_id));
                                    }
                                    model.sessions_open = false;
                                }
                                Err(e) => model
                                    .transcript
                                    .push(Entry::System(format!("fork failed: {e}"))),
                            }
                        }
                        Some(KeyAction::SwitchSavedProvider(name)) => {
                            let found = config.providers.iter().find(|p| p.name == name).cloned();
                            if let Some(p) = found {
                                let base = p.base_url.clone().unwrap_or_default();
                                let key = p
                                    .api_key_env
                                    .as_ref()
                                    .and_then(|e| std::env::var(e).ok())
                                    .or_else(|| key_store(&cwd).get(&name))
                                    .unwrap_or_default();
                                activate_endpoint(&p.kind, &base, &key);
                                model.provider_kind = p.kind.clone();
                                model.subtitle = format!("{} · {}", p.kind, model.model_name);
                                save_active(&cwd, &name, &p.kind, &base, &model.model_name);
                                refresh_endpoint(&mut model, &config, &catalog, &p.kind, &base, &key, &name).await;
                                let _ = control_tx
                                    .send(Control::SwitchProvider(p.kind.clone(), model.model_name.clone()));
                            } else {
                                model
                                    .transcript
                                    .push(Entry::System(format!("provider {name} not found")));
                            }
                        }
                        Some(KeyAction::ConnectProvider { name, kind, base_url, key }) => {
                            // Persist provider (no secret) + key, and add it to the live config.
                            if let Some(cfgp) = user_config_path() {
                                let _ = crate::config::save_provider(&cfgp, &name, &kind, &base_url);
                            }
                            if !key.is_empty() {
                                let _ = key_store(&cwd).set(&name, &key);
                            }
                            config.providers.retain(|p| p.name != name);
                            config.providers.push(crate::config::ProviderProfile {
                                name: name.clone(),
                                kind: kind.clone(),
                                base_url: Some(base_url.clone()),
                                api_key_env: None,
                            });
                            activate_endpoint(&kind, &base_url, &key);
                            model.provider_kind = kind.clone();
                            model.subtitle = format!("{kind} · {}", model.model_name);
                            save_active(&cwd, &name, &kind, &base_url, &model.model_name);
                            // Validate + fetch models + rebuild palette (so provider & models appear).
                            refresh_endpoint(&mut model, &config, &catalog, &kind, &base_url, &key, &name).await;
                            let _ = control_tx
                                .send(Control::SwitchProvider(kind, model.model_name.clone()));
                        }
                        Some(KeyAction::SetGoal(g)) => {
                            let store = GoalStore::new(cordy_dir(&cwd));
                            match store.set_goal(&g) {
                                Ok(()) => model
                                    .transcript
                                    .push(Entry::System(format!("goal set: {g}"))),
                                Err(e) => model
                                    .transcript
                                    .push(Entry::System(format!("goal: {e}"))),
                            }
                        }
                        Some(KeyAction::StartRalph) => {
                            model.busy = true;
                            model.status = "ralph-loop running…".into();
                            model
                                .transcript
                                .push(Entry::System("ralph-loop started (guardrail-bounded)".into()));
                            let _ = control_tx.send(Control::StartRalph);
                        }
                        Some(KeyAction::Compact) => {
                            model.busy = true;
                            model.status = "compacting…".into();
                            let _ = control_tx.send(Control::Compact);
                        }
                        Some(KeyAction::Interrupt) => {
                            // Cancel the running turn but KEEP the queue — queued messages were
                            // typed intentionally and are sent as one turn once this one unwinds.
                            if let Ok(mut slot) = cancel_slot.lock()
                                && let Some(tok) = slot.take()
                            {
                                tok.cancel();
                            }
                            model.transcript.push(Entry::System("interrupted".into()));
                        }
                        Some(KeyAction::NewSession) => {
                            model.transcript.clear();
                            model.streaming.clear();
                            model.total_in = 0;
                            model.total_out = 0;
                            model.total_saved = 0;
                            model.transcript.push(Entry::System("new session — context cleared".into()));
                            let _ = control_tx.send(Control::NewSession);
                        }
                        Some(KeyAction::OpenSessions) => {
                            let now = now_unix();
                            model.sessions = ui_store
                                .list_summaries()
                                .into_iter()
                                .filter(|s| s.messages > 0)
                                .map(|s| (s.meta.id.clone(), summary_label(&s, now)))
                                .collect();
                            model.sessions_sel = 0;
                            model.sessions_open = true;
                        }
                        Some(KeyAction::OpenProviders) => {
                            model.providers = config
                                .providers
                                .iter()
                                .map(|p| {
                                    (
                                        p.name.clone(),
                                        p.kind.clone(),
                                        p.base_url.clone().unwrap_or_default(),
                                    )
                                })
                                .collect();
                            model.providers_sel = 0;
                            model.providers_open = true;
                        }
                        Some(KeyAction::DeleteProvider(id)) => {
                            if let Some(cfgp) = user_config_path() {
                                let _ = crate::config::remove_provider(&cfgp, &id);
                            }
                            config.providers.retain(|p| p.name != id);
                            model.providers = config
                                .providers
                                .iter()
                                .map(|p| {
                                    (
                                        p.name.clone(),
                                        p.kind.clone(),
                                        p.base_url.clone().unwrap_or_default(),
                                    )
                                })
                                .collect();
                            model.providers_sel = model
                                .providers_sel
                                .min(model.providers.len().saturating_sub(1));
                            model
                                .transcript
                                .push(Entry::System(format!("deleted provider {id}")));
                        }
                        Some(KeyAction::LoadSession(id)) => {
                            if let Ok((_meta, msgs)) = ui_store.load(&id) {
                                model.transcript = transcript_from(&msgs);
                                model.transcript.push(Entry::System(format!("resumed session {id}")));
                                current_session = id.clone();
                                let _ = control_tx.send(Control::LoadSession(id));
                            }
                        }
                        Some(KeyAction::SetTheme(i)) => {
                            theme_idx = i.min(THEME_NAMES.len() - 1);
                            theme = build_theme(THEME_NAMES[theme_idx], &config);
                            model.theme_name = THEME_NAMES[theme_idx].to_string();
                            model
                                .transcript
                                .push(Entry::System(format!("theme: {}", THEME_NAMES[theme_idx])));
                        }
                        Some(KeyAction::AllowAlways) => {
                            if let Some((tool, _key)) = pending_perm.take() {
                                let spec = format!("{tool}:*");
                                perm_handle.add_rule(&tool, "*", true);
                                add_perm_allow(&cwd, &spec);
                                model.transcript.push(Entry::System(format!(
                                    "always allowing {spec} — won't ask again (saved to ~/.cordy/permissions.json)"
                                )));
                            }
                        }
                        Some(KeyAction::ToggleMouse) => {
                            mouse_on = !mouse_on;
                            if mouse_on {
                                let _ = execute!(terminal.backend_mut(), EnableMouseCapture);
                            } else {
                                let _ = execute!(terminal.backend_mut(), DisableMouseCapture);
                            }
                            model.transcript.push(Entry::System(format!(
                                "mouse capture {} — {}",
                                on_off(mouse_on),
                                if mouse_on {
                                    "wheel/click active"
                                } else {
                                    "drag to select & copy (Shift+drag also works)"
                                }
                            )));
                        }
                        Some(KeyAction::ShowPermissions) => {
                            let mut lines = perm_handle.describe_rules();
                            if lines.is_empty() {
                                lines.push("no rules — every risky action asks.".into());
                            }
                            lines.push(String::new());
                            lines.push("· press 'a' at a prompt to allow-always for that tool".into());
                            lines.push("· edit [permissions] in ~/.cordy/config.toml for globs".into());
                            lines.push("  allow=[\"bash:git *\"] deny=[\"bash:rm -rf *\"] mode=\"auto\"".into());
                            model.info = Some(("permissions".into(), lines));
                        }
                        Some(KeyAction::OpenEditor) => {
                            if let Some(text) = edit_in_external_editor(&mut terminal, &model.input) {
                                model.input = text;
                                model.cursor = model.input.len();
                            }
                        }
                        Some(KeyAction::ExportSession) => {
                            let path = cwd.join("cordy-export.md");
                            match std::fs::write(&path, export_markdown(&model.transcript)) {
                                Ok(()) => model
                                    .transcript
                                    .push(Entry::System(format!("exported → {}", path.display()))),
                                Err(e) => model
                                    .transcript
                                    .push(Entry::System(format!("export failed: {e}"))),
                            }
                        }
                        Some(KeyAction::CopyLast) => {
                            if let Some(text) = last_assistant_entry(&model.transcript) {
                                copy_osc52(&text);
                                model
                                    .transcript
                                    .push(Entry::System("copied last reply to clipboard".into()));
                            }
                        }
                        Some(KeyAction::MessagesUndo) => {
                            let mut group = Vec::new();
                            while let Some(e) = model.transcript.pop() {
                                let is_user = matches!(e, Entry::User(_));
                                group.push(e);
                                if is_user {
                                    break;
                                }
                            }
                            if !group.is_empty() {
                                group.reverse();
                                model.msg_redo.push(group);
                                let _ = control_tx.send(Control::UndoMessage);
                            }
                        }
                        Some(KeyAction::MessagesRedo) => {
                            if let Some(group) = model.msg_redo.pop() {
                                model.transcript.extend(group);
                                let _ = control_tx.send(Control::RedoMessage);
                            }
                        }
                        Some(KeyAction::PasteClipboard) => {
                            match paste_clipboard(&mut model, &cwd) {
                                Ok(Some(msg)) => model.transcript.push(Entry::System(msg)),
                                Ok(None) => {}
                                Err(e) => model
                                    .transcript
                                    .push(Entry::System(format!("clipboard paste failed: {e}"))),
                            }
                            model.suggestions = compute_suggestions(&model.input, &cwd);
                            model.suggestion_sel = 0;
                        }
                        None => {}
                    }
                    // Refresh inline autocomplete suggestions after any input change. Reset the
                    // highlight to the top only when the candidate set actually changed, so arrow
                    // navigation (which doesn't edit the input) is preserved.
                    let next = compute_suggestions(&model.input, &cwd);
                    if next != model.suggestions {
                        model.suggestion_sel = 0;
                    } else if !next.is_empty() {
                        model.suggestion_sel = model.suggestion_sel.min(next.len() - 1);
                    }
                    model.suggestions = next;
                }
            }
            Some(a) = agent_rx.recv() => {
                dirty = true;
                let done = matches!(a, AgentEvent::TurnComplete { .. });
                update(&mut model, Msg::Agent(a));
                // Push a completion footer (▣ mode · model · Ns) after each reply.
                if done && let Some(start) = turn_start.take() {
                    let mode = model.mode().to_string();
                    model.transcript.push(Entry::Turn {
                        mode,
                        model: model.model_name.clone(),
                        secs: start.elapsed().as_secs_f64(),
                    });
                }
                // Drain the queue once the turn is idle: combine every message typed while busy
                // into ONE prompt (joined by blank lines) and send it as a single turn, so a burst
                // of queued messages doesn't spawn a wasteful turn each.
                if !model.busy && !model.queue.is_empty() {
                    let text = std::mem::take(&mut model.queue).join("\n\n");
                    model.input = text;
                    if let Effect::Submit(t) = update(&mut model, Msg::Submit) {
                        let t = expand_pastes(&mut model, &t);
                        let content = match super::input::parse_user_input(&t, &cwd) {
                            Ok(blocks) => blocks,
                            Err(e) => {
                                update(&mut model, Msg::Agent(AgentEvent::Error(e)));
                                vec![ContentBlock::text(t)]
                            }
                        };
                        turn_start = Some(std::time::Instant::now());
                        let _ = prompt_tx.send(content);
                    }
                }
            }
            Some(ask) = perm_rx.recv() => {
                dirty = true;
                pending_reply = Some(ask.reply);
                pending_perm = Some((ask.tool, ask.key));
                update(&mut model, Msg::Permission(ask.summary));
            }
            _ = ticker.tick() => {
                update(&mut model, Msg::Tick);
                // Repaint on a tick only while something is animating. When idle we stay quiet so
                // the terminal's native text selection (Shift+drag, or /mouse off) isn't wiped.
                if model.busy || model.mode_flash > 0 || model.bg_count > 0 {
                    dirty = true;
                }

                // Config hot-reload (~ every second): re-apply theme/colors, statusline, mascot,
                // permissions, and the palette when the config files change on disk.
                reload_tick = reload_tick.wrapping_add(1);
                if reload_tick.is_multiple_of(8) {
                    let sig = config_mtime(&config_paths);
                    if sig != config_sig {
                        config_sig = sig;
                        config = Config::load(
                            user_config_path().as_deref(),
                            &cwd.join(".cordy/config.toml"),
                        );
                        if let Some(t) = config.theme.as_deref()
                            && let Some(i) = THEME_NAMES.iter().position(|x| *x == t)
                        {
                            theme_idx = i;
                            model.theme_name = t.to_string();
                        }
                        theme = build_theme(THEME_NAMES[theme_idx], &config);
                        model.statusline = config.statusline.clone();
                        model.show_mascot = config.mascot_enabled();
                        perm_handle.set_rules(permission_rules(&config, &cwd));
                        let modes = model.modes.clone();
                        let pk = model.provider_kind.clone();
                        let mn = model.model_name.clone();
                        model.palette =
                            build_palette(&config, &catalog, &live_models, &modes, &pk, &mn);
                        (model.price_in, model.price_out) =
                            resolve_price(&catalog, &config, &model.model_name);
                        model.context_window =
                            resolve_context(&catalog, &config, &model.model_name);
                        model.transcript.push(Entry::System("config reloaded".into()));
                        dirty = true;
                    }
                }
            }
        }
    };

    bg.kill_all(); // don't leave background dev servers running after Cordy exits

    disable_raw_mode()?;
    execute!(
        terminal.backend_mut(),
        LeaveAlternateScreen,
        DisableMouseCapture,
        DisableBracketedPaste
    )?;
    terminal.show_cursor()?;
    result
}

/// A plain printable key (no ctrl/alt/super) — the kind of event a terminal without bracketed
/// paste emits, one per pasted character. Shift is allowed (capitals / shifted symbols).
fn is_text_key(ev: &Event) -> bool {
    if let Event::Key(k) = ev {
        if k.kind == KeyEventKind::Release {
            return false;
        }
        let plain = !k
            .modifiers
            .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SUPER);
        return plain && matches!(k.code, KeyCode::Char(_) | KeyCode::Enter | KeyCode::Tab);
    }
    false
}

/// A bare Enter/Return keypress (no modifiers) — the ambiguous key that means "submit" when typed
/// but "newline" when it arrives as part of a paste.
fn is_plain_enter(ev: &Event) -> bool {
    matches!(ev, Event::Key(k)
        if k.kind != KeyEventKind::Release
            && k.code == KeyCode::Enter
            && !k.modifiers.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SUPER))
}

/// Forward a burst of text keys: 2+ back-to-back keys are a paste (terminals without bracketed
/// paste), coalesced into one `Event::Paste` so an embedded Enter never submits. A lone key is
/// forwarded unchanged. Returns `false` if the receiver is gone.
fn flush_burst(tx: &mpsc::UnboundedSender<Event>, burst: Vec<Event>) -> bool {
    if burst.len() >= 2 {
        let mut s = String::new();
        for ev in &burst {
            if let Event::Key(k) = ev {
                match k.code {
                    KeyCode::Char(c) => s.push(c),
                    KeyCode::Enter => s.push('\n'),
                    KeyCode::Tab => s.push('\t'),
                    _ => {}
                }
            }
        }
        return tx.send(Event::Paste(s)).is_ok();
    }
    for ev in burst {
        if tx.send(ev).is_err() {
            return false;
        }
    }
    true
}

/// Grace after a coalesced paste during which a lone Enter is treated as a paste straggler
/// (newline) rather than a submit — covers terminals that deliver a paste split across chunks.
const PASTE_GRACE: Duration = Duration::from_millis(60);

fn spawn_input_reader(tx: mpsc::UnboundedSender<Event>) {
    std::thread::spawn(move || {
        // While `Some`, we just flushed a paste and are within the grace window: a bare Enter now is
        // almost certainly a straggler newline from the same paste, not an intentional submit.
        let mut paste_until: Option<std::time::Instant> = None;
        loop {
            match event::poll(Duration::from_millis(100)) {
                Ok(true) => {
                    let Ok(ev) = event::read() else { break };

                    // Absorb a lone Enter that lands right after a paste as a newline.
                    if is_plain_enter(&ev)
                        && paste_until.is_some_and(|d| std::time::Instant::now() < d)
                    {
                        if tx.send(Event::Paste("\n".into())).is_err() {
                            break;
                        }
                        paste_until = Some(std::time::Instant::now() + PASTE_GRACE);
                        continue;
                    }

                    if is_text_key(&ev) {
                        // Gather a run of text keys. The first continuation read is instant (no
                        // latency for lone keystrokes); once a burst is forming we wait a short
                        // grace so paste stragglers — including a trailing Enter delivered a few ms
                        // late — coalesce instead of submitting.
                        let mut burst = vec![ev];
                        let mut trailing = None;
                        loop {
                            let wait = if burst.len() >= 2 {
                                Duration::from_millis(15)
                            } else {
                                Duration::from_millis(0)
                            };
                            match event::poll(wait) {
                                Ok(true) => {
                                    let Ok(next) = event::read() else { break };
                                    if is_text_key(&next) {
                                        burst.push(next);
                                    } else {
                                        trailing = Some(next);
                                        break;
                                    }
                                }
                                _ => break,
                            }
                        }
                        let was_paste = burst.len() >= 2;
                        if !flush_burst(&tx, burst) {
                            break;
                        }
                        paste_until = was_paste.then(|| std::time::Instant::now() + PASTE_GRACE);
                        if let Some(t) = trailing
                            && tx.send(t).is_err()
                        {
                            break;
                        }
                    } else {
                        paste_until = None;
                        if tx.send(ev).is_err() {
                            break;
                        }
                    }
                }
                Ok(false) => {
                    paste_until = None;
                    if tx.is_closed() {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
    });
}

/// Map a character to the physical QWERTY key at its position on a Russian ЙЦУКЕН layout, so
/// keybinds work regardless of the active OS layout. Latin (and unmapped) chars pass through.
fn latinize(c: char) -> char {
    let lower = c.to_lowercase().next().unwrap_or(c);
    let mapped = match lower {
        'й' => 'q',
        'ц' => 'w',
        'у' => 'e',
        'к' => 'r',
        'е' => 't',
        'н' => 'y',
        'г' => 'u',
        'ш' => 'i',
        'щ' => 'o',
        'з' => 'p',
        'х' => '[',
        'ъ' => ']',
        'ф' => 'a',
        'ы' => 's',
        'в' => 'd',
        'а' => 'f',
        'п' => 'g',
        'р' => 'h',
        'о' => 'j',
        'л' => 'k',
        'д' => 'l',
        'ж' => ';',
        'э' => '\'',
        'я' => 'z',
        'ч' => 'x',
        'с' => 'c',
        'м' => 'v',
        'и' => 'b',
        'т' => 'n',
        'ь' => 'm',
        'б' => ',',
        'ю' => '.',
        'ё' => '`',
        _ => return c,
    };
    if c.is_uppercase() {
        mapped.to_ascii_uppercase()
    } else {
        mapped
    }
}

/// [`latinize`] lifted over a [`KeyCode`]: translates `Char(..)`, leaves other keys unchanged.
fn latinize_code(code: KeyCode) -> KeyCode {
    match code {
        KeyCode::Char(c) => KeyCode::Char(latinize(c)),
        other => other,
    }
}

/// No modal/overlay is currently capturing input — safe to act on a raw transcript click.
fn no_overlay(model: &Model) -> bool {
    !model.palette_open
        && !model.sessions_open
        && !model.providers_open
        && !model.theme_open
        && !model.status_open
        && model.connect.is_none()
        && model.pending.is_none()
        && model.info.is_none()
}

/// Map a mouse click at absolute `(col, row)` to the transcript entry under it, using the layout
/// stashed during render. Returns `None` if the click is outside the transcript or hits no entry.
fn entry_at_click(model: &Model, col: u16, row: u16) -> Option<usize> {
    let (rx, ry, rw, rh) = model.transcript_rect?;
    if col < rx || col >= rx + rw || row < ry || row >= ry + rh {
        return None;
    }
    let line_idx = model.transcript_start + (row - ry) as usize;
    model
        .entry_spans
        .iter()
        .position(|&(start, len)| line_idx >= start && line_idx < start + len)
}

/// Handle a keypress with OpenCode-style keybinds. Editing keys mutate the model directly;
/// keys that need runtime side effects return a [`KeyAction`]. Leader key is `ctrl+x`.
fn handle_key(
    model: &mut Model,
    k: KeyEvent,
    pending_reply: &mut Option<oneshot::Sender<bool>>,
) -> Option<KeyAction> {
    use KeyCode::{
        BackTab, Backspace, Char, Delete, Down, End, Enter, Esc, Home, Left, PageDown, PageUp,
        Right, Tab, Up,
    };
    let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
    let alt = k.modifiers.contains(KeyModifiers::ALT);
    let shift = k.modifiers.contains(KeyModifiers::SHIFT);
    // Layout-independent keybinds: on a non-Latin layout (e.g. Cyrillic) crossterm reports the
    // layout character, so map it back to its physical QWERTY key for SHORTCUT matching. Only done
    // for modified keys and the leader chord — plain typing keeps the original char (fix below uses
    // `k.code` in the text-insert arm), so Cyrillic text still types correctly.
    let code = if ctrl || alt {
        latinize_code(k.code)
    } else {
        k.code
    };

    // Leader chord: resolve the key following ctrl+x (see the which-key overlay for the map).
    if model.leader {
        model.leader = false;
        return match latinize_code(k.code) {
            Char('q') => {
                update(model, Msg::Quit);
                None
            }
            Char('n') => Some(KeyAction::NewSession),
            Char('l') => Some(KeyAction::OpenSessions),
            Char('c') => Some(KeyAction::Compact),
            Char('h') => {
                model.transcript.push(Entry::System(HELP_TEXT.into()));
                None
            }
            Char('m') => {
                open_palette_scoped(model, "model:");
                None
            }
            Char('a') => {
                open_palette_scoped(model, "mode:");
                None
            }
            Char('t') => {
                model.theme_open = true;
                model.theme_sel = THEME_NAMES
                    .iter()
                    .position(|t| *t == model.theme_name)
                    .unwrap_or(0);
                None
            }
            Char('b') => {
                model.show_mascot = !model.show_mascot;
                None
            }
            Char('e') => Some(KeyAction::OpenEditor),
            Char('x') => Some(KeyAction::ExportSession),
            Char('y') => Some(KeyAction::CopyLast),
            Char('u') => Some(KeyAction::MessagesUndo),
            Char('r') => Some(KeyAction::MessagesRedo),
            Char('f') => Some(KeyAction::ToggleFavorite),
            Char('s') => {
                model.status_open = !model.status_open;
                None
            }
            _ => None,
        };
    }

    // Status view / info modals: any key closes them.
    if model.status_open {
        model.status_open = false;
        return None;
    }
    if model.info.is_some() {
        model.info = None;
        return None;
    }

    // Theme picker: ↑↓ preview-select, Enter apply, Esc cancel.
    if model.theme_open {
        match k.code {
            Esc => model.theme_open = false,
            KeyCode::Up => model.theme_sel = model.theme_sel.saturating_sub(1),
            KeyCode::Down => model.theme_sel = (model.theme_sel + 1).min(THEME_NAMES.len() - 1),
            Enter => {
                model.theme_open = false;
                return Some(KeyAction::SetTheme(model.theme_sel));
            }
            _ => {}
        }
        return None;
    }

    // Connect wizard captures all keys while open.
    if model.connect.is_some() {
        return handle_connect_key(model, k);
    }

    // Session picker navigation. Enter loads · d deletes · f forks.
    if model.sessions_open {
        match k.code {
            Esc => model.sessions_open = false,
            KeyCode::Up => model.sessions_sel = model.sessions_sel.saturating_sub(1),
            KeyCode::Down => {
                model.sessions_sel =
                    (model.sessions_sel + 1).min(model.sessions.len().saturating_sub(1))
            }
            Enter => {
                model.sessions_open = false;
                if let Some((id, _)) = model.sessions.get(model.sessions_sel) {
                    return Some(KeyAction::LoadSession(id.clone()));
                }
            }
            Char('d') => {
                if let Some((id, _)) = model.sessions.get(model.sessions_sel) {
                    return Some(KeyAction::DeleteSession(id.clone()));
                }
            }
            Char('f') => {
                if let Some((id, _)) = model.sessions.get(model.sessions_sel) {
                    return Some(KeyAction::ForkSession(id.clone()));
                }
            }
            _ => {}
        }
        return None;
    }

    // Provider manager navigation. Enter switches · c connect new · d deletes.
    if model.providers_open {
        match k.code {
            Esc => model.providers_open = false,
            KeyCode::Up => model.providers_sel = model.providers_sel.saturating_sub(1),
            KeyCode::Down => {
                model.providers_sel =
                    (model.providers_sel + 1).min(model.providers.len().saturating_sub(1))
            }
            Enter => {
                model.providers_open = false;
                if let Some((id, _, _)) = model.providers.get(model.providers_sel) {
                    return Some(KeyAction::SwitchSavedProvider(id.clone()));
                }
            }
            Char('c') => {
                model.providers_open = false;
                model.connect = Some(Connect::default());
            }
            Char('d') => {
                if let Some((id, _, _)) = model.providers.get(model.providers_sel) {
                    return Some(KeyAction::DeleteProvider(id.clone()));
                }
            }
            _ => {}
        }
        return None;
    }

    // Command palette navigation (ctrl+p): type to filter, ↑/↓ select, Enter run, Esc close.
    if model.palette_open {
        let filtered = model.palette_filtered();
        match k.code {
            Esc => model.palette_open = false,
            KeyCode::Up => model.palette_sel = model.palette_sel.saturating_sub(1),
            KeyCode::Down => {
                model.palette_sel = (model.palette_sel + 1).min(filtered.len().saturating_sub(1))
            }
            Backspace => {
                model.palette_query.pop();
                model.palette_sel = 0;
            }
            Enter => {
                model.palette_open = false;
                if let Some(&idx) = filtered.get(model.palette_sel) {
                    let action = model.palette[idx].action.clone();
                    return exec_palette(model, action);
                }
            }
            Char(c) if !ctrl && !alt => {
                model.palette_query.push(c);
                model.palette_sel = 0;
            }
            _ => {}
        }
        return None;
    }

    // Permission modal answers: y approve once · a always (persisted) · n deny.
    if model.pending.is_some() {
        match k.code {
            Char('a') | Char('A') => {
                if let Some(reply) = pending_reply.take() {
                    let _ = reply.send(true);
                }
                update(model, Msg::PermissionResolved);
                return Some(KeyAction::AllowAlways);
            }
            Char('y') | Char('Y') | Enter => {
                if let Some(reply) = pending_reply.take() {
                    let _ = reply.send(true);
                }
                update(model, Msg::PermissionResolved);
            }
            Char('n') | Char('N') | Esc => {
                if let Some(reply) = pending_reply.take() {
                    let _ = reply.send(false);
                }
                update(model, Msg::PermissionResolved);
            }
            _ => {}
        }
        return None;
    }

    // Command palette (ctrl+p).
    if ctrl && matches!(k.code, Char('p')) {
        model.palette_open = true;
        model.palette_query.clear();
        model.palette_sel = 0;
        return None;
    }

    // Leader key + global exits.
    if ctrl && matches!(k.code, Char('x')) {
        model.leader = true;
        return None;
    }
    // ctrl+alt message scrolling (line/half-page/page/jump-to-latest). Precedes plain alt arms.
    if ctrl && alt {
        match k.code {
            Char('b') => model.scroll = model.scroll.saturating_add(20),
            Char('f') => model.scroll = model.scroll.saturating_sub(20),
            Char('u') => model.scroll = model.scroll.saturating_add(10),
            Char('d') => model.scroll = model.scroll.saturating_sub(10),
            Char('y') => model.scroll = model.scroll.saturating_add(1),
            Char('e') => model.scroll = model.scroll.saturating_sub(1),
            Char('g') => model.scroll = 0, // jump to latest
            Char('k') => model.leader = !model.leader, // which_key_toggle
            _ => {}
        }
        return None;
    }
    // ctrl+g jumps to the first message (scroll fully up; the view clamps to the top).
    if ctrl && matches!(k.code, Char('g')) {
        model.scroll = u16::MAX;
        return None;
    }
    // ctrl+r renames the current session (prefill the command).
    if ctrl && matches!(k.code, Char('r')) {
        model.input = "/rename ".into();
        model.cursor = model.input.len();
        return None;
    }
    // F2 / shift+F2 cycle recently-used models.
    if matches!(k.code, KeyCode::F(2)) {
        return Some(KeyAction::CycleRecent(if shift { -1 } else { 1 }));
    }
    // ctrl+shift+d deletes the current line; plain ctrl+d exits.
    if ctrl && shift && matches!(code, Char('d') | Char('D')) {
        update(model, Msg::KillLine);
        return None;
    }
    if ctrl && matches!(code, Char('d')) {
        update(model, Msg::Quit);
        return None;
    }
    if ctrl && matches!(code, Char('c')) {
        update(
            model,
            if model.input.is_empty() {
                Msg::Quit
            } else {
                Msg::ClearInput
            },
        );
        return None;
    }
    // Paste an image (or text) from the clipboard. Bound to ctrl+v, alt+v, and ctrl+shift+v —
    // many terminals intercept plain ctrl+v for their own text paste, so alt+v / ctrl+shift+v are
    // the reliable ways to reach the app for a clipboard IMAGE.
    if (ctrl || alt) && matches!(code, Char('v')) {
        return Some(KeyAction::PasteClipboard);
    }

    match code {
        Esc => {
            // Esc dismisses an open autocomplete popup, interrupts a running turn, or does
            // nothing — it never quits the app (quit: ctrl+c on empty input, ctrl+d, ^X q, /quit).
            if !model.suggestions.is_empty() {
                model.suggestions.clear();
                model.suggestion_sel = 0;
                return None;
            }
            if model.busy {
                return Some(KeyAction::Interrupt);
            }
            None
        }
        Tab => {
            // Tab completes the highlighted autocomplete suggestion if any, else cycles the mode.
            if !model.suggestions.is_empty() {
                apply_completion(model);
            } else {
                update(model, Msg::CycleMode(1));
            }
            None
        }
        BackTab => {
            update(model, Msg::CycleMode(-1));
            None
        }
        // --- newline (multiline): ctrl+j, alt/shift+Enter ---
        Char('j') if ctrl => {
            update(model, Msg::Newline);
            None
        }
        Enter if alt || shift => {
            update(model, Msg::Newline);
            None
        }
        Enter => {
            // With the autocomplete popup open, Enter accepts the highlighted suggestion instead
            // of submitting; the next Enter submits.
            if !model.suggestions.is_empty() {
                apply_completion(model);
                return None;
            }
            let raw = model.input.trim().to_string();
            if raw.starts_with('/') {
                update(model, Msg::ClearInput);
                return handle_command(model, &raw);
            }
            match update(model, Msg::Submit) {
                Effect::Submit(text) => Some(KeyAction::Prompt(text)),
                _ => None,
            }
        }
        // --- cursor movement (shift extends the selection: input_select_*) ---
        // ctrl/alt+←→ = word-wise; plain = char; ctrl+b/f = char; alt+b/f = word.
        Left => {
            let m = if ctrl || alt {
                Msg::WordBackward
            } else {
                Msg::Left
            };
            move_with_sel(model, m, shift);
            None
        }
        Right => {
            let m = if ctrl || alt {
                Msg::WordForward
            } else {
                Msg::Right
            };
            move_with_sel(model, m, shift);
            None
        }
        // Up/Down: navigate the autocomplete popup if open; else move across wrapped visual lines,
        // falling back to prompt history only at the top/bottom edge.
        Up => {
            let w = model.input_width as usize;
            if !model.suggestions.is_empty() {
                model.suggestion_sel = model.suggestion_sel.saturating_sub(1);
            } else if super::move_vertical(&model.input, model.cursor, -1, w).is_some() {
                move_with_sel(model, Msg::CursorUp, shift);
            } else {
                update(model, Msg::HistoryPrev);
            }
            None
        }
        Down => {
            let w = model.input_width as usize;
            if !model.suggestions.is_empty() {
                let last = model.suggestions.len().saturating_sub(1);
                model.suggestion_sel = (model.suggestion_sel + 1).min(last);
            } else if super::move_vertical(&model.input, model.cursor, 1, w).is_some() {
                move_with_sel(model, Msg::CursorDown, shift);
            } else {
                update(model, Msg::HistoryNext);
            }
            None
        }
        Home => {
            move_with_sel(model, Msg::Home, shift);
            None
        }
        End => {
            move_with_sel(model, Msg::End, shift);
            None
        }
        Char('b') | Char('B') if alt => {
            move_with_sel(model, Msg::WordBackward, shift);
            None
        }
        Char('f') | Char('F') if alt => {
            move_with_sel(model, Msg::WordForward, shift);
            None
        }
        Char('b') | Char('B') if ctrl => {
            move_with_sel(model, Msg::Left, shift);
            None
        }
        Char('f') | Char('F') if ctrl => {
            move_with_sel(model, Msg::Right, shift);
            None
        }
        Char('a') | Char('A') if ctrl => {
            move_with_sel(model, Msg::LineHome, shift);
            None
        }
        Char('e') | Char('E') if ctrl => {
            move_with_sel(model, Msg::LineEnd, shift);
            None
        }
        // --- deletion ---
        Backspace => {
            update(
                model,
                if ctrl || alt {
                    Msg::KillWordBack
                } else {
                    Msg::Backspace
                },
            );
            None
        }
        Delete => {
            update(
                model,
                if ctrl || alt {
                    Msg::KillWordForward
                } else {
                    Msg::Delete
                },
            );
            None
        }
        Char('d') if alt => {
            update(model, Msg::KillWordForward);
            None
        }
        Char('w') if ctrl => {
            update(model, Msg::KillWordBack);
            None
        }
        Char('u') if ctrl => {
            update(model, Msg::KillToStart);
            None
        }
        Char('k') if ctrl => {
            update(model, Msg::KillToEnd);
            None
        }
        // --- undo/redo (ctrl+- / ctrl+.) ---
        Char('-') if ctrl => {
            update(model, Msg::Undo);
            None
        }
        Char('.') if ctrl => {
            update(model, Msg::Redo);
            None
        }
        // --- transcript scroll ---
        PageUp => {
            model.scroll = model.scroll.saturating_add(5);
            None
        }
        PageDown => {
            model.scroll = model.scroll.saturating_sub(5);
            None
        }
        // --- text ---
        Char(c) if !ctrl && !alt => {
            update(model, Msg::Insert(c));
            None
        }
        _ => None,
    }
}

/// Drive the `/connect` provider wizard. Esc closes it; Enter advances (and finishes with a
/// [`KeyAction::ConnectProvider`] on the last step). Editing keys mutate the current text field.
fn handle_connect_key(model: &mut Model, k: KeyEvent) -> Option<KeyAction> {
    use KeyCode::{Backspace, Char, Down, Enter, Esc, Up};
    let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
    let alt = k.modifiers.contains(KeyModifiers::ALT);
    // Take ownership; re-store unless the wizard closes.
    let mut c = model.connect.take()?;
    if matches!(k.code, Esc) || (ctrl && matches!(k.code, Char('c'))) {
        return None; // closed
    }
    match c.step {
        ConnectStep::Pick => match k.code {
            Up => c.sel = c.sel.saturating_sub(1),
            Down => c.sel = (c.sel + 1).min(PRESETS.len() - 1),
            Enter => {
                c.preset = c.sel;
                match PRESETS[c.preset].base_url {
                    Some(b) => {
                        c.base = b.to_string();
                        c.input = PRESETS[c.preset].name.to_string(); // prefill the name
                        c.step = ConnectStep::Name;
                    }
                    None => c.step = ConnectStep::Url,
                }
            }
            _ => {}
        },
        ConnectStep::Url => match k.code {
            Char(ch) if !ctrl && !alt => c.input.push(ch),
            Backspace => {
                c.input.pop();
            }
            Enter if !c.input.trim().is_empty() => {
                c.base = c.input.trim().to_string();
                c.input = PRESETS[c.preset].name.to_string(); // prefill the name
                c.step = ConnectStep::Name;
            }
            _ => {}
        },
        ConnectStep::Name => match k.code {
            Char(ch) if !ctrl && !alt => c.input.push(ch),
            Backspace => {
                c.input.pop();
            }
            Enter if !c.input.trim().is_empty() => {
                c.name = c.input.trim().to_string();
                c.input.clear();
                c.step = ConnectStep::Key;
            }
            _ => {}
        },
        ConnectStep::Key => match k.code {
            Char(ch) if !ctrl && !alt => c.input.push(ch),
            Backspace => {
                c.input.pop();
            }
            Enter => {
                let p = &PRESETS[c.preset];
                return Some(KeyAction::ConnectProvider {
                    name: provider_id(&c.name),
                    kind: p.kind.to_string(),
                    base_url: c.base.clone(),
                    key: c.input.trim().to_string(),
                }); // wizard closes (state already taken)
            }
            _ => {}
        },
    }
    model.connect = Some(c);
    None
}

/// Derive a filesystem/config-safe provider id from a display name: lowercase, alphanumerics and
/// dashes only (`My Provider!` → `my-provider`).
fn provider_id(name: &str) -> String {
    let mut out = String::new();
    let mut prev_dash = false;
    for ch in name.trim().to_lowercase().chars() {
        if ch.is_ascii_alphanumeric() {
            out.push(ch);
            prev_dash = false;
        } else if !prev_dash && !out.is_empty() {
            out.push('-');
            prev_dash = true;
        }
    }
    let trimmed = out.trim_end_matches('-').to_string();
    if trimmed.is_empty() {
        "provider".into()
    } else {
        trimmed
    }
}

/// Apply a cursor movement, extending the selection when `shift` is held and clearing it
/// otherwise. This turns every plain movement keybind into its `input_select_*` counterpart.
fn move_with_sel(model: &mut Model, msg: Msg, shift: bool) {
    if shift {
        if model.anchor.is_none() {
            model.anchor = Some(model.cursor);
        }
    } else {
        model.anchor = None;
    }
    update(model, msg);
}

/// Open the command palette pre-filtered to a scope (e.g. `"model:"`, `"mode:"`).
fn open_palette_scoped(model: &mut Model, query: &str) {
    model.palette_open = true;
    model.palette_query = query.to_string();
    model.palette_sel = 0;
}

/// Execute a selected palette action, returning a [`KeyAction`] for runtime side effects.
fn exec_palette(model: &mut Model, action: super::PaletteAction) -> Option<KeyAction> {
    use super::PaletteAction;
    match action {
        PaletteAction::Command(cmd) => match cmd.as_str() {
            "/new" => Some(KeyAction::NewSession),
            "/model" | "/goal" | "/rename" => {
                model.input = format!("{cmd} ");
                model.cursor = model.input.len();
                None
            }
            _ => handle_command(model, &cmd),
        },
        PaletteAction::SwitchModel(m) => Some(KeyAction::SwitchModel(m)),
        PaletteAction::SwitchMode(i) => {
            model.mode_idx = i;
            model.mode_flash = 6;
            None
        }
        PaletteAction::SwitchProvider(name) => Some(KeyAction::SwitchSavedProvider(name)),
        PaletteAction::SwitchTheme(i) => Some(KeyAction::SetTheme(i)),
    }
}

/// Common models offered in the palette when the config catalog is empty.
const BUILTIN_MODELS: [&str; 6] = [
    "meta/llama-3.3-70b-instruct",
    "meta/llama-3.1-8b-instruct",
    "qwen/qwen2.5-coder-32b-instruct",
    "deepseek-ai/deepseek-r1",
    "gpt-4o",
    "gpt-4o-mini",
];

/// Validate a just-activated endpoint by fetching its `/v1/models`, report the result to the
/// transcript, and rebuild the palette with the live model list. Anthropic has no OpenAI-style
/// list endpoint, so it is reported as connected without a model fetch.
async fn refresh_endpoint(
    model: &mut Model,
    config: &Config,
    catalog: &Catalog,
    kind: &str,
    base_url: &str,
    key: &str,
    label: &str,
) {
    let modes = model.modes.clone();
    let mname = model.model_name.clone();
    if kind == "anthropic" {
        model.transcript.push(Entry::System(format!(
            "connected {label} (anthropic) — set a model with /model <name>"
        )));
        model.palette = build_palette(config, catalog, &[], &modes, kind, &mname);
        return;
    }
    match list_models(base_url, key).await {
        Ok(models) if !models.is_empty() => {
            model.transcript.push(Entry::System(format!(
                "✓ {label}: {} models available — ^P to pick",
                models.len()
            )));
            model.palette = build_palette(config, catalog, &models, &modes, kind, &mname);
        }
        Ok(_) => {
            model.transcript.push(Entry::System(format!(
                "connected {label}, but it returned no models — check the base URL / key"
            )));
            model.palette = build_palette(config, catalog, &[], &modes, kind, &mname);
        }
        Err(e) => {
            model.transcript.push(Entry::System(format!(
                "⚠ {label} unreachable: {e} — saved anyway; verify key / URL"
            )));
        }
    }
}

/// Build the command-palette rows: commands, modes, models (live `/v1/models` + models.dev
/// metadata), and configured providers.
fn build_palette(
    config: &Config,
    catalog: &Catalog,
    live_models: &[String],
    modes: &[String],
    provider_kind: &str,
    model_name: &str,
) -> Vec<super::PaletteItem> {
    use super::{PaletteAction, PaletteItem};
    let mut items: Vec<PaletteItem> = Vec::new();
    let cmd = |label: &str, hint: &str| PaletteItem {
        label: label.into(),
        hint: hint.into(),
        action: PaletteAction::Command(label.into()),
    };
    items.push(cmd("/help", "keybinds & commands"));
    items.push(cmd("/connect", "add a provider (API key / endpoint)"));
    items.push(cmd("/permissions", "view permission rules"));
    items.push(cmd("/mouse", "toggle mouse capture (off to copy text)"));
    items.push(cmd(
        "/providers",
        "manage providers (switch/connect/delete)",
    ));
    items.push(cmd("/sessions", "switch · delete (d) · fork (f)"));
    items.push(cmd("/rename", "rename the current session"));
    items.push(cmd("/new", "start a fresh session"));
    items.push(cmd("/compact", "summarize history to reclaim context"));
    items.push(cmd("/ralph", "run the autonomous loop toward the goal"));
    items.push(cmd("/goal", "set the autonomous north-star"));
    items.push(cmd("/thinking", "toggle showing model reasoning"));
    items.push(cmd("/tooloutput", "toggle tool output visibility"));
    items.push(cmd("/animations", "toggle UI animations"));
    items.push(cmd("/stash", "stash the current draft"));
    items.push(cmd("/unstash", "restore a stashed draft"));
    items.push(cmd("/skills", "list loaded skills"));
    items.push(cmd("/mcp", "list MCP servers"));
    items.push(cmd("/clear", "clear the transcript"));
    items.push(cmd("/quit", "exit cordy"));

    // Modes / agents.
    for (i, m) in modes.iter().enumerate() {
        items.push(PaletteItem {
            label: format!("mode: {m}"),
            hint: "switch active mode/agent".into(),
            action: PaletteAction::SwitchMode(i),
        });
    }

    // Themes.
    for (i, name) in THEME_NAMES.iter().enumerate() {
        items.push(PaletteItem {
            label: format!("theme: {name}"),
            hint: "switch UI theme".into(),
            action: PaletteAction::SwitchTheme(i),
        });
    }

    // Models: prefer the endpoint's live list, then config, then a small built-in fallback.
    let model_names: Vec<String> = if !live_models.is_empty() {
        live_models.to_vec()
    } else if !config.models.is_empty() {
        config.models.iter().map(|m| m.name.clone()).collect()
    } else {
        BUILTIN_MODELS.iter().map(|s| s.to_string()).collect()
    };
    for m in model_names {
        if m == model_name {
            continue;
        }
        items.push(PaletteItem {
            label: format!("model: {m}"),
            hint: model_hint(catalog, config, &m),
            action: PaletteAction::SwitchModel(m),
        });
    }

    // Providers configured in .cordy/config.toml (connect / switch endpoint). The active one is
    // tagged rather than hidden, so a freshly-connected provider is always visible.
    for p in &config.providers {
        let active = p.kind == provider_kind;
        items.push(PaletteItem {
            label: format!("provider: {}", p.name),
            hint: if active {
                format!("{} · active", p.kind)
            } else {
                format!("connect {} endpoint", p.kind)
            },
            action: PaletteAction::SwitchProvider(p.name.clone()),
        });
    }

    items
}

const HELP_TEXT: &str = "\
keys: Tab/Shift+Tab cycle agent · Enter send · ^J/Alt+Enter newline · Esc interrupt/quit
      ^X leader: q quit · n new · l sessions · c compact · m models · a agents · t theme
                 b mascot · e editor · x export · y copy · s status · f favorite · u/r undo/redo msg
      move: ←/→ char · ^←/Alt+←→ word · ↑/↓ line-or-history · ^A/^E line home/end · Home/End buffer
      edit: ^W/Alt+⌫ del word back · Alt+D del word fwd · ^U/^K kill to start/end · ^Shift+D kill line
            ^- undo · ^. redo · ^C clear/quit · ^L redraw · paste supported
      models: F2/Shift+F2 cycle recent · ^X f favorite
      scroll: wheel · PgUp/PgDn · ^G top · ^Alt+G bottom · ^Alt+U/D half · ^Alt+B/F page · ^Alt+Y/E line
      session: ^R rename · in /sessions: d delete · f fork
commands: /help /clear /quit /model <name> /goal <text> /ralph /compact
          /connect /sessions /rename <title> /new
          /thinking /tooloutput /animations (toggles) · /stash /unstash · /skills /mcp
input: @image <path> attaches an image · @<path> injects a file";

/// Execute a slash command. Returns a [`KeyAction`] for commands the runtime must act on
/// (`/model`, `/goal`); all other commands are handled inline (mutating the model).
fn handle_command(model: &mut Model, raw: &str) -> Option<KeyAction> {
    use super::input::{Command, parse_command};
    if raw.trim() == "/new" {
        return Some(KeyAction::NewSession);
    }
    if raw.trim() == "/sessions" {
        return Some(KeyAction::OpenSessions);
    }
    if raw.trim() == "/connect" {
        model.connect = Some(Connect::default());
        return None;
    }
    if raw.trim() == "/permissions" {
        return Some(KeyAction::ShowPermissions);
    }
    if raw.trim() == "/mouse" {
        return Some(KeyAction::ToggleMouse);
    }
    if raw.trim() == "/providers" {
        return Some(KeyAction::OpenProviders);
    }
    if let Some(rest) = raw.trim().strip_prefix("/rename") {
        let title = rest.trim();
        if title.is_empty() {
            model
                .transcript
                .push(Entry::System("usage: /rename <title>".into()));
            return None;
        }
        return Some(KeyAction::RenameSession(title.to_string()));
    }
    // View toggles + prompt stash + capability listings (OpenCode `none`-key features).
    match raw.trim() {
        "/thinking" => {
            model.show_thinking = !model.show_thinking;
            model.transcript.push(Entry::System(format!(
                "display thinking: {}",
                on_off(model.show_thinking)
            )));
            return None;
        }
        "/tooloutput" => {
            model.show_tool_output = !model.show_tool_output;
            model.transcript.push(Entry::System(format!(
                "tool output: {}",
                on_off(model.show_tool_output)
            )));
            return None;
        }
        "/animations" => {
            model.animations = !model.animations;
            model.transcript.push(Entry::System(format!(
                "animations: {}",
                on_off(model.animations)
            )));
            return None;
        }
        "/stash" => {
            if model.input.trim().is_empty() {
                model
                    .transcript
                    .push(Entry::System("nothing to stash".into()));
            } else {
                let draft = std::mem::take(&mut model.input);
                model.cursor = 0;
                model.stash.push(draft);
                model.transcript.push(Entry::System(format!(
                    "stashed draft ({} in stash)",
                    model.stash.len()
                )));
            }
            return None;
        }
        "/unstash" => {
            match model.stash.pop() {
                Some(draft) => {
                    model.input = draft;
                    model.cursor = model.input.len();
                }
                None => model
                    .transcript
                    .push(Entry::System("stash is empty".into())),
            }
            return None;
        }
        "/skills" => {
            let lines = if model.skill_names.is_empty() {
                vec![
                    "no skills loaded.".to_string(),
                    "add them under .cordy/skills/<name>/SKILL.md (project)".to_string(),
                    "or ~/.cordy/skills/<name>/SKILL.md (global)".to_string(),
                ]
            } else {
                model.skill_names.iter().map(|s| format!("• {s}")).collect()
            };
            model.info = Some(("skills".into(), lines));
            return None;
        }
        "/mcp" => {
            let lines = if model.mcp_names.is_empty() {
                vec!["no MCP servers configured — add [[mcp]] to config.toml".to_string()]
            } else {
                model
                    .mcp_names
                    .iter()
                    .map(|(n, s)| format!("• {n}  {s}"))
                    .collect()
            };
            model.info = Some(("mcp servers".into(), lines));
            return None;
        }
        _ => {}
    }
    match parse_command(raw) {
        Some(Command::Help) => model.transcript.push(Entry::System(HELP_TEXT.into())),
        Some(Command::Clear) => model.transcript.clear(),
        Some(Command::Quit) => model.should_quit = true,
        Some(Command::Compact) => return Some(KeyAction::Compact),
        Some(Command::Model(Some(name))) => return Some(KeyAction::SwitchModel(name)),
        Some(Command::Model(None)) => model
            .transcript
            .push(Entry::System("usage: /model <name> to hot-swap".into())),
        Some(Command::Goal(g)) if !g.trim().is_empty() => {
            return Some(KeyAction::SetGoal(g));
        }
        Some(Command::Goal(_)) => model.transcript.push(Entry::System(
            "usage: /goal <text> to set the autonomous north-star".into(),
        )),
        Some(Command::Ralph) => return Some(KeyAction::StartRalph),
        Some(Command::Unknown(u)) => model
            .transcript
            .push(Entry::System(format!("unknown command: /{u}"))),
        None => {}
    }
    None
}

/// Estimated USD cost for accumulated usage given per-million pricing.
fn usage_cost(u: &Usage, price_in: Option<f64>, price_out: Option<f64>) -> f64 {
    match (price_in, price_out) {
        (Some(i), Some(o)) => {
            (u.input_tokens as f64) / 1e6 * i + (u.output_tokens as f64) / 1e6 * o
        }
        _ => 0.0,
    }
}

/// The last assistant message's joined text.
fn last_assistant_text(messages: &[Message]) -> String {
    for m in messages.iter().rev() {
        if m.role == Role::Assistant {
            return m
                .content
                .iter()
                .filter_map(|b| match b {
                    ContentBlock::Text { text } => Some(text.as_str()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("");
        }
    }
    String::new()
}

/// Client-side context compaction: summarize the oldest messages into one synopsis and keep the
/// recent turns verbatim. No-op for providers that manage context server-side.
async fn compact_session(
    agent: &AgentLoop,
    session: &mut Session,
    threshold: u64,
    agent_tx: &mpsc::UnboundedSender<AgentEvent>,
) {
    let native = agent.provider.caps().native_context_mgmt;
    let cm = ContextManager::new(threshold, CompactMode::Manual, native);
    let (old_owned, keep_owned, n) = match cm.plan_compaction(&session.messages) {
        Some((old, keep)) => (old.to_vec(), keep.to_vec(), old.len()),
        None => {
            let _ = agent_tx.send(AgentEvent::Error("compact: not enough history yet".into()));
            return;
        }
    };

    let req = ChatRequest {
        model: session.model.clone(),
        system: "Summarize the conversation so far concisely: preserve decisions, code changes, \
                 file paths, and open tasks. Output only the summary."
            .into(),
        messages: old_owned,
        tools: Vec::new(),
        max_tokens: Some(1024),
        temperature: None,
    };
    let summary = match agent.provider.stream(req).await {
        Ok(stream) => match assemble(stream).await {
            Ok((msg, _)) => last_assistant_text(&[msg]),
            Err(_) => String::new(),
        },
        Err(e) => {
            let _ = agent_tx.send(AgentEvent::Error(format!("compact: {e}")));
            return;
        }
    };

    let mut compacted = vec![Message {
        role: Role::User,
        content: vec![ContentBlock::text(format!(
            "Summary of earlier conversation:\n{summary}"
        ))],
    }];
    compacted.extend(keep_owned);
    session.messages = compacted;
    let _ = agent_tx.send(AgentEvent::Error(format!(
        "compacted {n} older messages into a summary"
    )));
}

/// Drive the autonomous ralph-loop: each iteration rebuilds a fresh context from the goal + the
/// on-disk progress notes, runs one turn, records progress, and stops on completion or a
/// guardrail (iteration cap / cost cap). The fresh-context-per-iteration is the ralph insight —
/// long tasks don't drown in context because the durable state lives on disk.
async fn run_ralph(
    agent: &AgentLoop,
    session: &mut Session,
    cwd: &std::path::Path,
    price_in: Option<f64>,
    price_out: Option<f64>,
    agent_tx: &mpsc::UnboundedSender<AgentEvent>,
) {
    let store = GoalStore::new(cordy_dir(cwd));
    let Some(goal) = store.goal() else {
        let _ = agent_tx.send(AgentEvent::Error(
            "ralph: no goal — use /goal <text> first".into(),
        ));
        return;
    };
    let guard = Guardrails::default();
    let mut iteration = 0usize;
    loop {
        let progress = store.progress();
        let prompt = iteration_prompt(&goal, &progress);
        session.messages.clear(); // ralph: discard rotting context; goal+progress live on disk
        session.push_user(prompt);
        if let Err(e) = agent
            .run_turn(session, agent_tx, &CancellationToken::new())
            .await
        {
            let _ = agent_tx.send(AgentEvent::Error(format!("ralph: {e}")));
            break;
        }
        iteration += 1;
        let reply = last_assistant_text(&session.messages);
        let updated = format!("{progress}\n\n## iteration {iteration}\n{reply}");
        let _ = store.set_progress(updated.trim_start());

        let spent = usage_cost(&session.total_usage, price_in, price_out);
        let done = is_done(&reply);
        if !guard.should_continue(iteration, spent, done) {
            let reason = guard
                .stop_reason(iteration, spent, done)
                .unwrap_or("stopped");
            let _ = agent_tx.send(AgentEvent::Error(format!(
                "ralph stopped: {reason} (iteration {iteration})"
            )));
            break;
        }
    }
}

// ---- rendering -----------------------------------------------------------------------------

const LOGO: [&str; 6] = [
    " ██████╗ ██████╗ ██████╗ ██████╗ ██╗   ██╗",
    "██╔════╝██╔═══██╗██╔══██╗██╔══██╗╚██╗ ██╔╝",
    "██║     ██║   ██║██████╔╝██║  ██║ ╚████╔╝ ",
    "██║     ██║   ██║██╔══██╗██║  ██║  ╚██╔╝  ",
    "╚██████╗╚██████╔╝██║  ██║██████╔╝   ██║   ",
    " ╚═════╝ ╚═════╝ ╚═╝  ╚═╝╚═════╝    ╚═╝   ",
];

fn view(f: &mut Frame, model: &mut Model, theme: &Theme) {
    let area = f.area();
    // Fill the whole app background with the theme's base color (so themes recolor the chat too).
    f.render_widget(
        Block::default().style(Style::default().bg(theme.base)),
        area,
    );
    // On a wide terminal, reserve a right-hand info panel (OpenCode-style); the chat uses the rest.
    let wide = area.width >= 118;
    let (main_area, panel_area) = if wide {
        let cols = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Min(60), Constraint::Length(34)])
            .split(area);
        (cols[0], Some(cols[1]))
    } else {
        (area, None)
    };

    // The input box grows with the number of logical lines (multiline input), within bounds.
    // +3 = a top breathing row + a blank line + the mode line.
    let input_rows = model.input.split('\n').count().max(1) as u16;
    let input_h = (input_rows + 3).clamp(5, 13);
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(1),          // transcript / splash
            Constraint::Length(input_h), // input box (blank + mode line + content rows)
            Constraint::Length(1),       // hints
            Constraint::Length(1),       // bottom status bar
        ])
        .split(main_area);

    // ---- transcript / splash ----------------------------------------------------------------
    // Any transcript entry (including system/command output) leaves the splash for the log.
    let has_convo = !model.transcript.is_empty() || !model.streaming.is_empty();

    if has_convo || model.busy {
        // Cards are pre-wrapped to the content width, so the Paragraph itself must NOT wrap.
        let cw = (chunks[0].width as usize).saturating_sub(2).max(14);
        let mut lines: Vec<Line> = Vec::new();
        let mut entry_spans: Vec<(usize, usize)> = Vec::with_capacity(model.transcript.len());
        for e in &model.transcript {
            let start_line = lines.len();
            lines.extend(render_entry(e, theme, model.show_tool_output, cw));
            lines.push(Line::raw(""));
            entry_spans.push((start_line, lines.len() - start_line));
        }
        // Live reasoning (when display_thinking is on).
        if model.show_thinking && !model.thinking.is_empty() {
            for l in model.thinking.lines() {
                let line = Line::from(Span::styled(
                    format!("  💭 {l}"),
                    Style::default()
                        .fg(theme.dim)
                        .add_modifier(Modifier::ITALIC),
                ));
                lines.extend(wrap_line(&line, cw));
            }
        }
        if !model.streaming.is_empty() {
            lines.extend(assistant_block(&model.streaming, theme, true, cw));
            lines.push(Line::raw(""));
        }
        // Live activity line under the last message (what the agent is doing right now).
        if model.busy {
            lines.push(live_status_line(model, theme));
        }
        let max = chunks[0].height as usize;
        let total = lines.len();
        // Clamp scroll to the real maximum so an offset can never point past the top of the
        // content — otherwise a big jump-up (ctrl+g) plus fresh output leaves the newest lines
        // unreachable by scrolling down.
        let max_scroll = total.saturating_sub(max) as u16;
        model.scroll = model.scroll.min(max_scroll);
        let end = total
            .saturating_sub(model.scroll as usize)
            .max(max.min(total));
        let start = end.saturating_sub(max);
        // Stash layout for mouse hit-testing (click-to-rewind maps a click row → transcript entry).
        model.transcript_start = start;
        model.transcript_rect = Some((chunks[0].x, chunks[0].y, chunks[0].width, chunks[0].height));
        model.entry_spans = entry_spans;
        let view_lines = lines[start..end].to_vec();
        f.render_widget(
            Paragraph::new(view_lines).block(Block::default().padding(Padding::new(1, 1, 0, 0))),
            chunks[0],
        );
        // Scrollbar on the right edge when the transcript overflows the viewport.
        if total > max {
            let mut sb_state = ScrollbarState::new(total.saturating_sub(max)).position(start);
            f.render_stateful_widget(
                Scrollbar::new(ScrollbarOrientation::VerticalRight)
                    .begin_symbol(None)
                    .end_symbol(None)
                    .thumb_style(Style::default().fg(theme.accent))
                    .track_style(Style::default().fg(theme.border)),
                chunks[0],
                &mut sb_state,
            );
        }
    } else {
        model.transcript_rect = None;
        model.entry_spans.clear();
        f.render_widget(splash(chunks[0].height, theme), chunks[0]);
    }

    // Compact mascot top-right of the chat — only when there's no side panel to house the info.
    if model.show_mascot && panel_area.is_none() {
        render_mascot(f, chunks[0], model);
    }

    // Wide layout: the OpenCode-style info panel on the right.
    if let Some(p) = panel_area {
        render_side_panel(f, p, model, theme);
    }

    // ---- input box (left-accent bar + prompt + mode line) -----------------------------------
    // Text columns available per input row (excludes borders/padding + the 2-col prompt gutter);
    // drives wrapped-line cursor navigation. Keep in sync with the input block's Padding below.
    model.input_width = chunks[1].width.saturating_sub(6).max(8);
    let mut input_content = build_input_lines(model, theme);
    // Queued messages (typed while busy) shown above the prompt, dispatched in order on turn end.
    if !model.queue.is_empty() {
        let mut q: Vec<Line> = model
            .queue
            .iter()
            .map(|m| {
                let preview: String = m.chars().take(60).collect();
                let ell = if m.chars().count() > 60 { "…" } else { "" };
                Line::from(Span::styled(
                    format!("⧗ queued: {preview}{ell}"),
                    Style::default()
                        .fg(theme.dim)
                        .add_modifier(Modifier::ITALIC),
                ))
            })
            .collect();
        q.push(Line::raw(""));
        q.extend(input_content);
        input_content = q;
    }
    let mode_style = if model.mode_flash > 0 {
        Style::default()
            .fg(theme.accent)
            .add_modifier(Modifier::BOLD | Modifier::REVERSED)
    } else {
        Style::default()
            .fg(theme.accent)
            .add_modifier(Modifier::BOLD)
    };
    let mode_line = Line::from(vec![
        Span::styled(format!(" {} ", model.mode()), mode_style),
        Span::styled(
            format!("  ·  {}  ·  Tab to switch mode", model.subtitle),
            Style::default().fg(theme.dim),
        ),
    ]);
    input_content.push(Line::raw(""));
    input_content.push(mode_line);
    let input = Paragraph::new(input_content).block(
        Block::default()
            .borders(Borders::LEFT)
            .border_style(Style::default().fg(theme.accent))
            .style(Style::default().bg(theme.surface)) // raised surface, matches the dropdown
            .padding(Padding::new(2, 1, 1, 0)), // top breathing room above the prompt
    );
    f.render_widget(input, chunks[1]);

    // ---- inline autocomplete dropdown (above the input, OpenCode-style) ----------------------
    if !model.suggestions.is_empty()
        && !model.palette_open
        && !model.sessions_open
        && model.pending.is_none()
    {
        let n = model.suggestions.len().min(8);
        let h = n as u16;
        let panel_w = 62u16.min(chunks[1].width.saturating_sub(2)).max(20);
        let rect = Rect {
            x: chunks[1].x + 1,
            y: chunks[1].y.saturating_sub(h),
            width: panel_w,
            height: h,
        };
        f.render_widget(Clear, rect);
        let block = Block::default()
            .style(Style::default().bg(theme.surface))
            .padding(Padding::horizontal(1));
        let inner = block.inner(rect);
        f.render_widget(block, rect);
        let iw = inner.width as usize;
        let rows: Vec<Line> = model
            .suggestions
            .iter()
            .take(n)
            .enumerate()
            .map(|(i, s)| {
                modal_row(
                    iw,
                    "",
                    s,
                    slash_desc(s),
                    "",
                    i == model.suggestion_sel,
                    theme,
                )
            })
            .collect();
        f.render_widget(Paragraph::new(rows), inner);
    }

    // ---- hints (right-aligned) --------------------------------------------------------------
    f.render_widget(
        Paragraph::new(Line::from(Span::styled(
            "Tab complete/mode · ^P commands · ^X l sessions · Esc interrupt ",
            Style::default().fg(theme.dim),
        )))
        .alignment(Alignment::Right),
        chunks[2],
    );

    // ---- bottom status bar (minimal) --------------------------------------------------------
    let spin = if model.busy {
        format!("{} ", super::spinner_frame(model.tick))
    } else {
        String::new()
    };
    if let Some(tpl) = &model.statusline {
        f.render_widget(
            Paragraph::new(Line::from(Span::styled(
                format!(" {}", render_statusline(tpl, model, &spin)),
                Style::default().fg(theme.dim),
            ))),
            chunks[3],
        );
    } else {
        // Left: cwd (only when there's no side panel to show it) + spinner/status.
        let mut left: Vec<Span> = Vec::new();
        if panel_area.is_none() {
            left.push(Span::styled(
                format!(" {}  ", model.footer),
                Style::default().fg(theme.dim),
            ));
        } else {
            left.push(Span::raw(" "));
        }
        if !spin.is_empty() || model.status != "ready" {
            left.push(Span::styled(
                format!("{spin}{}", model.status),
                Style::default().fg(theme.accent),
            ));
        }
        // Right: tokens (context %) + a single hint.
        let pct = match model.context_window {
            Some(w) if w > 0 && model.last_in > 0 => (model.last_in * 100 / w).min(100),
            _ => 0,
        };
        let toks = model.last_in.max(model.total_in);
        let right = Line::from(Span::styled(
            format!("{} ({pct}%) ", fmt_num(toks)),
            Style::default().fg(theme.dim),
        ));
        f.render_widget(Paragraph::new(Line::from(left)), chunks[3]);
        f.render_widget(Paragraph::new(right).alignment(Alignment::Right), chunks[3]);
    }

    // ---- session picker ---------------------------------------------------------------------
    if model.sessions_open {
        let popup = centered_rect(64, 62, area);
        let (body, w) = modal_shell(f, popup, "Sessions", theme);
        let surf = theme.surface;
        let mut rows: Vec<Line> = Vec::new();
        if model.sessions.is_empty() {
            rows.push(Line::from(Span::styled(
                "  no saved sessions",
                Style::default().fg(theme.dim).bg(surf),
            )));
        } else {
            let sel = model
                .sessions_sel
                .min(model.sessions.len().saturating_sub(1));
            let avail = body.height as usize;
            let scroll = (sel + 1).saturating_sub(avail);
            for (i, (_id, label)) in model.sessions.iter().enumerate().skip(scroll).take(avail) {
                rows.push(modal_row(w, "", label, "", "", i == sel, theme));
            }
        }
        f.render_widget(Paragraph::new(rows), body);
    }

    // ---- provider manager (/providers) ------------------------------------------------------
    if model.providers_open {
        let popup = centered_rect(66, 62, area);
        let (body, w) = modal_shell(f, popup, "Providers", theme);
        let surf = theme.surface;
        let mut rows: Vec<Line> = Vec::new();
        if model.providers.is_empty() {
            rows.push(Line::from(Span::styled(
                "no providers — press c to connect one",
                Style::default().fg(theme.dim).bg(surf),
            )));
        } else {
            let sel = model
                .providers_sel
                .min(model.providers.len().saturating_sub(1));
            for (i, (id, kind, base)) in model.providers.iter().enumerate() {
                let hint = format!("{kind} · {base}");
                rows.push(modal_row(w, "", id, &hint, "", i == sel, theme));
            }
        }
        rows.push(Line::from(Span::styled(" ", Style::default().bg(surf))));
        rows.push(Line::from(Span::styled(
            "Enter switch · c connect · d delete · esc close",
            Style::default().fg(theme.dim).bg(surf),
        )));
        f.render_widget(Paragraph::new(rows), body);
    }

    // ---- connect provider wizard ------------------------------------------------------------
    if let Some(c) = &model.connect {
        render_connect(f, area, c, theme);
    }

    // ---- which-key overlay (leader pending) -------------------------------------------------
    if model.leader {
        render_which_key(f, area, theme);
    }

    // ---- status view (<leader>s) ------------------------------------------------------------
    if model.status_open {
        render_status(f, area, model, theme);
    }

    // ---- theme picker (<leader>t) -----------------------------------------------------------
    if model.theme_open {
        render_theme_picker(f, area, model, theme);
    }

    // ---- info modal (/skills, /mcp) ---------------------------------------------------------
    if let Some((title, lines)) = &model.info {
        render_info(f, area, title, lines, theme);
    }

    // ---- command palette (ctrl+p): search box + filtered rows -------------------------------
    if model.palette_open {
        render_palette(f, area, model, theme);
    }

    // ---- permission modal -------------------------------------------------------------------
    if let Some(summary) = &model.pending {
        let surf = theme.surface;
        // Show at most ~14 lines of the request (diffs can be long) then the action row.
        let mut body: Vec<Line> = summary
            .lines()
            .take(14)
            .map(|l| {
                Line::from(Span::styled(
                    l.to_string(),
                    Style::default().fg(diff_color(l, theme)).bg(surf),
                ))
            })
            .collect();
        if summary.lines().count() > 14 {
            body.push(Line::from(Span::styled(
                format!("  … {} more lines", summary.lines().count() - 14),
                Style::default().fg(theme.border).bg(surf),
            )));
        }
        body.push(Line::from(Span::styled(" ", Style::default().bg(surf))));
        let hint = |key: &str, label: &str, c: Color| {
            vec![
                Span::styled(
                    key.to_string(),
                    Style::default().fg(c).bg(surf).add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    format!(" {label}   "),
                    Style::default().fg(theme.dim).bg(surf),
                ),
            ]
        };
        let mut row = hint(" y ", "approve", theme.assistant);
        row.extend(hint(" a ", "always (this tool)", theme.accent));
        row.extend(hint(" n ", "deny", theme.system));
        body.push(Line::from(row));

        // Size the panel to its content (incl. the 2-row header), centered.
        let content_h = body.len() as u16 + 4;
        let h = content_h.clamp(7, (area.height * 7 / 10).max(7));
        let longest = body.iter().map(|l| l.width() as u16).max().unwrap_or(40);
        let w = (longest + 6).clamp(44, area.width.saturating_sub(4));
        let rect = Rect {
            x: area.x + area.width.saturating_sub(w) / 2,
            y: area.y + area.height.saturating_sub(h) / 2,
            width: w,
            height: h,
        };
        let (bodyrect, _w) = modal_shell(f, rect, "Permission", theme);
        f.render_widget(Paragraph::new(body).wrap(Wrap { trim: false }), bodyrect);
    }
}

/// Suspend the TUI, open `$VISUAL`/`$EDITOR` (or a platform default) on the input buffer, then
/// restore the TUI and return the edited text. Returns `None` on any failure.
fn edit_in_external_editor(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    initial: &str,
) -> Option<String> {
    let file = std::env::temp_dir().join("cordy-input.md");
    std::fs::write(&file, initial).ok()?;
    let editor = std::env::var("VISUAL")
        .or_else(|_| std::env::var("EDITOR"))
        .unwrap_or_else(|_| {
            if cfg!(windows) {
                "notepad".into()
            } else {
                "vi".into()
            }
        });

    let _ = disable_raw_mode();
    let _ = execute!(
        terminal.backend_mut(),
        LeaveAlternateScreen,
        DisableMouseCapture
    );
    let status = std::process::Command::new(&editor).arg(&file).status();
    let _ = enable_raw_mode();
    let _ = execute!(
        terminal.backend_mut(),
        EnterAlternateScreen,
        EnableMouseCapture
    );
    let _ = terminal.clear();

    match status {
        Ok(_) => std::fs::read_to_string(&file)
            .ok()
            .map(|s| s.trim_end_matches('\n').to_string()),
        Err(_) => None,
    }
}

/// Render the transcript as a Markdown document for `session_export` (<leader>x).
fn export_markdown(transcript: &[Entry]) -> String {
    let mut out = String::from("# Cordy session\n\n");
    for e in transcript {
        match e {
            Entry::User(t) => out.push_str(&format!("## You\n\n{t}\n\n")),
            Entry::Assistant(t) => out.push_str(&format!("## Assistant\n\n{t}\n\n")),
            Entry::Tool { name, text, .. } => {
                out.push_str(&format!("### tool: {name}\n\n```\n{text}\n```\n\n"))
            }
            Entry::System(t) => out.push_str(&format!("> {t}\n\n")),
            Entry::Turn { .. } => {}
        }
    }
    out
}

/// The most recent assistant message text, if any.
fn last_assistant_entry(transcript: &[Entry]) -> Option<String> {
    transcript.iter().rev().find_map(|e| match e {
        Entry::Assistant(t) => Some(t.clone()),
        _ => None,
    })
}

/// Copy `text` to the system clipboard using the OSC 52 terminal escape (no external dependency;
/// works in kitty/iterm/wezterm/tmux with clipboard enabled).
fn copy_osc52(text: &str) {
    use base64::Engine;
    use std::io::Write;
    let b64 = base64::engine::general_purpose::STANDARD.encode(text);
    let seq = format!("\x1b]52;c;{b64}\x07");
    let mut out = io::stdout();
    let _ = out.write_all(seq.as_bytes());
    let _ = out.flush();
}

/// A generic scrollable info modal (used by `/skills` and `/mcp`). Any key closes it.
fn render_info(f: &mut Frame, area: Rect, title: &str, lines: &[String], theme: &Theme) {
    let popup = centered_rect(52, 54, area);
    let (body, _w) = modal_shell(f, popup, title, theme);
    let surf = theme.surface;
    let mut rows: Vec<Line> = lines
        .iter()
        .map(|l| {
            Line::from(Span::styled(
                l.clone(),
                Style::default().fg(theme.user).bg(surf),
            ))
        })
        .collect();
    rows.push(Line::from(Span::styled(" ", Style::default().bg(surf))));
    rows.push(Line::from(Span::styled(
        "any key to close",
        Style::default().fg(theme.dim).bg(surf),
    )));
    f.render_widget(Paragraph::new(rows).wrap(Wrap { trim: false }), body);
}

const HL_BG: Color = Color::Rgb(232, 174, 128);
const HL_FG: Color = Color::Rgb(24, 26, 32);

/// The theme picker (`<leader>t`): theme list with a live color swatch per row.
fn render_theme_picker(f: &mut Frame, area: Rect, model: &Model, theme: &Theme) {
    let popup = centered_rect(46, 44, area);
    let (body, w) = modal_shell(f, popup, "Theme", theme);
    let surf = theme.surface;
    let rows: Vec<Line> = THEME_NAMES
        .iter()
        .enumerate()
        .map(|(i, name)| {
            let selected = i == model.theme_sel;
            let (bg, fg) = if selected {
                (HL_BG, HL_FG)
            } else {
                (surf, theme.user)
            };
            let t = theme_by_name(name);
            let bullet = if selected { "● " } else { "  " };
            let mut spans = vec![Span::styled(
                format!("{bullet}{name:<10}"),
                Style::default().bg(bg).fg(fg).add_modifier(Modifier::BOLD),
            )];
            for c in [t.user, t.assistant, t.tool, t.accent] {
                spans.push(Span::styled("██", Style::default().fg(c).bg(bg)));
            }
            let used = 2 + 10 + 8;
            spans.push(Span::styled(
                " ".repeat(w.saturating_sub(used)),
                Style::default().bg(bg),
            ));
            Line::from(spans)
        })
        .collect();
    f.render_widget(Paragraph::new(rows), body);
}

/// The status view (`<leader>s`): a snapshot of the session's model, context, and cost.
fn render_status(f: &mut Frame, area: Rect, model: &Model, theme: &Theme) {
    let popup = centered_rect(56, 60, area);
    let (body, _w) = modal_shell(f, popup, "Status", theme);
    let surf = theme.surface;
    let label = Style::default().fg(theme.dim).bg(surf);
    let value = Style::default().fg(theme.user).bg(surf);
    let row = |k: &str, v: String| {
        Line::from(vec![
            Span::styled(format!("{k:<12}"), label),
            Span::styled(v, value),
        ])
    };
    let dash = |s: String| if s.is_empty() { "—".into() } else { s };
    let fav = if model.favorites.contains(&model.model_name) {
        " ★"
    } else {
        ""
    };
    let rows = vec![
        row("model", format!("{}{fav}", model.model_name)),
        row("provider", model.provider_kind.clone()),
        row("mode", model.mode().to_string()),
        row("context", dash(model.ctx_line())),
        row(
            "tokens",
            format!("{} in / {} out", model.total_in, model.total_out),
        ),
        row("saved", format!("~{}", model.total_saved)),
        row("cost", dash(model.cost_str())),
        row("cwd", model.footer.clone()),
        row("history", format!("{} prompts", model.history.len())),
        Line::from(Span::styled(" ", Style::default().bg(surf))),
        Line::from(Span::styled("any key to close", label)),
    ];
    f.render_widget(Paragraph::new(rows).wrap(Wrap { trim: false }), body);
}

/// The which-key overlay: the leader (`ctrl+x`) chord map, shown while a leader press is pending.
fn render_which_key(f: &mut Frame, area: Rect, theme: &Theme) {
    const CHORDS: [(&str, &str); 16] = [
        ("q", "quit"),
        ("n", "new session"),
        ("l", "sessions"),
        ("c", "compact"),
        ("m", "models"),
        ("a", "agents"),
        ("t", "theme"),
        ("b", "mascot"),
        ("e", "editor"),
        ("x", "export"),
        ("y", "copy reply"),
        ("s", "status"),
        ("f", "favorite"),
        ("h", "help"),
        ("u", "undo msg"),
        ("r", "redo msg"),
    ];
    let surf = theme.surface;
    let cols = 2;
    let rows_per_col = CHORDS.len().div_ceil(cols);
    let mut lines: Vec<Line> = Vec::new();
    for r in 0..rows_per_col {
        let mut spans = Vec::new();
        for c in 0..cols {
            if let Some((key, label)) = CHORDS.get(c * rows_per_col + r) {
                spans.push(Span::styled(
                    format!(" {key} "),
                    Style::default()
                        .fg(theme.accent)
                        .bg(surf)
                        .add_modifier(Modifier::BOLD),
                ));
                spans.push(Span::styled(
                    format!(" {label:<16}"),
                    Style::default().fg(theme.dim).bg(surf),
                ));
            }
        }
        lines.push(Line::from(spans));
    }
    let h = rows_per_col as u16 + 3;
    let w = 46u16.min(area.width);
    let rect = Rect {
        x: area.x + area.width.saturating_sub(w) / 2,
        y: area.y + area.height.saturating_sub(h + 6),
        width: w,
        height: h,
    };
    let (body, _w) = modal_shell(f, rect, "Leader  ^X", theme);
    f.render_widget(Paragraph::new(lines), body);
}

/// Render the `/connect` provider wizard modal for the current step.
fn render_connect(f: &mut Frame, area: Rect, c: &Connect, theme: &Theme) {
    let popup = centered_rect(62, 66, area);
    let (body, w) = modal_shell(f, popup, "Connect provider", theme);
    let surf = theme.surface;
    let dim = Style::default().fg(theme.dim).bg(surf);
    let bright = Style::default().fg(theme.user).bg(surf);
    let bold = bright.add_modifier(Modifier::BOLD);
    let mut rows: Vec<Line> = Vec::new();

    match c.step {
        ConnectStep::Pick => {
            for (i, p) in PRESETS.iter().enumerate() {
                let url = p.base_url.unwrap_or("custom endpoint");
                rows.push(modal_row(w, "", p.label, url, "", i == c.sel, theme));
            }
        }
        ConnectStep::Url => {
            rows.push(Line::from(Span::styled(
                format!("{} · custom endpoint", PRESETS[c.preset].label),
                dim,
            )));
            rows.push(Line::from(Span::styled(" ", Style::default().bg(surf))));
            rows.push(Line::from(vec![
                Span::styled("Base URL  ", bold),
                Span::styled(c.input.clone(), bright),
                Span::styled("▌", Style::default().fg(theme.accent).bg(surf)),
            ]));
            rows.push(Line::from(Span::styled(" ", Style::default().bg(surf))));
            rows.push(Line::from(Span::styled(
                "e.g. https://api.example.com/v1",
                dim,
            )));
        }
        ConnectStep::Name => {
            rows.push(Line::from(Span::styled(c.base.clone(), dim)));
            rows.push(Line::from(Span::styled(" ", Style::default().bg(surf))));
            rows.push(Line::from(vec![
                Span::styled("Name      ", bold),
                Span::styled(c.input.clone(), bright),
                Span::styled("▌", Style::default().fg(theme.accent).bg(surf)),
            ]));
            rows.push(Line::from(Span::styled(" ", Style::default().bg(surf))));
            rows.push(Line::from(Span::styled(
                format!("id → {}", provider_id(&c.input)),
                dim,
            )));
        }
        ConnectStep::Key => {
            rows.push(Line::from(vec![
                Span::styled(format!("{}  ", c.name), bright),
                Span::styled(c.base.clone(), dim),
            ]));
            rows.push(Line::from(Span::styled(" ", Style::default().bg(surf))));
            let masked = "•".repeat(c.input.chars().count());
            rows.push(Line::from(vec![
                Span::styled("API key   ", bold),
                Span::styled(masked, bright),
                Span::styled("▌", Style::default().fg(theme.accent).bg(surf)),
            ]));
            rows.push(Line::from(Span::styled(" ", Style::default().bg(surf))));
            rows.push(Line::from(Span::styled(
                "Enter to connect · key stored in ~/.cordy/keys.json",
                dim,
            )));
        }
    }
    rows.push(Line::from(Span::styled(" ", Style::default().bg(surf))));
    rows.push(Line::from(Span::styled(
        "↑↓ select · Enter next · Esc cancel",
        dim,
    )));
    f.render_widget(Paragraph::new(rows).wrap(Wrap { trim: false }), body);
}

/// Substitute the status-line template placeholders with live values. `spin` is the already-
/// rendered spinner glyph (empty when idle).
fn render_statusline(tpl: &str, model: &Model, spin: &str) -> String {
    tpl.replace("{cwd}", &model.footer)
        .replace("{model}", &model.model_name)
        .replace("{provider}", &model.provider_kind)
        .replace("{mode}", model.mode())
        .replace(
            "{tokens}",
            &format!("{} in / {} out", model.total_in, model.total_out),
        )
        .replace("{cost}", &model.cost_str())
        .replace("{ctx}", &model.ctx_line())
        .replace("{saved}", &format!("~{}", model.total_saved))
        .replace("{status}", &model.status)
        .replace("{spinner}", spin.trim_end())
        .replace("{bg}", &model.bg_count.to_string())
        .replace("{agents}", &model.subagent_count.to_string())
        .replace("{version}", env!("CARGO_PKG_VERSION"))
}

/// A live "what the agent is doing now" line: spinner + current activity with animated dots.
fn live_status_line(model: &Model, theme: &Theme) -> Line<'static> {
    let base = model.status.trim_end_matches(['.', '…', ' ']);
    let base = if base.is_empty() { "thinking" } else { base };
    let dots = ".".repeat(1 + (model.tick as usize / 3) % 3);
    Line::from(vec![
        Span::styled(
            format!("  {} ", super::spinner_frame(model.tick)),
            Style::default().fg(theme.accent),
        ),
        Span::styled(
            format!("{base}{dots}"),
            Style::default()
                .fg(theme.dim)
                .add_modifier(Modifier::ITALIC),
        ),
    ])
}

/// One-line description for a slash command in the autocomplete dropdown (empty for `@` paths).
fn slash_desc(cmd: &str) -> &'static str {
    match cmd {
        "/help" => "keybinds & commands",
        "/model" => "hot-swap model",
        "/goal" => "set the autonomous north-star",
        "/ralph" => "run the autonomous loop",
        "/compact" => "summarize history",
        "/clear" => "clear the transcript",
        "/quit" => "exit cordy",
        "/new" => "start a fresh session",
        "/sessions" => "switch session",
        "/connect" => "add a provider",
        "/rename" => "rename session",
        "/thinking" => "toggle reasoning",
        "/tooloutput" => "toggle tool output",
        "/animations" => "toggle animations",
        "/stash" => "stash draft",
        "/unstash" => "restore draft",
        "/skills" => "list skills",
        "/mcp" => "list MCP servers",
        "/permissions" => "view permissions",
        "/mouse" => "toggle mouse (off = select/copy)",
        "/providers" => "manage providers",
        _ => "",
    }
}

/// Slash commands offered by inline autocomplete.
const SLASH_COMMANDS: [&str; 21] = [
    "/help",
    "/model",
    "/goal",
    "/ralph",
    "/compact",
    "/clear",
    "/quit",
    "/new",
    "/sessions",
    "/connect",
    "/rename",
    "/thinking",
    "/tooloutput",
    "/animations",
    "/stash",
    "/unstash",
    "/skills",
    "/mcp",
    "/permissions",
    "/mouse",
    "/providers",
];

/// "on" / "off" for a toggle message.
fn on_off(b: bool) -> &'static str {
    if b { "on" } else { "off" }
}

/// A compact relative age like `5m ago` / `2h ago` / `3d ago`.
fn rel_time(then: u64, now: u64) -> String {
    let d = now.saturating_sub(then);
    if d < 60 {
        "just now".into()
    } else if d < 3600 {
        format!("{}m ago", d / 60)
    } else if d < 86400 {
        format!("{}h ago", d / 3600)
    } else {
        format!("{}d ago", d / 86400)
    }
}

/// A one-line picker label: last-activity time · last user message · model.
fn summary_label(s: &SessionSummary, now: u64) -> String {
    let when = rel_time(s.updated, now);
    let title = if !s.meta.title.is_empty() {
        s.meta.title.clone()
    } else if !s.last_user.is_empty() {
        let mut t: String = s.last_user.chars().take(48).collect();
        if s.last_user.chars().count() > 48 {
            t.push('…');
        }
        t
    } else {
        format!("{} msgs", s.messages)
    };
    format!("{when:<10} · {title}  ·  {}", s.meta.model)
}

/// Rebuild transcript entries (user/assistant text) from stored messages.
fn transcript_from(messages: &[Message]) -> Vec<Entry> {
    let mut v = Vec::new();
    for m in messages {
        let text: String = m
            .content
            .iter()
            .filter_map(|b| match b {
                ContentBlock::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("");
        match m.role {
            Role::User if !text.is_empty() => v.push(Entry::User(text)),
            Role::Assistant if !text.is_empty() => v.push(Entry::Assistant(text)),
            _ => {}
        }
    }
    v
}

/// Inline autocomplete candidates for the current input (slash commands or `@` file paths).
fn compute_suggestions(input: &str, cwd: &std::path::Path) -> Vec<String> {
    if input.starts_with('/') && !input.contains(char::is_whitespace) {
        return SLASH_COMMANDS
            .iter()
            .filter(|c| c.starts_with(input) && **c != input)
            .map(|c| c.to_string())
            .collect();
    }
    if let Some(tok) = input.rsplit(char::is_whitespace).next()
        && let Some(partial) = tok.strip_prefix('@')
    {
        return file_suggestions(cwd, partial);
    }
    Vec::new()
}

/// File/dir suggestions under `cwd` matching a partial `@`-path.
fn file_suggestions(cwd: &std::path::Path, partial: &str) -> Vec<String> {
    let (dir, base) = match partial.rfind('/') {
        Some(i) => (partial[..=i].to_string(), partial[i + 1..].to_string()),
        None => (String::new(), partial.to_string()),
    };
    let scan = if dir.is_empty() {
        cwd.to_path_buf()
    } else {
        cwd.join(&dir)
    };
    let mut out = Vec::new();
    if let Ok(rd) = std::fs::read_dir(&scan) {
        for e in rd.flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            if name.starts_with('.') && !base.starts_with('.') {
                continue;
            }
            if name.to_lowercase().starts_with(&base.to_lowercase()) {
                let is_dir = e.file_type().map(|t| t.is_dir()).unwrap_or(false);
                let suffix = if is_dir { "/" } else { "" };
                out.push(format!("@{dir}{name}{suffix}"));
            }
        }
    }
    out.sort();
    out.truncate(8);
    out
}

/// Pastes at/above this many characters are collapsed to a `[pasted N chars]` placeholder.
const PASTE_COLLAPSE_MIN: usize = 300;

/// Insert pasted `text` into the input. Line endings are normalized (CRLF / lone CR → `\n`, kept as
/// newlines, never a submit). A large paste is stashed and replaced by a `[#id pasted N chars]`
/// placeholder that `expand_pastes` re-inflates on submit.
fn insert_paste(model: &mut Model, text: &str) {
    let norm = text.replace("\r\n", "\n").replace('\r', "\n");
    let count = norm.chars().count();
    if count >= PASTE_COLLAPSE_MIN {
        let id = model.paste_seq;
        model.paste_seq = model.paste_seq.wrapping_add(1);
        let token = format!("[#{id} pasted {count} chars]");
        model.pastes.insert(id, norm);
        for ch in token.chars() {
            update(model, Msg::Insert(ch));
        }
    } else {
        for ch in norm.chars() {
            update(
                model,
                if ch == '\n' {
                    Msg::Newline
                } else {
                    Msg::Insert(ch)
                },
            );
        }
    }
}

/// Replace every `[#id pasted N chars]` placeholder in `text` with its stashed blob, consuming the
/// entries from `model.pastes`. Called on submit so the model receives the full pasted content.
fn expand_pastes(model: &mut Model, text: &str) -> String {
    if !text.contains("[#") || model.pastes.is_empty() {
        return text.to_string();
    }
    let mut out = text.to_string();
    let ids: Vec<u32> = model.pastes.keys().copied().collect();
    for id in ids {
        // Match the exact token shape produced by `insert_paste` for this id.
        if let Some(blob) = model.pastes.get(&id) {
            let count = blob.chars().count();
            let token = format!("[#{id} pasted {count} chars]");
            if out.contains(&token) {
                let blob = model.pastes.remove(&id).unwrap();
                out = out.replace(&token, &blob);
            }
        }
    }
    out
}

/// Paste from the system clipboard. An image is encoded to PNG under `~/.cordy/cache/pasted/` and a
/// ` @image <path>` token is appended to the input (reusing the existing vision pipeline);
/// otherwise clipboard text is inserted via [`insert_paste`]. Returns a status line to surface.
fn paste_clipboard(model: &mut Model, cwd: &std::path::Path) -> anyhow::Result<Option<String>> {
    let mut cb = arboard::Clipboard::new()?;
    // Prefer an image when the clipboard holds one.
    if let Ok(img) = cb.get_image() {
        let dir = user_cordy_dir()
            .map(|d| d.join("cache/pasted"))
            .unwrap_or_else(|| cwd.join(".cordy/cache/pasted"));
        std::fs::create_dir_all(&dir)?;
        let id = model.paste_seq;
        model.paste_seq = model.paste_seq.wrapping_add(1);
        let path = dir.join(format!("clip-{id}.png"));
        write_png(&path, img.width, img.height, &img.bytes)?;
        let token = format!(" @image {}", path.display());
        for ch in token.chars() {
            update(model, Msg::Insert(ch));
        }
        return Ok(Some(format!(
            "📎 image attached ({}×{}) — send to include it",
            img.width, img.height
        )));
    }
    // No image — fall back to clipboard text.
    match cb.get_text() {
        Ok(text) if !text.is_empty() => {
            insert_paste(model, &text);
            Ok(None)
        }
        _ => Ok(Some("clipboard is empty".into())),
    }
}

/// Write RGBA8 pixels to a PNG file.
fn write_png(
    path: &std::path::Path,
    width: usize,
    height: usize,
    rgba: &[u8],
) -> anyhow::Result<()> {
    let file = std::fs::File::create(path)?;
    let mut enc = png::Encoder::new(std::io::BufWriter::new(file), width as u32, height as u32);
    enc.set_color(png::ColorType::Rgba);
    enc.set_depth(png::BitDepth::Eight);
    enc.write_header()?.write_image_data(rgba)?;
    Ok(())
}

/// Replace the current token with the highlighted autocomplete suggestion.
fn apply_completion(model: &mut Model) {
    let sel = model
        .suggestion_sel
        .min(model.suggestions.len().saturating_sub(1));
    let Some(first) = model.suggestions.get(sel).cloned() else {
        return;
    };
    if model.input.starts_with('/') && !model.input.contains(char::is_whitespace) {
        model.input = format!("{first} ");
    } else {
        let idx = model
            .input
            .rfind(char::is_whitespace)
            .map(|i| i + 1)
            .unwrap_or(0);
        model.input.truncate(idx);
        model.input.push_str(&first);
        if !first.ends_with('/') {
            model.input.push(' ');
        }
    }
    model.cursor = model.input.len();
}

/// The (possibly multiline) input rendered with a `❯` prompt on the first line, a continuation
/// gutter on the rest, and a blinking block cursor at the edit position.
fn build_input_lines(model: &Model, theme: &Theme) -> Vec<Line<'static>> {
    if model.input.is_empty() && !model.busy {
        return vec![Line::from(vec![
            Span::styled("❯ ", Style::default().fg(theme.accent)),
            Span::styled(
                "Ask Cordy to build something…  (ctrl+j newline · ↑ history)",
                Style::default().fg(theme.dim),
            ),
        ])];
    }
    let cur = model.cursor.min(model.input.len());
    let sel = model.anchor.map(|a| (a.min(cur), a.max(cur)));
    // Solid cursor (no blink) so the UI stays still when idle — a blinking cursor would force a
    // redraw every tick, which wipes the terminal's native text selection.
    let blink_on = true;
    let cursor_style = Style::default().add_modifier(Modifier::REVERSED);
    let sel_style = Style::default()
        .fg(theme.accent)
        .add_modifier(Modifier::UNDERLINED);
    let prompt = |r: usize| {
        Span::styled(
            if r == 0 { "❯ " } else { "  " },
            Style::default().fg(theme.accent),
        )
    };

    // Render char by char (by byte offset) so the cursor block and the selection highlight land
    // exactly, across logical lines and wrapped visual rows. Wrapping mirrors `visual_rows` in the
    // model (hard wrap every `input_width` chars) so ↑/↓ cursor motion matches what's drawn.
    let width = if model.input_width == 0 {
        usize::MAX
    } else {
        model.input_width as usize
    };
    let mut out: Vec<Line> = Vec::new();
    let mut row = 0usize;
    let mut col = 0usize;
    let mut spans = vec![prompt(0)];
    for (i, ch) in model.input.char_indices() {
        if ch == '\n' {
            if i == cur && blink_on {
                spans.push(Span::styled(" ", cursor_style));
            }
            out.push(Line::from(std::mem::take(&mut spans)));
            row += 1;
            col = 0;
            spans.push(prompt(row));
            continue;
        }
        if col == width {
            out.push(Line::from(std::mem::take(&mut spans)));
            row += 1;
            col = 0;
            spans.push(prompt(row));
        }
        let in_sel = sel.is_some_and(|(s, e)| i >= s && i < e);
        let style = if i == cur && blink_on {
            cursor_style
        } else if in_sel {
            sel_style
        } else {
            Style::default()
        };
        spans.push(Span::styled(ch.to_string(), style));
        col += 1;
    }
    if cur == model.input.len() && blink_on {
        spans.push(Span::styled(" ", cursor_style));
    }
    out.push(Line::from(spans));
    out
}

/// A tiny one-line mascot face `[o_o]`; only the eyes animate while busy (looks around / blinks).
fn mascot_face(tick: u64, busy: bool) -> &'static str {
    if !busy {
        "[o_o]"
    } else {
        match (tick / 4) % 4 {
            0 => "[o_o]",
            1 => "[o_O]",
            2 => "[-_-]",
            _ => "[^_^]",
        }
    }
}

/// Render the compact one-line mascot in the top-right corner.
fn render_mascot(f: &mut Frame, area: Rect, model: &Model) {
    const W: u16 = 5;
    if area.width < W + 2 || area.height < 2 {
        return;
    }
    let rect = Rect {
        x: area.x + area.width - W - 1,
        y: area.y,
        width: W,
        height: 1,
    };
    let color = if !model.busy {
        Color::Rgb(120, 130, 145)
    } else if (model.tick / 2).is_multiple_of(2) {
        Color::Rgb(245, 160, 80)
    } else {
        Color::Rgb(200, 110, 50)
    };
    f.render_widget(
        Paragraph::new(Line::from(Span::styled(
            mascot_face(model.tick, model.busy),
            Style::default().fg(color).add_modifier(Modifier::BOLD),
        ))),
        rect,
    );
}

/// The OpenCode-style right info panel (wide terminals): session, Context, Model, MCP, LSP; with
/// the cwd + version anchored at the bottom. Borderless save for a faint left rule.
fn render_side_panel(f: &mut Frame, area: Rect, model: &Model, theme: &Theme) {
    let block = Block::default()
        .borders(Borders::LEFT)
        .border_style(Style::default().fg(theme.surface))
        .padding(Padding::new(2, 2, 1, 1));
    let inner = block.inner(area);
    f.render_widget(block, area);
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(1), Constraint::Length(2)])
        .split(inner);

    let dim = Style::default().fg(theme.dim);
    let bright = Style::default().fg(theme.user);
    let head = |label: &str| {
        Line::from(Span::styled(
            label.to_string(),
            Style::default().fg(theme.user).add_modifier(Modifier::BOLD),
        ))
    };
    let mut lines: Vec<Line> = Vec::new();

    lines.push(Line::from(Span::styled(
        "Cordy",
        Style::default()
            .fg(theme.accent)
            .add_modifier(Modifier::BOLD),
    )));
    lines.push(Line::raw(""));

    // Context.
    lines.push(head("Context"));
    let pct = match model.context_window {
        Some(w) if w > 0 && model.last_in > 0 => (model.last_in * 100 / w).min(100),
        _ => 0,
    };
    lines.push(Line::from(Span::styled(
        format!(
            "{} tokens",
            group_thousands(model.last_in.max(model.total_in))
        ),
        bright,
    )));
    lines.push(Line::from(Span::styled(format!("{pct}% used"), dim)));
    let cost = model.cost_str();
    lines.push(Line::from(Span::styled(
        format!(
            "{} spent",
            if cost.is_empty() {
                "$0.00".into()
            } else {
                cost
            }
        ),
        dim,
    )));
    lines.push(Line::raw(""));

    // Model.
    lines.push(head("Model"));
    lines.push(Line::from(Span::styled(model.model_name.clone(), bright)));
    lines.push(Line::from(Span::styled(model.provider_kind.clone(), dim)));
    lines.push(Line::raw(""));

    // Activity — only when something is running.
    if model.bg_count > 0 || model.subagent_count > 0 {
        lines.push(head("Activity"));
        if model.bg_count > 0 {
            lines.push(Line::from(vec![
                Span::styled("• ", Style::default().fg(theme.tool)),
                Span::styled(format!("{} background job(s)", model.bg_count), bright),
            ]));
        }
        if model.subagent_count > 0 {
            lines.push(Line::from(vec![
                Span::styled("• ", Style::default().fg(theme.tool)),
                Span::styled(format!("{} sub-agent(s)", model.subagent_count), bright),
            ]));
        }
        lines.push(Line::raw(""));
    }

    // MCP.
    lines.push(head("MCP"));
    if model.mcp_names.is_empty() {
        lines.push(Line::from(Span::styled("no servers configured", dim)));
    } else {
        for (name, status) in &model.mcp_names {
            let dot = if status.starts_with("Connected") {
                theme.assistant
            } else {
                theme.system
            };
            lines.push(Line::from(vec![
                Span::styled("• ", Style::default().fg(dot)),
                Span::styled(name.clone(), bright),
                Span::styled(format!("  {status}"), dim),
            ]));
        }
    }
    lines.push(Line::raw(""));

    // LSP.
    lines.push(head("LSP"));
    lines.push(Line::from(Span::styled(
        "LSPs are disabled",
        dim.add_modifier(Modifier::ITALIC),
    )));

    f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), rows[0]);

    // Bottom: cwd + version (+ an update badge when a newer release is available).
    let mut version_spans = vec![
        Span::styled("• ", Style::default().fg(theme.accent)),
        Span::styled(
            format!("cordy v{}", env!("CARGO_PKG_VERSION")),
            Style::default().fg(theme.user).add_modifier(Modifier::BOLD),
        ),
    ];
    if let Some(v) = &model.latest_version {
        version_spans.push(Span::styled(
            format!("  ↑ v{v}"),
            Style::default()
                .fg(theme.accent)
                .add_modifier(Modifier::BOLD),
        ));
    }
    let footer = vec![
        Line::from(Span::styled(model.footer.clone(), dim)),
        Line::from(version_spans),
    ];
    f.render_widget(Paragraph::new(footer), rows[1]);
}

/// Short human number: `1.2k`, `3.4M`.
fn fmt_num(n: u64) -> String {
    if n >= 1_000_000 {
        format!("{:.1}M", n as f64 / 1e6)
    } else if n >= 1_000 {
        format!("{:.1}k", n as f64 / 1e3)
    } else {
        n.to_string()
    }
}

/// Group a number with a space every 3 digits: `13 091`.
fn group_thousands(n: u64) -> String {
    let s = n.to_string();
    let b = s.as_bytes();
    let mut out = String::new();
    for (i, c) in b.iter().enumerate() {
        if i > 0 && (b.len() - i).is_multiple_of(3) {
            out.push(' ');
        }
        out.push(*c as char);
    }
    out
}

/// The empty-state splash: a vertically-centered CORDY logo + tagline.
fn splash(height: u16, theme: &Theme) -> Paragraph<'static> {
    let block_h = LOGO.len() + 3;
    let top = (height as usize).saturating_sub(block_h) / 2;
    let mut lines: Vec<Line> = vec![Line::raw(""); top];
    for l in LOGO {
        lines.push(Line::styled(l.to_string(), Style::default().fg(theme.dim)));
    }
    lines.push(Line::raw(""));
    lines.push(Line::styled(
        "your terminal coding agent",
        Style::default()
            .fg(theme.accent)
            .add_modifier(Modifier::BOLD),
    ));
    lines.push(Line::from(vec![
        Span::styled("◆ ", Style::default().fg(Color::Rgb(245, 120, 45))),
        Span::styled("powered by redstone.md", Style::default().fg(theme.dim)),
    ]));
    Paragraph::new(lines).alignment(Alignment::Center)
}

/// Color a diff line inside the permission modal.
fn diff_color(line: &str, theme: &Theme) -> Color {
    match line.chars().next() {
        Some('+') => theme.assistant,
        Some('-') => theme.system,
        _ => theme.dim,
    }
}

/// A message rendered with a colored left gutter and a role badge.
/// Word-wrap a styled [`Line`] to `width` columns, preserving each span's style. Breaks at the
/// last space before the limit, else hard-breaks. Returns at least one line.
fn wrap_line(line: &Line, width: usize) -> Vec<Line<'static>> {
    let cells: Vec<(char, Style)> = line
        .spans
        .iter()
        .flat_map(|sp| sp.content.chars().map(move |c| (c, sp.style)))
        .collect();
    if cells.is_empty() {
        return vec![Line::raw("")];
    }
    if width == 0 {
        return vec![cells_to_line(&cells)];
    }
    let mut out = Vec::new();
    let mut start = 0;
    while start < cells.len() {
        let hard = (start + width).min(cells.len());
        let mut end = hard;
        let mut skip = 0;
        if hard < cells.len()
            && let Some(pos) = (start..hard).rev().find(|&i| cells[i].0 == ' ')
            && pos > start
        {
            end = pos;
            skip = 1;
        }
        out.push(cells_to_line(&cells[start..end]));
        start = end + skip;
    }
    if out.is_empty() {
        out.push(Line::raw(""));
    }
    out
}

/// Group a run of `(char, style)` cells into a [`Line`], merging consecutive same-style runs.
fn cells_to_line(cells: &[(char, Style)]) -> Line<'static> {
    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut buf = String::new();
    let mut cur: Option<Style> = None;
    for (ch, st) in cells {
        match cur {
            Some(s) if s == *st => buf.push(*ch),
            _ => {
                if let Some(s) = cur {
                    spans.push(Span::styled(std::mem::take(&mut buf), s));
                }
                buf.push(*ch);
                cur = Some(*st);
            }
        }
    }
    if let Some(s) = cur {
        spans.push(Span::styled(buf, s));
    }
    Line::from(spans)
}

/// Largest content width regardless of terminal size — keeps long lines readable.
const MAX_CARD: usize = 96;

/// A user message: a full-width filled "surface" block with a left accent bar (mirrors the input).
fn user_block(text: &str, theme: &Theme, width: usize) -> Vec<Line<'static>> {
    let w = width.max(4);
    let inner = w.saturating_sub(2); // "▌ "
    let bar = Style::default().fg(theme.accent).bg(theme.surface);
    let fill = Style::default().bg(theme.surface);
    let txt = Style::default().fg(theme.user).bg(theme.surface);
    let pad_row = || {
        Line::from(vec![
            Span::styled("▌", bar),
            Span::styled(" ".repeat(w - 1), fill),
        ])
    };
    let mut out = vec![pad_row()];
    for raw in text.lines() {
        let line = Line::from(Span::styled(raw.to_string(), txt));
        for wl in wrap_line(&line, inner) {
            let used = wl.width();
            let mut spans = vec![Span::styled("▌ ", bar)];
            spans.extend(wl.spans.into_iter().map(|mut s| {
                s.style = s.style.bg(theme.surface);
                s
            }));
            spans.push(Span::styled(" ".repeat(inner.saturating_sub(used)), fill));
            out.push(Line::from(spans));
        }
    }
    out.push(pad_row());
    out
}

/// An assistant message: plain markdown, indented two spaces, no frame. Cursor when streaming.
fn assistant_block(text: &str, theme: &Theme, streaming: bool, width: usize) -> Vec<Line<'static>> {
    let inner = width.clamp(16, MAX_CARD).saturating_sub(2);
    let md = super::markdown::render_markdown(text, theme);
    let md = if md.is_empty() {
        vec![Line::raw("")]
    } else {
        md
    };
    let mut out: Vec<Line<'static>> = Vec::new();
    let n = md.len();
    for (i, ml) in md.into_iter().enumerate() {
        let wrapped = wrap_line(&ml, inner);
        let wl = wrapped.len();
        for (j, w_line) in wrapped.into_iter().enumerate() {
            let mut spans = vec![Span::raw("  ")];
            spans.extend(w_line.spans);
            if streaming && i + 1 == n && j + 1 == wl {
                spans.push(Span::styled("▌", Style::default().fg(theme.accent)));
            }
            out.push(Line::from(spans));
        }
    }
    out
}

/// Render one transcript entry in the minimal (OpenCode-style) look.
fn render_entry(
    e: &Entry,
    theme: &Theme,
    show_tool_output: bool,
    width: usize,
) -> Vec<Line<'static>> {
    match e {
        Entry::User(t) => user_block(t, theme, width),
        Entry::Assistant(t) => assistant_block(t, theme, false, width),
        Entry::Tool { name, text, saved } => {
            let inner = width.clamp(16, MAX_CARD).saturating_sub(4);
            let mut head = vec![
                Span::styled("  ", Style::default()),
                Span::styled(
                    format!("▣ {name}"),
                    Style::default().fg(theme.tool).add_modifier(Modifier::BOLD),
                ),
            ];
            if *saved > 0 {
                head.push(Span::styled(
                    format!("  · saved ~{saved}"),
                    Style::default().fg(theme.dim),
                ));
            }
            if !show_tool_output {
                head.push(Span::styled(
                    format!("  · {} lines", text.lines().count()),
                    Style::default().fg(theme.border),
                ));
                return vec![Line::from(head)];
            }
            let mut out = vec![Line::from(head)];
            for raw in text.lines().take(12) {
                let line = Line::from(Span::styled(
                    raw.to_string(),
                    Style::default().fg(theme.dim),
                ));
                for wl in wrap_line(&line, inner) {
                    let mut spans = vec![Span::styled("    ", Style::default())];
                    spans.extend(wl.spans);
                    out.push(Line::from(spans));
                }
            }
            let extra = text.lines().count().saturating_sub(12);
            if extra > 0 {
                out.push(Line::from(Span::styled(
                    format!("    … {extra} more lines"),
                    Style::default().fg(theme.border),
                )));
            }
            out
        }
        Entry::System(t) => {
            let line = Line::from(vec![
                Span::styled("  ", Style::default()),
                Span::styled(
                    t.clone(),
                    Style::default()
                        .fg(theme.system)
                        .add_modifier(Modifier::ITALIC),
                ),
            ]);
            wrap_line(&line, width.max(14))
        }
        Entry::Turn { mode, model, secs } => {
            // Completion footer under a reply: ▣ mode · model · Ns
            vec![Line::from(vec![
                Span::styled(
                    format!("  ▣ {mode}"),
                    Style::default()
                        .fg(theme.accent)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    format!(" · {model} · {secs:.1}s"),
                    Style::default().fg(theme.dim),
                ),
            ])]
        }
    }
}

/// Group a palette item belongs to (from its label prefix).
fn palette_group(label: &str) -> &'static str {
    if label.starts_with("model:") {
        "Models"
    } else if label.starts_with("mode:") {
        "Agents"
    } else if label.starts_with("provider:") {
        "Providers"
    } else if label.starts_with("theme:") {
        "Themes"
    } else {
        "Commands"
    }
}

/// The label shown to the user (drops the group prefix).
fn palette_display(label: &str) -> String {
    for p in ["model: ", "mode: ", "provider: ", "theme: "] {
        if let Some(r) = label.strip_prefix(p) {
            return r.to_string();
        }
    }
    label.to_string()
}

/// The keybind shortcut shown on the right of a palette row (empty if none).
fn palette_shortcut(label: &str) -> &'static str {
    match label {
        "/new" => "^X n",
        "/sessions" => "^X l",
        "/model" => "^X m",
        "/compact" => "^X c",
        "/rename" => "^R",
        "/help" => "^X h",
        "/connect" => "^X f",
        _ => "",
    }
}

/// One palette row: bright name + dim hint, right-aligned shortcut; the selected row is a
/// full-width peach highlight bar with a `●` bullet (OpenCode style).
/// Render the shared modal chrome — a centered surface panel with a bold title and `esc` on the
/// right — and return the body rect (below the 2-row header) plus its usable width.
fn modal_shell(f: &mut Frame, popup: Rect, title: &str, theme: &Theme) -> (Rect, usize) {
    f.render_widget(Clear, popup);
    let block = Block::default()
        .style(Style::default().bg(theme.surface))
        .padding(Padding::new(2, 2, 1, 1));
    let inner = block.inner(popup);
    f.render_widget(block, popup);
    let w = inner.width as usize;
    let surf = theme.surface;
    let hpad = w.saturating_sub(title.chars().count() + 3);
    let header = Paragraph::new(vec![
        Line::from(vec![
            Span::styled(
                title.to_string(),
                Style::default()
                    .fg(theme.user)
                    .bg(surf)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(" ".repeat(hpad), Style::default().bg(surf)),
            Span::styled("esc", Style::default().fg(theme.dim).bg(surf)),
        ]),
        Line::from(Span::styled(" ", Style::default().bg(surf))),
    ]);
    f.render_widget(
        header,
        Rect {
            x: inner.x,
            y: inner.y,
            width: inner.width,
            height: 2.min(inner.height),
        },
    );
    let body = Rect {
        x: inner.x,
        y: inner.y.saturating_add(2),
        width: inner.width,
        height: inner.height.saturating_sub(2),
    };
    (body, w)
}

/// One selectable modal row (palette / picker): bright name + dim hint, right-aligned shortcut; the
/// selected row is a full-width peach highlight bar with a `●` bullet (OpenCode style).
fn modal_row(
    w: usize,
    star: &str,
    disp: &str,
    hint: &str,
    sc: &str,
    selected: bool,
    theme: &Theme,
) -> Line<'static> {
    let name = format!("{star}{disp}");
    let sc_w = sc.chars().count();
    if selected {
        const PEACH: Color = Color::Rgb(232, 174, 128);
        const DARK: Color = Color::Rgb(24, 26, 32);
        let hl = Style::default().bg(PEACH).fg(DARK);
        let mut left = format!("● {name}");
        if !hint.is_empty() {
            let avail = w.saturating_sub(left.chars().count() + sc_w + 4);
            let h: String = hint.chars().take(avail).collect();
            left.push_str(&format!("  {h}"));
        }
        let pad = w.saturating_sub(left.chars().count() + sc_w);
        Line::from(vec![
            Span::styled(left, hl.add_modifier(Modifier::BOLD)),
            Span::styled(" ".repeat(pad), hl),
            Span::styled(sc.to_string(), hl),
        ])
    } else {
        let surf = theme.surface;
        let mut spans = vec![Span::styled(
            format!("  {name}"),
            Style::default()
                .fg(theme.user)
                .bg(surf)
                .add_modifier(Modifier::BOLD),
        )];
        let mut used = 2 + name.chars().count();
        if !hint.is_empty() {
            let avail = w.saturating_sub(used + sc_w + 4);
            let h: String = hint.chars().take(avail).collect();
            used += 2 + h.chars().count();
            spans.push(Span::styled(
                format!("  {h}"),
                Style::default().fg(theme.dim).bg(surf),
            ));
        }
        let pad = w.saturating_sub(used + sc_w);
        spans.push(Span::styled(" ".repeat(pad), Style::default().bg(surf)));
        spans.push(Span::styled(
            sc.to_string(),
            Style::default().fg(theme.dim).bg(surf),
        ));
        Line::from(spans)
    }
}

/// The ctrl+p command palette / model picker (OpenCode-style).
fn render_palette(f: &mut Frame, area: Rect, model: &Model, theme: &Theme) {
    let popup = centered_rect(62, 74, area);
    let title = if model.palette_query.starts_with("model") {
        "Select model"
    } else if model.palette_query.starts_with("mode") {
        "Select agent"
    } else {
        "Commands"
    };
    let (body, w) = modal_shell(f, popup, title, theme);
    let surf = theme.surface;
    let dim = Style::default().fg(theme.dim).bg(surf);
    let bright = Style::default().fg(theme.user).bg(surf);

    let mut rows: Vec<Line> = Vec::new();
    // Search line + blank.
    if model.palette_query.is_empty() {
        rows.push(Line::from(Span::styled("Search", dim)));
    } else {
        rows.push(Line::from(vec![
            Span::styled(model.palette_query.clone(), bright),
            Span::styled("▌", Style::default().fg(theme.accent).bg(surf)),
        ]));
    }
    rows.push(Line::from(Span::styled(" ", Style::default().bg(surf))));

    let filtered = model.palette_filtered();
    if filtered.is_empty() {
        rows.push(Line::from(Span::styled("  no matches", dim)));
    }
    let sel = model.palette_sel.min(filtered.len().saturating_sub(1));

    // Build every item row (with group headers), recording the selected item's line index so the
    // window can always keep it on screen (group headers no longer push the selection off-screen).
    let mut items: Vec<Line> = Vec::new();
    let mut sel_line = 0usize;
    let mut prev_group = "";
    for (i, &idx) in filtered.iter().enumerate() {
        let it = &model.palette[idx];
        let g = palette_group(&it.label);
        if g != prev_group {
            items.push(Line::from(Span::styled(
                g.to_string(),
                Style::default()
                    .fg(theme.accent)
                    .bg(surf)
                    .add_modifier(Modifier::BOLD),
            )));
            prev_group = g;
        }
        if i == sel {
            sel_line = items.len();
        }
        let disp = palette_display(&it.label);
        let star = if let Some(name) = it.label.strip_prefix("model: ") {
            if model.favorites.iter().any(|fav| fav == name) {
                "★ "
            } else if model.recents.iter().any(|r| r == name) {
                "● "
            } else {
                ""
            }
        } else {
            ""
        };
        items.push(modal_row(
            w,
            star,
            &disp,
            &it.hint,
            palette_shortcut(&it.label),
            i == sel,
            theme,
        ));
    }

    // Scroll so the selected row is visible.
    let avail = (body.height as usize).saturating_sub(rows.len()).max(1);
    let scroll = (sel_line + 1).saturating_sub(avail);
    rows.extend(items.into_iter().skip(scroll).take(avail));

    f.render_widget(Paragraph::new(rows), body);
}

/// A rectangle centered in `area`, sized as a percentage of it.
fn centered_rect(pct_x: u16, pct_y: u16, area: Rect) -> Rect {
    let vertical = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage((100 - pct_y) / 2),
            Constraint::Percentage(pct_y),
            Constraint::Percentage((100 - pct_y) / 2),
        ])
        .split(area);
    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - pct_x) / 2),
            Constraint::Percentage(pct_x),
            Constraint::Percentage((100 - pct_x) / 2),
        ])
        .split(vertical[1])[1]
}
