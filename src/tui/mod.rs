//! Terminal UI — Elm/MVU.
//!
//! [`Model`] holds all state; [`update`] is a pure `(Model, Msg) -> Effect` transition tested in
//! isolation; [`view`] renders the model; [`run`] is the runtime that owns the terminal, spawns
//! the agent driver, and pumps input/agent/permission events through `update`. The input line is
//! a cursor-aware editor with OpenCode-style keybinds; Tab cycles the active mode.

mod input;
mod markdown;
mod runtime;
mod theme;

pub use runtime::run;

use crate::core::agent::AgentEvent;

/// One rendered line of conversation history.
#[derive(Debug, Clone, PartialEq)]
pub enum Entry {
    User(String),
    Assistant(String),
    Tool {
        name: String,
        text: String,
        saved: u64,
    },
    System(String),
    /// Completion footer under an assistant reply: mode chip + model + elapsed seconds.
    Turn {
        mode: String,
        model: String,
        secs: f64,
    },
}

/// What selecting a command-palette row does.
#[derive(Clone)]
pub enum PaletteAction {
    /// Run a slash command (or prefill it if it needs an argument).
    Command(String),
    /// Hot-swap to this model on the current provider.
    SwitchModel(String),
    /// Switch the active mode/agent by index.
    SwitchMode(usize),
    /// Connect to a saved provider by its config name (resolves base URL + key).
    SwitchProvider(String),
    /// Apply the theme at this index in `THEME_NAMES`.
    SwitchTheme(usize),
}

/// A row in the ctrl+p command palette.
#[derive(Clone)]
pub struct PaletteItem {
    pub label: String,
    pub hint: String,
    pub action: PaletteAction,
}

/// Which step of the `/connect` provider wizard is showing.
#[derive(Debug, Clone, PartialEq)]
pub enum ConnectStep {
    /// Choosing a provider preset.
    Pick,
    /// Typing a custom base URL (for the "Custom…" preset).
    Url,
    /// Typing a display name (required — the id is derived from it).
    Name,
    /// Typing the API key.
    Key,
}

/// Transient state for the in-TUI `/connect` provider wizard (OpenCode-style).
#[derive(Debug, Clone)]
pub struct Connect {
    pub step: ConnectStep,
    /// Highlighted preset while picking.
    pub sel: usize,
    /// Chosen preset index (valid once past [`ConnectStep::Pick`]).
    pub preset: usize,
    /// Resolved base URL (from the preset or typed for a custom endpoint).
    pub base: String,
    /// The chosen display name (id is derived from it).
    pub name: String,
    /// Current text field (URL → name → key).
    pub input: String,
}

impl Default for Connect {
    fn default() -> Self {
        Connect {
            step: ConnectStep::Pick,
            sel: 0,
            preset: 0,
            base: String::new(),
            name: String::new(),
            input: String::new(),
        }
    }
}

