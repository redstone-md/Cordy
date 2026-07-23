//! Parsing user input into canonical content blocks.
//!
//! Supports inline `@image <path>` attachments: the referenced file is read and base64-encoded
//! into a [`ContentBlock::Image`], and the surrounding words become the text block. This is the
//! vision entry point; `@path` file mentions and slash commands are layered on in later steps.

use std::path::Path;

use base64::Engine;
use base64::engine::general_purpose::STANDARD;

use crate::core::types::ContentBlock;

/// A slash command typed into the prompt.
#[derive(Debug, Clone, PartialEq)]
pub enum Command {
    Help,
    Clear,
    Quit,
    Compact,
    /// `/model [name]` — switch model (None just lists / shows current).
    Model(Option<String>),
    /// `/goal [...]` — inspect or drive the session goal.
    Goal(GoalCmd),
    Unknown(String),
}

/// What `/goal` was asked to do.
#[derive(Debug, Clone, PartialEq)]
pub enum GoalCmd {
    /// Bare `/goal` — show the current goal and its usage.
    Show,
    /// `/goal <objective> [--budget N] [--cost N] [--turns N]` — set the objective and start work.
    Set {
        objective: String,
        limits: GoalLimitArgs,
    },
    /// `/goal edit` — reopen the objective in the composer.
    Edit,
    Pause,
    Resume,
    Clear,
}

/// Caps parsed off a `/goal` line. `None` means "leave as-is".
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct GoalLimitArgs {
    pub token_budget: Option<i64>,
    pub cost_cap_usd: Option<f64>,
    pub max_iterations: Option<u32>,
}

impl GoalLimitArgs {
    pub fn is_empty(&self) -> bool {
        self.token_budget.is_none() && self.cost_cap_usd.is_none() && self.max_iterations.is_none()
    }
}

/// Split `--budget/--cost/--turns` flags out of a `/goal` argument, returning the objective text
/// and the caps. Unparsable values are left in the objective rather than silently dropped.
fn parse_goal_args(arg: &str) -> (String, GoalLimitArgs) {
    let mut limits = GoalLimitArgs::default();
    let mut words: Vec<&str> = Vec::new();
    let mut it = arg.split_whitespace().peekable();
    while let Some(word) = it.next() {
        let flag = match word {
            "--budget" | "--tokens" => 0,
            "--cost" => 1,
            "--turns" | "--iterations" => 2,
            _ => {
                words.push(word);
                continue;
            }
        };
        let Some(value) = it.peek().copied() else {
            words.push(word);
            continue;
        };
        let cleaned = value.trim_start_matches('$').replace(['_', ','], "");
        let parsed = match flag {
            0 => parse_token_count(&cleaned).map(|v| limits.token_budget = Some(v)),
            1 => cleaned
                .parse::<f64>()
                .ok()
                .map(|v| limits.cost_cap_usd = Some(v)),
            _ => cleaned
                .parse::<u32>()
                .ok()
                .map(|v| limits.max_iterations = Some(v)),
        };
        if parsed.is_some() {
            it.next();
        } else {
            words.push(word);
        }
    }
    (words.join(" "), limits)
}

/// Accept `50000`, `50k` and `1.5m` for token counts.
fn parse_token_count(raw: &str) -> Option<i64> {
    let lowered = raw.to_ascii_lowercase();
    let (digits, scale) = match lowered.strip_suffix('k') {
        Some(rest) => (rest, 1_000.0),
        None => match lowered.strip_suffix('m') {
            Some(rest) => (rest, 1_000_000.0),
            None => (lowered.as_str(), 1.0),
        },
    };
    let value: f64 = digits.parse().ok()?;
    let scaled = value * scale;
    (scaled.is_finite() && scaled >= 1.0).then(|| scaled.round() as i64)
}

