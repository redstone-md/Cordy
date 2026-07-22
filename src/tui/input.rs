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
    /// `/goal <text>` — set the autonomous north-star.
    Goal(String),
    /// `/ralph` — run the autonomous ralph-loop toward the current goal.
    Ralph,
    Unknown(String),
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
        "goal" => Command::Goal(arg.to_string()),
        "ralph" => Command::Ralph,
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
            Some(Command::Goal("ship it".into()))
        );
        assert_eq!(parse_command("/wat"), Some(Command::Unknown("wat".into())));
        assert_eq!(parse_command("hello"), None);
    }
}