/// All UI state.
#[derive(Default)]
pub struct Model {
    pub transcript: Vec<Entry>,
    /// Current input line.
    pub input: String,
    /// Cursor byte offset within `input`.
    pub cursor: usize,
    /// Selection anchor (byte offset); `Some` while a shift-selection is active. The selected
    /// range is `[min(anchor,cursor), max(anchor,cursor))`.
    pub anchor: Option<usize>,
    /// Assistant text streaming in for the current (not-yet-finalized) message.
    pub streaming: String,
    pub status: String,
    pub busy: bool,
    /// When set, a permission modal is showing this summary and awaiting y/n.
    pub pending: Option<String>,
    pub should_quit: bool,
    /// Input mode line (provider · model), set once at startup.
    pub subtitle: String,
    /// Bottom-bar left segment (cwd), set once at startup.
    pub footer: String,
    /// Whether to draw the animated mascot.
    pub show_mascot: bool,
    /// Available modes/agents cycled with Tab.
    pub modes: Vec<String>,
    pub mode_idx: usize,
    /// Ticks remaining for the mode-switch flash animation.
    pub mode_flash: u8,
    /// Leader key (ctrl+x) is pending its second keystroke.
    pub leader: bool,
    /// Command palette (ctrl+p) open state, filter query, selected row, and item source.
    pub palette_open: bool,
    pub palette_query: String,
    pub palette_sel: usize,
    pub palette: Vec<PaletteItem>,
    /// Session picker overlay: open state, rows `(id, label)`, and selection.
    pub sessions_open: bool,
    pub sessions: Vec<(String, String)>,
    pub sessions_sel: usize,
    /// The `/connect` provider wizard, when open.
    pub connect: Option<Connect>,
    /// Provider manager overlay (`/providers`): open state, rows `(id, kind, base)`, selection.
    pub providers_open: bool,
    pub providers: Vec<(String, String, String)>,
    pub providers_sel: usize,
    /// Status view overlay (<leader>s).
    pub status_open: bool,
    /// Theme picker overlay (<leader>t): open state, selection, and the active theme name.
    pub theme_open: bool,
    pub theme_sel: usize,
    pub theme_name: String,
    /// Generic info modal (skills / mcp listings): title + body lines. `None` when closed.
    pub info: Option<(String, Vec<String>)>,
    /// Live counts for the status bar: running background jobs and active sub-agents.
    pub bg_count: usize,
    pub subagent_count: usize,
    /// Recently-used models (newest first) for F2 cycling, and favorited models (ctrl+f).
    pub recents: Vec<String>,
    pub favorites: Vec<String>,
    /// View toggles (OpenCode `none`-key features, exposed as commands).
    pub show_thinking: bool,
    pub show_tool_output: bool,
    pub animations: bool,
    /// Live reasoning/thinking buffer for the current turn (shown when `show_thinking`).
    pub thinking: String,
    /// Stashed prompt drafts (`/stash` push · `/unstash` pop).
    pub stash: Vec<String>,
    /// Loaded skill names, and MCP servers as `(name, status)` for the panel + `/mcp`.
    pub skill_names: Vec<String>,
    pub mcp_names: Vec<(String, String)>,
    /// Inline input autocomplete candidates (slash commands / `@` file paths).
    pub suggestions: Vec<String>,
    /// Highlighted row within `suggestions` (navigated with ↑/↓ while the popup is open).
    pub suggestion_sel: usize,
    /// Messages typed while the agent was busy, sent in order once the turn completes.
    pub queue: Vec<String>,
    /// Draft stashed when browsing history with ↑, restored when stepping back past the newest.
    pub history_draft: Option<(String, usize)>,
    /// Large pastes collapsed to `[pasted N chars]` placeholders; expanded on submit. Indexed by
    /// the id embedded in the placeholder token.
    pub pastes: std::collections::HashMap<u32, String>,
    /// Monotonic id source for `pastes`.
    pub paste_seq: u32,
    /// Text columns available for one input row (excludes the prompt gutter); set during render
    /// so cursor navigation can follow wrapped visual lines.
    pub input_width: u16,
    /// Submitted-prompt history (newest last) and the browse cursor while pressing ↑/↓.
    pub history: Vec<String>,
    pub history_idx: Option<usize>,
    /// Input-editor undo/redo snapshots `(text, cursor)`.
    pub undo_stack: Vec<(String, usize)>,
    pub redo_stack: Vec<(String, usize)>,
    /// Removed message groups, for messages_redo (<leader>r).
    pub msg_redo: Vec<Vec<Entry>>,
    /// Transcript scroll offset from the bottom (0 = follow latest).
    pub scroll: u16,
    /// Animation frame counter (advanced on each tick).
    pub tick: u64,
    // Live cost/token HUD accumulators.
    pub total_in: u64,
    pub total_out: u64,
    pub total_saved: u64,
    /// Prompt tokens sent on the most recent turn — i.e. how much context is currently in use
    /// (each turn resends the full history), for the context gauge.
    pub last_in: u64,
    /// Active model's context window (from config/models.dev), for the context gauge.
    pub context_window: Option<u64>,
    /// Per-million-token pricing for the active model, if known.
    pub price_in: Option<f64>,
    pub price_out: Option<f64>,
    /// Active model + provider, for the status-line placeholders.
    pub model_name: String,
    pub provider_kind: String,
    /// User-customizable status-line template (placeholders like `{model}`, `{ctx}`, `{cost}`).
    /// `None` uses the built-in default layout.
    pub statusline: Option<String>,
    /// A newer published Cordy version, if the startup update check found one.
    pub latest_version: Option<String>,
}

impl Model {
    /// The HUD line: cumulative tokens, optimizer savings, and estimated cost.
    pub fn hud_line(&self) -> String {
        let mut s = format!("{} in / {} out", self.total_in, self.total_out);
        if self.total_saved > 0 {
            s.push_str(&format!(" · saved ~{}", self.total_saved));
        }
        if let (Some(pi), Some(po)) = (self.price_in, self.price_out) {
            let cost = (self.total_in as f64) / 1e6 * pi + (self.total_out as f64) / 1e6 * po;
            s.push_str(&format!(" · ${cost:.4}"));
        }
        s
    }

    /// Estimated cost so far as `$x.xxxx`, or empty when pricing is unknown.
    pub fn cost_str(&self) -> String {
        match (self.price_in, self.price_out) {
            (Some(pi), Some(po)) => {
                let cost = (self.total_in as f64) / 1e6 * pi + (self.total_out as f64) / 1e6 * po;
                format!("${cost:.4}")
            }
            _ => String::new(),
        }
    }