/// Parse a leading-`/` line into a [`Command`]. Returns `None` for ordinary messages.
pub fn parse_command(raw: &str) -> Option<Command> {
    let rest = raw.trim().strip_prefix('/')?;
    let mut it = rest.splitn(2, char::is_whitespace);
    let name = it.next().unwrap_or("");
    let arg = it.next().map(str::trim).unwrap_or("");
    Some(match name {
        "help" => Command::Help,
        "clear" => Command::Clear,
        "quit" | "exit" => Command::Quit,
        "compact" => Command::Compact,
        "model" => Command::Model((!arg.is_empty()).then(|| arg.to_string())),
        "goal" => Command::Goal(match arg {
            "" => GoalCmd::Show,
            "edit" => GoalCmd::Edit,
            "pause" | "stop" => GoalCmd::Pause,
            "resume" | "continue" | "start" => GoalCmd::Resume,
            "clear" | "reset" | "none" => GoalCmd::Clear,
            _ => {
                let (objective, limits) = parse_goal_args(arg);
                if objective.trim().is_empty() {
                    // Flags with no objective just retune the caps on the existing goal.
                    GoalCmd::Set {
                        objective: String::new(),
                        limits,
                    }
                } else {
                    GoalCmd::Set { objective, limits }
                }
            }
        }),
        // `/ralph` predates `/goal`; keep it working as "get back to the goal".
        "ralph" => Command::Goal(GoalCmd::Resume),
        other => Command::Unknown(other.to_string()),
    })
}

/// Turn a raw input line into the content blocks for a user message. `@image <path>` tokens
/// become image blocks; everything else is joined into a single text block.
pub fn parse_user_input(raw: &str, cwd: &Path) -> Result<Vec<ContentBlock>, String> {
    let mut text_words: Vec<&str> = Vec::new();
    let mut images: Vec<ContentBlock> = Vec::new();

    let mut mentions: Vec<ContentBlock> = Vec::new();

    let mut tokens = raw.split_whitespace().peekable();
    while let Some(tok) = tokens.next() {
        if tok == "@image" {
            let path = tokens
                .next()
                .ok_or_else(|| "@image needs a file path".to_string())?;
            images.push(load_image(path, cwd)?);
        } else if let Some(path) = tok.strip_prefix('@').filter(|p| !p.is_empty()) {
            // `@path` file mention: inject the file's contents as context.
            mentions.push(load_file_mention(path, cwd)?);
        } else {
            text_words.push(tok);
        }
    }

    let mut blocks: Vec<ContentBlock> = Vec::new();
    let text = text_words.join(" ");
    if !text.is_empty() {
        blocks.push(ContentBlock::text(text));
    }
    blocks.extend(mentions);
    blocks.extend(images);
    if blocks.is_empty() {
        blocks.push(ContentBlock::text(raw));
    }
    Ok(blocks)
}

fn load_image(path: &str, cwd: &Path) -> Result<ContentBlock, String> {
    let full = if Path::new(path).is_absolute() {
        Path::new(path).to_path_buf()
    } else {
        cwd.join(path)
    };
    let bytes = std::fs::read(&full).map_err(|e| format!("{}: {e}", full.display()))?;
    let media_type = media_type_of(&full).to_string();
    Ok(ContentBlock::Image {
        media_type,
        data: STANDARD.encode(bytes),
    })
}

fn load_file_mention(path: &str, cwd: &Path) -> Result<ContentBlock, String> {
    let full = if Path::new(path).is_absolute() {
        Path::new(path).to_path_buf()
    } else {
        cwd.join(path)
    };
    let content = std::fs::read_to_string(&full).map_err(|e| format!("{}: {e}", full.display()))?;
    Ok(ContentBlock::text(format!(
        "Contents of {}:\n{content}",
        full.display()
    )))
}