    /// Context-window gauge: `ctx 12k/128k 9%` when the window is known, `ctx 12k` when only the
    /// last prompt size is known, empty before anything has been sent.
    pub fn ctx_line(&self) -> String {
        if self.last_in == 0 {
            return String::new();
        }
        let used = crate::core::models_dev::fmt_context(self.last_in);
        match self.context_window {
            Some(w) if w > 0 => {
                let pct = ((self.last_in as f64 / w as f64) * 100.0).round() as u64;
                format!(
                    "ctx {used}/{} {pct}%",
                    crate::core::models_dev::fmt_context(w)
                )
            }
            _ => format!("ctx {used}"),
        }
    }

    /// The active mode/agent name.
    pub fn mode(&self) -> &str {
        self.modes
            .get(self.mode_idx)
            .map(String::as_str)
            .unwrap_or("build")
    }

    /// Indices of palette items matching the current query (case-insensitive substring).
    pub fn palette_filtered(&self) -> Vec<usize> {
        let q = self.palette_query.to_lowercase();
        self.palette
            .iter()
            .enumerate()
            .filter(|(_, it)| {
                q.is_empty()
                    || it.label.to_lowercase().contains(&q)
                    || it.hint.to_lowercase().contains(&q)
            })
            .map(|(i, _)| i)
            .collect()
    }
}

/// Messages that drive the model.
pub enum Msg {
    // Input editing (OpenCode keybinds map onto these).
    Insert(char),
    Backspace,
    Delete,
    Left,
    Right,
    /// Buffer start/end (Home/End keys).
    Home,
    End,
    /// Current-line start/end (ctrl+a / ctrl+e).
    LineHome,
    LineEnd,
    /// Insert a newline (multiline input).
    Newline,
    /// Move the cursor to the same column on the previous/next logical line.
    CursorUp,
    CursorDown,
    /// Word-wise cursor motion.
    WordForward,
    WordBackward,
    KillWordBack,
    /// Delete the word after the cursor.
    KillWordForward,
    KillToStart,
    KillToEnd,
    /// Delete the current logical line.
    KillLine,
    ClearInput,
    /// Input-editor undo / redo.
    Undo,
    Redo,
    /// Browse submitted-prompt history.
    HistoryPrev,
    HistoryNext,
    /// Cycle the active mode (+1 forward / -1 back).
    CycleMode(i8),
    Submit,
    Quit,
    Agent(AgentEvent),
    /// A permission request arrived with this human-readable summary.
    Permission(String),
    /// The pending permission was answered; clear the modal.
    PermissionResolved,
    Tick,
}

/// Side effect the runtime must perform after an update.
#[derive(Debug, PartialEq)]
pub enum Effect {
    None,
    Submit(String),
    Quit,
}

/// Pure state transition. All side effects are returned as [`Effect`] for the runtime to run.
pub fn update(model: &mut Model, msg: Msg) -> Effect {
    // While a permission modal is open, ignore text editing.
    let editing_blocked = model.pending.is_some();
    match msg {
        Msg::Insert(c) if !editing_blocked => {
            push_undo(model);
            delete_selection(model); // typing replaces an active selection
            model.input.insert(model.cursor, c);
            model.cursor += c.len_utf8();
            Effect::None
        }
        Msg::Insert(_) => Effect::None,
        Msg::Newline => {
            if !editing_blocked {
                push_undo(model);
                delete_selection(model);
                model.input.insert(model.cursor, '\n');
                model.cursor += 1;
            }
            Effect::None
        }
        Msg::Backspace => {
            if !editing_blocked {
                if selection_nonempty(model) {
                    push_undo(model);
                    delete_selection(model);
                } else if model.cursor > 0 {
                    push_undo(model);
                    let p = prev_boundary(&model.input, model.cursor);
                    model.input.replace_range(p..model.cursor, "");
                    model.cursor = p;
                }
            }
            Effect::None
        }
        Msg::Delete => {
            if !editing_blocked {
                if selection_nonempty(model) {
                    push_undo(model);
                    delete_selection(model);
                } else if model.cursor < model.input.len() {
                    push_undo(model);
                    let n = next_boundary(&model.input, model.cursor);
                    model.input.replace_range(model.cursor..n, "");
                }
            }
            Effect::None
        }
        Msg::Left => {
            model.cursor = prev_boundary(&model.input, model.cursor);
            Effect::None
        }
        Msg::Right => {
            model.cursor = next_boundary(&model.input, model.cursor);
            Effect::None
        }
        Msg::Home => {
            model.cursor = 0;
            Effect::None
        }
        Msg::End => {
            model.cursor = model.input.len();
            Effect::None
        }
        Msg::LineHome => {
            model.cursor = line_home(&model.input, model.cursor);
            Effect::None
        }
        Msg::LineEnd => {
            model.cursor = line_end(&model.input, model.cursor);
            Effect::None
        }
        Msg::CursorUp => {
            if let Some(c) = move_vertical(&model.input, model.cursor, -1, model.input_width as usize)
            {
                model.cursor = c;
            }
            Effect::None
        }
        Msg::CursorDown => {
            if let Some(c) = move_vertical(&model.input, model.cursor, 1, model.input_width as usize) {
                model.cursor = c;
            }
            Effect::None
        }
        Msg::WordForward => {
            model.cursor = word_forward(&model.input, model.cursor);
            Effect::None
        }
        Msg::WordBackward => {
            model.cursor = word_start(&model.input, model.cursor);
            Effect::None
        }
        Msg::KillWordBack => {
            if !editing_blocked && model.cursor > 0 {
                push_undo(model);
                let w = word_start(&model.input, model.cursor);
                model.input.replace_range(w..model.cursor, "");
                model.cursor = w;
            }
            Effect::None
        }
        Msg::KillWordForward => {
            if !editing_blocked && model.cursor < model.input.len() {
                push_undo(model);
                let w = word_forward(&model.input, model.cursor);
                model.input.replace_range(model.cursor..w, "");
            }
            Effect::None
        }
        Msg::KillToStart => {
            if !editing_blocked {
                push_undo(model);
                let h = line_home(&model.input, model.cursor);
                model.input.replace_range(h..model.cursor, "");
                model.cursor = h;
            }
            Effect::None
        }
        Msg::KillToEnd => {
            if !editing_blocked {
                push_undo(model);
                let e = line_end(&model.input, model.cursor);
                model.input.replace_range(model.cursor..e, "");
            }
            Effect::None
        }
        Msg::KillLine => {
            if !editing_blocked {
                push_undo(model);
                let (a, b) = current_line_range(&model.input, model.cursor);
                // Remove the line's text plus its trailing newline (or the leading one at EOF).
                let mut end = b;
                if model.input[end..].starts_with('\n') {
                    end += 1;
                } else if a > 0 {
                    // last line: also drop the preceding newline
                    return kill_last_line(model, a, b);
                }
                model.input.replace_range(a..end, "");
                model.cursor = a.min(model.input.len());
            }
            Effect::None
        }
        Msg::ClearInput => {
            if !model.input.is_empty() {
                push_undo(model);
            }
            model.input.clear();
            model.cursor = 0;
            model.anchor = None;
            model.history_idx = None;
            Effect::None
        }
        Msg::Undo => {
            if let Some((text, cur)) = model.undo_stack.pop() {
                model.redo_stack.push((model.input.clone(), model.cursor));
                model.input = text;
                model.cursor = cur.min(model.input.len());
            }
            Effect::None
        }
        Msg::Redo => {
            if let Some((text, cur)) = model.redo_stack.pop() {
                model.undo_stack.push((model.input.clone(), model.cursor));
                model.input = text;
                model.cursor = cur.min(model.input.len());
            }
            Effect::None
        }
        Msg::HistoryPrev => {
            history_prev(model);
            Effect::None
        }
        Msg::HistoryNext => {
            history_next(model);
            Effect::None
        }
        Msg::CycleMode(dir) => {
            if !model.modes.is_empty() {
                let n = model.modes.len() as isize;
                let i = (model.mode_idx as isize + dir as isize).rem_euclid(n);
                model.mode_idx = i as usize;
                model.mode_flash = 6;
            }
            Effect::None
        }
        Msg::Submit => {
            if model.input.trim().is_empty() {
                return Effect::None;
            }
            let text = std::mem::take(&mut model.input);
            model.cursor = 0;
            model.anchor = None;
            model.history_draft = None;
            // Busy: hold the message in the queue; it is dispatched when the turn completes. The
            // queue is shown above the prompt (see the input view) rather than the transcript, so
            // the message appears in-line only once it actually starts processing.
            if model.busy {
                model.queue.push(text);
                model.history_idx = None;
                return Effect::None;
            }
            if model.history.last().map(String::as_str) != Some(text.as_str()) {
                model.history.push(text.clone());
            }
            model.history_idx = None;
            model.undo_stack.clear();
            model.redo_stack.clear();
            model.transcript.push(Entry::User(text.clone()));
            model.busy = true;
            model.status = "thinking…".into();
            model.thinking.clear();
            model.scroll = 0; // snap back to the latest on a new turn
            Effect::Submit(text)
        }
        Msg::Quit => {
            model.should_quit = true;
            Effect::Quit
        }
        Msg::Agent(ev) => {
            apply_agent(model, ev);
            Effect::None
        }
        Msg::Permission(summary) => {
            model.pending = Some(summary);
            Effect::None
        }
        Msg::PermissionResolved => {
            model.pending = None;
            Effect::None
        }
        Msg::Tick => {
            if model.animations {
                model.tick = model.tick.wrapping_add(1);
            }
            model.mode_flash = model.mode_flash.saturating_sub(1);
            Effect::None
        }
    }
}

/// Whether an active selection spans at least one character.
fn selection_nonempty(model: &Model) -> bool {
    model.anchor.is_some_and(|a| a != model.cursor)
}