fn media_type_of(path: &Path) -> &'static str {
    match path
        .extension()
        .and_then(|e| e.to_str())
        .map(str::to_ascii_lowercase)
        .as_deref()
    {
        Some("jpg") | Some("jpeg") => "image/jpeg",
        Some("gif") => "image/gif",
        Some("webp") => "image/webp",
        _ => "image/png",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_text_is_one_block() {
        let blocks = parse_user_input("fix the bug in main", Path::new(".")).unwrap();
        assert_eq!(blocks, vec![ContentBlock::text("fix the bug in main")]);
    }

    #[test]
    fn image_token_becomes_image_block() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("shot.png"), b"\x89PNGfake").unwrap();
        let blocks = parse_user_input("look at @image shot.png please", dir.path()).unwrap();
        assert_eq!(blocks.len(), 2);
        assert_eq!(blocks[0], ContentBlock::text("look at please"));
        match &blocks[1] {
            ContentBlock::Image { media_type, data } => {
                assert_eq!(media_type, "image/png");
                assert!(!data.is_empty());
            }
            other => panic!("expected image, got {other:?}"),
        }
    }

    #[test]
    fn file_mention_injects_contents() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("notes.txt"), "hello notes").unwrap();
        let blocks = parse_user_input("summarize @notes.txt", dir.path()).unwrap();
        assert_eq!(blocks.len(), 2);
        assert_eq!(blocks[0], ContentBlock::text("summarize"));
        match &blocks[1] {
            ContentBlock::Text { text } => {
                assert!(text.contains("Contents of"));
                assert!(text.contains("hello notes"));
            }
            other => panic!("expected text mention, got {other:?}"),
        }
    }

    #[test]
    fn missing_image_is_an_error() {
        let err = parse_user_input("@image nope.png", Path::new(".")).unwrap_err();
        assert!(err.contains("nope.png"));
    }

    #[test]
    fn dangling_image_token_errors() {
        let err = parse_user_input("here @image", Path::new(".")).unwrap_err();
        assert!(err.contains("needs a file path"));
    }

    #[test]
    fn parses_slash_commands() {
        assert_eq!(parse_command("/help"), Some(Command::Help));
        assert_eq!(
            parse_command("/model gpt-4o"),
            Some(Command::Model(Some("gpt-4o".into())))
        );
        assert_eq!(parse_command("/model"), Some(Command::Model(None)));
        assert_eq!(
            parse_command("/goal ship it"),
            Some(Command::Goal(GoalCmd::Set {
                objective: "ship it".into(),
                limits: GoalLimitArgs::default(),
            }))
        );
        assert_eq!(parse_command("/wat"), Some(Command::Unknown("wat".into())));
        assert_eq!(parse_command("hello"), None);
    }

    #[test]
    fn parses_goal_subcommands() {
        assert_eq!(parse_command("/goal"), Some(Command::Goal(GoalCmd::Show)));
        assert_eq!(
            parse_command("/goal pause"),
            Some(Command::Goal(GoalCmd::Pause))
        );
        assert_eq!(
            parse_command("/goal clear"),
            Some(Command::Goal(GoalCmd::Clear))
        );
        // `/ralph` is the old name for "keep going".
        assert_eq!(
            parse_command("/ralph"),
            Some(Command::Goal(GoalCmd::Resume))
        );
    }

    #[test]
    fn parses_goal_budget_flags() {
        let Some(Command::Goal(GoalCmd::Set { objective, limits })) =
            parse_command("/goal fix the flaky test --budget 50k --cost 2.5 --turns 8")
        else {
            panic!("expected a goal set command");
        };
        assert_eq!(objective, "fix the flaky test");
        assert_eq!(limits.token_budget, Some(50_000));
        assert_eq!(limits.cost_cap_usd, Some(2.5));
        assert_eq!(limits.max_iterations, Some(8));
    }

    #[test]
    fn a_flag_without_a_usable_value_stays_in_the_objective() {
        let Some(Command::Goal(GoalCmd::Set { objective, limits })) =
            parse_command("/goal make --budget bigger")
        else {
            panic!("expected a goal set command");
        };
        assert_eq!(objective, "make --budget bigger");
        assert!(limits.is_empty());
    }

    #[test]
    fn token_counts_accept_k_and_m_suffixes() {
        assert_eq!(parse_token_count("50000"), Some(50_000));
        assert_eq!(parse_token_count("50k"), Some(50_000));
        assert_eq!(parse_token_count("1.5M"), Some(1_500_000));
        assert_eq!(parse_token_count("nope"), None);
    }
}