/// Delete the active selection (if any), moving the cursor to its start. Clears the anchor.
/// Returns whether anything was removed.
fn delete_selection(model: &mut Model) -> bool {
    if let Some(a) = model.anchor.take() {
        let (s, e) = (a.min(model.cursor), a.max(model.cursor));
        if s != e {
            model.input.replace_range(s..e, "");
            model.cursor = s;
            return true;
        }
    }
    false
}

/// Snapshot the input for undo before a mutation, dropping the oldest when the stack is large.
/// A new edit invalidates the redo stack.
fn push_undo(model: &mut Model) {
    model.undo_stack.push((model.input.clone(), model.cursor));
    if model.undo_stack.len() > 200 {
        model.undo_stack.remove(0);
    }
    model.redo_stack.clear();
    model.history_idx = None;
}

/// Replace the input with the previous history entry. The live draft is stashed on the first
/// step into history so stepping back past the newest entry restores it verbatim.
fn history_prev(model: &mut Model) {
    if model.history.is_empty() {
        return;
    }
    let idx = match model.history_idx {
        None => {
            // Entering history from a live draft — remember it (text + cursor) to restore later.
            model.history_draft = Some((model.input.clone(), model.cursor));
            model.history.len() - 1
        }
        Some(0) => 0,
        Some(i) => i - 1,
    };
    model.history_idx = Some(idx);
    model.input = model.history[idx].clone();
    model.cursor = model.input.len();
}

/// Move forward through history; stepping past the newest entry restores the stashed draft.
fn history_next(model: &mut Model) {
    match model.history_idx {
        Some(i) if i + 1 < model.history.len() => {
            model.history_idx = Some(i + 1);
            model.input = model.history[i + 1].clone();
            model.cursor = model.input.len();
        }
        Some(_) => {
            model.history_idx = None;
            let (text, cur) = model.history_draft.take().unwrap_or_default();
            model.cursor = cur.min(text.len());
            model.input = text;
        }
        None => {}
    }
}

/// Byte ranges (start, end-exclusive-of-newline) of each logical line in `s`.
fn line_ranges(s: &str) -> Vec<(usize, usize)> {
    let mut v = Vec::new();
    let mut start = 0;
    for (i, c) in s.char_indices() {
        if c == '\n' {
            v.push((start, i));
            start = i + c.len_utf8();
        }
    }
    v.push((start, s.len()));
    v
}

/// The byte range of the logical line containing `cursor`.
fn current_line_range(s: &str, cursor: usize) -> (usize, usize) {
    let ranges = line_ranges(s);
    for &(a, b) in &ranges {
        if cursor <= b {
            return (a, b);
        }
    }
    (0, s.len())
}

/// Start of the current logical line (for Home).
fn line_home(s: &str, cursor: usize) -> usize {
    current_line_range(s, cursor).0
}

/// End of the current logical line (for End).
fn line_end(s: &str, cursor: usize) -> usize {
    current_line_range(s, cursor).1
}

/// Visual rows of `s` hard-wrapped at `width` characters, as byte ranges `(start, end)`. Each
/// logical line yields at least one row; an over-long line splits every `width` chars. `width == 0`
/// disables wrapping (one row per logical line).
fn visual_rows(s: &str, width: usize) -> Vec<(usize, usize)> {
    let width = if width == 0 { usize::MAX } else { width };
    let mut rows = Vec::new();
    for (a, b) in line_ranges(s) {
        let mut chunk_start = a;
        let mut count = 0usize;
        let mut pos = a;
        for ch in s[a..b].chars() {
            if count == width {
                rows.push((chunk_start, pos));
                chunk_start = pos;
                count = 0;
            }
            pos += ch.len_utf8();
            count += 1;
        }
        rows.push((chunk_start, pos)); // trailing (possibly empty) remainder
    }
    rows
}

/// The (visual row, column-in-chars) of `cursor` within the wrapped rows of `s`.
fn visual_row_col(rows: &[(usize, usize)], s: &str, cursor: usize) -> (usize, usize) {
    for (r, &(a, b)) in rows.iter().enumerate() {
        if cursor <= b {
            return (r, s[a..cursor].chars().count());
        }
    }
    (rows.len().saturating_sub(1), 0)
}

/// Move the cursor vertically by `dir` (−1 up / +1 down) across wrapped visual rows, preserving the
/// column. Returns `None` at the top/bottom visual row (caller may fall back to history).
fn move_vertical(s: &str, cursor: usize, dir: i32, width: usize) -> Option<usize> {
    let rows = visual_rows(s, width);
    let (row, col) = visual_row_col(&rows, s, cursor);
    let target = row as i32 + dir;
    if target < 0 || target as usize >= rows.len() {
        return None;
    }
    let (a, b) = rows[target as usize];
    let mut pos = a;
    for (n, ch) in s[a..b].chars().enumerate() {
        if n == col {
            return Some(pos);
        }
        pos += ch.len_utf8();
    }
    Some(b) // column past the row end → clamp to end
}

/// Next word boundary after byte index `i` (skip whitespace, then the word).
fn word_forward(s: &str, i: usize) -> usize {
    let mut j = i;
    while j < s.len() {
        let c = s[j..].chars().next().unwrap();
        if c.is_whitespace() {
            j += c.len_utf8();
        } else {
            break;
        }
    }
    while j < s.len() {
        let c = s[j..].chars().next().unwrap();
        if c.is_whitespace() {
            break;
        }
        j += c.len_utf8();
    }
    j
}

/// Delete the last logical line together with the newline that precedes it.
fn kill_last_line(model: &mut Model, a: usize, b: usize) -> Effect {
    let start = prev_boundary(&model.input, a); // the '\n' before this line
    model.input.replace_range(start..b, "");
    model.cursor = start.min(model.input.len());
    Effect::None
}

/// Spinner frame for the current tick (used while busy).
pub fn spinner_frame(tick: u64) -> char {
    const FRAMES: [char; 10] = ['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];
    FRAMES[(tick as usize) % FRAMES.len()]
}

/// Previous char boundary before byte index `i`.
fn prev_boundary(s: &str, i: usize) -> usize {
    s[..i]
        .char_indices()
        .next_back()
        .map(|(idx, _)| idx)
        .unwrap_or(0)
}

/// Next char boundary after byte index `i`.
fn next_boundary(s: &str, i: usize) -> usize {
    s[i..]
        .char_indices()
        .nth(1)
        .map(|(idx, _)| i + idx)
        .unwrap_or_else(|| s.len())
}

/// Start of the word before byte index `i` (skips trailing spaces then the word).
fn word_start(s: &str, i: usize) -> usize {
    let mut j = i;
    while j > 0 {
        let p = prev_boundary(s, j);
        if s[p..j].chars().next().is_some_and(char::is_whitespace) {
            j = p;
        } else {
            break;
        }
    }
    while j > 0 {
        let p = prev_boundary(s, j);
        if s[p..j].chars().next().is_some_and(|c| !c.is_whitespace()) {
            j = p;
        } else {
            break;
        }
    }
    j
}

/// Fold an agent event into the transcript / streaming buffer.
fn apply_agent(model: &mut Model, ev: AgentEvent) {
    match ev {
        AgentEvent::TextDelta(s) => model.streaming.push_str(&s),
        AgentEvent::ThinkingDelta(s) => model.thinking.push_str(&s),
        AgentEvent::ToolStarted { name, .. } => {
            flush_streaming(model);
            model.status = format!("running {name}…");
        }
        AgentEvent::ToolFinished { name, output, .. } => {
            model.total_saved += output.saved;
            model.transcript.push(Entry::Tool {
                name,
                text: output.text,
                saved: output.saved,
            });
        }
        AgentEvent::TurnComplete { usage } => {
            flush_streaming(model);
            model.thinking.clear();
            model.busy = false;
            model.scroll = 0; // follow to the latest so the reply is visible
            model.total_in += usage.input_tokens;
            model.total_out += usage.output_tokens;
            if usage.input_tokens > 0 {
                model.last_in = usage.input_tokens; // current context occupancy
            }
            model.status = "ready".into();
        }
        AgentEvent::SubAgent { agent, note } => {
            model
                .transcript
                .push(Entry::System(format!("  ↳ [{agent}] {note}")));
        }
        AgentEvent::Error(e) => {
            flush_streaming(model);
            model.transcript.push(Entry::System(format!("error: {e}")));
            model.busy = false;
            model.status = "error".into();
        }
    }
}

fn flush_streaming(model: &mut Model) {
    if !model.streaming.is_empty() {
        let text = std::mem::take(&mut model.streaming);
        model.transcript.push(Entry::Assistant(text));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::types::{ToolOutput, Usage};

    fn typed(s: &str) -> Model {
        let mut m = Model::default();
        for c in s.chars() {
            update(&mut m, Msg::Insert(c));
        }
        m
    }

    #[test]
    fn typing_places_cursor_at_end() {
        let m = typed("hello");
        assert_eq!(m.input, "hello");
        assert_eq!(m.cursor, 5);
    }

    #[test]
    fn cursor_move_and_insert_in_middle() {
        let mut m = typed("helo");
        update(&mut m, Msg::Left); // after 'hel', before 'o'
        update(&mut m, Msg::Insert('l'));
        assert_eq!(m.input, "hello");
    }

    #[test]
    fn backspace_and_delete_at_cursor() {
        let mut m = typed("abc");
        update(&mut m, Msg::Home);
        update(&mut m, Msg::Delete); // remove 'a'
        assert_eq!(m.input, "bc");
        update(&mut m, Msg::End);
        update(&mut m, Msg::Backspace); // remove 'c'
        assert_eq!(m.input, "b");
    }

    #[test]
    fn kill_word_and_line() {
        let mut m = typed("foo bar baz");
        update(&mut m, Msg::KillWordBack); // removes "baz"
        assert_eq!(m.input, "foo bar ");
        update(&mut m, Msg::KillToStart);
        assert_eq!(m.input, "");
    }

    #[test]
    fn submit_takes_input_and_resets_cursor() {
        let mut m = typed("hi");
        let eff = update(&mut m, Msg::Submit);
        assert_eq!(eff, Effect::Submit("hi".into()));
        assert_eq!(m.input, "");
        assert_eq!(m.cursor, 0);
        assert!(m.busy);
    }

    #[test]
    fn tab_cycles_mode_and_flashes() {
        let mut m = Model {
            modes: vec!["build".into(), "plan".into()],
            ..Default::default()
        };
        assert_eq!(m.mode(), "build");
        update(&mut m, Msg::CycleMode(1));
        assert_eq!(m.mode(), "plan");
        assert!(m.mode_flash > 0);
        update(&mut m, Msg::CycleMode(1));
        assert_eq!(m.mode(), "build"); // wraps
        update(&mut m, Msg::CycleMode(-1));
        assert_eq!(m.mode(), "plan"); // reverse wraps
    }

    #[test]
    fn tick_decays_flash() {
        let mut m = Model {
            mode_flash: 2,
            ..Default::default()
        };
        update(&mut m, Msg::Tick);
        assert_eq!(m.mode_flash, 1);
    }

    #[test]
    fn streaming_then_complete_flushes_assistant() {
        let mut m = Model {
            busy: true,
            ..Default::default()
        };
        update(&mut m, Msg::Agent(AgentEvent::TextDelta("hel".into())));
        update(&mut m, Msg::Agent(AgentEvent::TextDelta("lo".into())));
        assert_eq!(m.streaming, "hello");
        update(
            &mut m,
            Msg::Agent(AgentEvent::TurnComplete {
                usage: Usage::default(),
            }),
        );
        assert_eq!(m.transcript, vec![Entry::Assistant("hello".into())]);
        assert!(!m.busy);
    }

    #[test]
    fn hud_accumulates_tokens_savings_and_cost() {
        let mut m = Model {
            price_in: Some(1.0),
            price_out: Some(2.0),
            ..Default::default()
        };
        update(
            &mut m,
            Msg::Agent(AgentEvent::ToolFinished {
                id: "1".into(),
                name: "bash".into(),
                output: ToolOutput {
                    text: "x".into(),
                    is_error: false,
                    saved: 30,
                },
            }),
        );
        update(
            &mut m,
            Msg::Agent(AgentEvent::TurnComplete {
                usage: Usage {
                    input_tokens: 1_000_000,
                    output_tokens: 500_000,
                    ..Default::default()
                },
            }),
        );
        let hud = m.hud_line();
        assert!(hud.contains("1000000 in / 500000 out"), "{hud}");
        assert!(hud.contains("saved ~30"), "{hud}");
        assert!(hud.contains("$2.0000"), "{hud}");
    }

    #[test]
    fn ctx_gauge_reports_usage_percent() {
        let mut m = Model {
            context_window: Some(128_000),
            ..Default::default()
        };
        assert_eq!(m.ctx_line(), ""); // nothing sent yet
        update(
            &mut m,
            Msg::Agent(AgentEvent::TurnComplete {
                usage: Usage {
                    input_tokens: 12_800,
                    output_tokens: 50,
                    ..Default::default()
                },
            }),
        );
        assert_eq!(m.last_in, 12_800);
        let line = m.ctx_line();
        assert!(line.contains("128k"), "{line}");
        assert!(line.contains("10%"), "{line}");
    }

    #[test]
    fn submit_snaps_scroll_to_latest() {
        let mut m = typed("hi");
        m.scroll = 20;
        update(&mut m, Msg::Submit);
        assert_eq!(m.scroll, 0);
    }

    #[test]
    fn multiline_newline_and_vertical_move() {
        let mut m = typed("abc");
        update(&mut m, Msg::Newline);
        for c in "de".chars() {
            update(&mut m, Msg::Insert(c));
        }
        assert_eq!(m.input, "abc\nde");
        // cursor after "de" (row 1, col 2). Up → row 0 col 2 (after "ab").
        update(&mut m, Msg::CursorUp);
        assert_eq!(m.cursor, 2);
        update(&mut m, Msg::CursorDown);
        assert_eq!(m.cursor, 6); // clamped to end of "de"
        // no line above the first → cursor unchanged, caller falls back to history
        update(&mut m, Msg::Home);
        assert!(move_vertical(&m.input, m.cursor, -1, 0).is_none());
    }

    #[test]
    fn vertical_move_follows_wrapped_visual_rows() {
        // A single logical line longer than the width wraps into visual rows; Up must move to the
        // row above (not fall through to history) until the top visual row.
        let m = typed("abcdefghij"); // 10 chars, width 4 → rows "abcd" "efgh" "ij"
        // cursor at 9 (before 'j'), visual row 2 col 1. Up → row 1 col 1 = byte 5.
        assert_eq!(move_vertical(&m.input, 9, -1, 4), Some(5));
        // row 1 → row 0 col 1 = byte 1.
        assert_eq!(move_vertical(&m.input, 5, -1, 4), Some(1));
        // top visual row → None (caller falls back to history).
        assert_eq!(move_vertical(&m.input, 1, -1, 4), None);
        // Down from the top row returns to row 1.
        assert_eq!(move_vertical(&m.input, 1, 1, 4), Some(5));
    }

    #[test]
    fn history_down_restores_draft() {
        let mut m = typed("my draft");
        m.history.push("older msg".into());
        // Up enters history, stashing the draft.
        update(&mut m, Msg::HistoryPrev);
        assert_eq!(m.input, "older msg");
        // Down past the newest restores the draft verbatim (with cursor).
        update(&mut m, Msg::HistoryNext);
        assert_eq!(m.input, "my draft");
        assert_eq!(m.cursor, "my draft".len());
    }

    #[test]
    fn busy_submit_queues_message() {
        let mut m = typed("second");
        m.busy = true;
        let eff = update(&mut m, Msg::Submit);
        assert_eq!(eff, Effect::None);
        assert_eq!(m.queue, vec!["second".to_string()]);
        assert!(m.input.is_empty());
    }

    #[test]
    fn line_home_end_vs_buffer() {
        let mut m = typed("ab\ncd");
        // cursor at end (row1 col2). LineHome → start of "cd".
        update(&mut m, Msg::LineHome);
        assert_eq!(m.cursor, 3);
        update(&mut m, Msg::Home); // buffer start
        assert_eq!(m.cursor, 0);
        update(&mut m, Msg::LineEnd);
        assert_eq!(m.cursor, 2); // end of "ab"
        update(&mut m, Msg::End);
        assert_eq!(m.cursor, 5);
    }

    #[test]
    fn word_motions_and_forward_delete() {
        let mut m = typed("foo bar baz");
        update(&mut m, Msg::Home);
        update(&mut m, Msg::WordForward); // to end of "foo"
        assert_eq!(m.cursor, 3);
        update(&mut m, Msg::WordForward); // to end of "bar"
        assert_eq!(m.cursor, 7);
        update(&mut m, Msg::Home);
        update(&mut m, Msg::KillWordForward); // deletes "foo"
        assert_eq!(m.input, " bar baz");
    }

    #[test]
    fn kill_line_removes_current_line() {
        let mut m = typed("one\ntwo\nthree");
        // cursor on "three" (last line)
        update(&mut m, Msg::KillLine);
        assert_eq!(m.input, "one\ntwo");
        // now on "two" (last line) again
        update(&mut m, Msg::KillLine);
        assert_eq!(m.input, "one");
    }

    #[test]
    fn history_browse_up_down() {
        let mut m = Model::default();
        for prompt in ["first", "second"] {
            for c in prompt.chars() {
                update(&mut m, Msg::Insert(c));
            }
            update(&mut m, Msg::Submit);
            m.busy = false; // simulate turn completion
        }
        assert_eq!(m.history, vec!["first".to_string(), "second".to_string()]);
        update(&mut m, Msg::HistoryPrev);
        assert_eq!(m.input, "second");
        update(&mut m, Msg::HistoryPrev);
        assert_eq!(m.input, "first");
        update(&mut m, Msg::HistoryNext);
        assert_eq!(m.input, "second");
        update(&mut m, Msg::HistoryNext); // past newest → empty draft
        assert_eq!(m.input, "");
    }

    #[test]
    fn selection_delete_and_replace() {
        let mut m = typed("hello");
        m.anchor = Some(0); // select the whole "hello"
        update(&mut m, Msg::Backspace);
        assert_eq!(m.input, "");
        assert!(m.anchor.is_none());

        let mut m = typed("hello");
        m.cursor = 0;
        m.anchor = Some(5); // select all, cursor at start
        update(&mut m, Msg::Insert('X')); // typing replaces the selection
        assert_eq!(m.input, "X");
    }

    #[test]
    fn undo_redo_restores_edits() {
        let mut m = typed("hello");
        update(&mut m, Msg::KillWordBack); // ""
        assert_eq!(m.input, "");
        update(&mut m, Msg::Undo);
        assert_eq!(m.input, "hello");
        update(&mut m, Msg::Redo);
        assert_eq!(m.input, "");
    }

    #[test]
    fn permission_modal_blocks_input_editing() {
        let mut m = Model::default();
        update(&mut m, Msg::Permission("edit foo.rs".into()));
        assert_eq!(m.pending.as_deref(), Some("edit foo.rs"));
        update(&mut m, Msg::Insert('x')); // ignored while modal open
        assert_eq!(m.input, "");
        update(&mut m, Msg::PermissionResolved);
        assert!(m.pending.is_none());
    }
}
