//! Minimal Markdown → ratatui renderer for assistant messages.
//!
//! Handles the common cases seen in model output: headings, bullet lists, fenced code blocks,
//! and inline **bold**, *italic*/_italic_, and `code`. Anything else renders as plain text.

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};

use super::theme::Theme;

/// Muted tone for `inline code` and code blocks — subtle, no background box.
const CODE_FG: Color = Color::Rgb(188, 172, 142);

/// Render markdown `text` into styled lines.
pub fn render_markdown(text: &str, theme: &Theme) -> Vec<Line<'static>> {
    let mut out: Vec<Line<'static>> = Vec::new();
    let mut in_code = false;

    for raw in text.lines() {
        let trimmed = raw.trim_start();

        // Fenced code block toggle.
        if trimmed.starts_with("```") {
            let lang = trimmed.trim_start_matches('`');
            let label = if !in_code && !lang.is_empty() {
                format!("╶─ {lang} ")
            } else {
                "╶──────".into()
            };
            in_code = !in_code;
            out.push(Line::from(Span::styled(
                label,
                Style::default().fg(theme.border),
            )));
            continue;
        }
        if in_code {
            out.push(Line::from(vec![
                Span::styled("▏ ", Style::default().fg(theme.border)),
                Span::styled(raw.to_string(), Style::default().fg(CODE_FG)),
            ]));
            continue;
        }

        // Headings.
        if let Some(h) = heading(trimmed) {
            out.push(Line::from(Span::styled(
                h.to_string(),
                Style::default()
                    .fg(theme.accent)
                    .add_modifier(Modifier::BOLD),
            )));
            continue;
        }

        // Bullet lists.
        let (mut spans, content) = if let Some(c) = trimmed
            .strip_prefix("- ")
            .or_else(|| trimmed.strip_prefix("* "))
        {
            (
                vec![Span::styled("• ", Style::default().fg(theme.accent))],
                c,
            )
        } else {
            (Vec::new(), raw)
        };
        spans.extend(inline_spans(content));
        out.push(Line::from(spans));
    }
    out
}

/// Strip a leading `#`/`##`/`###` heading marker, returning the text.
fn heading(s: &str) -> Option<&str> {
    for marker in ["### ", "## ", "# "] {
        if let Some(rest) = s.strip_prefix(marker) {
            return Some(rest);
        }
    }
    None
}

/// Parse inline markdown (`**bold**`, `*italic*`/`_italic_`, `` `code` ``) into styled spans.
fn inline_spans(s: &str) -> Vec<Span<'static>> {
    let chars: Vec<char> = s.chars().collect();
    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut buf = String::new();
    let mut i = 0;

    let flush = |buf: &mut String, spans: &mut Vec<Span<'static>>| {
        if !buf.is_empty() {
            spans.push(Span::raw(std::mem::take(buf)));
        }
    };

    while i < chars.len() {
        // Inline code: `...` — a soft muted tone, no loud background box.
        if chars[i] == '`'
            && let Some(end) = find(&chars, i + 1, |c| c == '`')
        {
            flush(&mut buf, &mut spans);
            let code: String = chars[i + 1..end].iter().collect();
            spans.push(Span::styled(code, Style::default().fg(CODE_FG)));
            i = end + 1;
            continue;
        }
        // Bold: **...**
        if chars[i] == '*'
            && i + 1 < chars.len()
            && chars[i + 1] == '*'
            && let Some(end) = find_pair(&chars, i + 2)
        {
            flush(&mut buf, &mut spans);
            let inner: String = chars[i + 2..end].iter().collect();
            spans.push(Span::styled(
                inner,
                Style::default().add_modifier(Modifier::BOLD),
            ));
            i = end + 2;
            continue;
        }
        // Italic: *...* or _..._
        if (chars[i] == '*' || chars[i] == '_')
            && let Some(end) = find(&chars, i + 1, |c| c == chars[i])
            && end > i + 1
        {
            flush(&mut buf, &mut spans);
            let inner: String = chars[i + 1..end].iter().collect();
            spans.push(Span::styled(
                inner,
                Style::default().add_modifier(Modifier::ITALIC),
            ));
            i = end + 1;
            continue;
        }
        buf.push(chars[i]);
        i += 1;
    }
    flush(&mut buf, &mut spans);
    if spans.is_empty() {
        spans.push(Span::raw(String::new()));
    }
    spans
}

/// Index of the next char at/after `from` matching `pred`.
fn find(chars: &[char], from: usize, pred: impl Fn(char) -> bool) -> Option<usize> {
    (from..chars.len()).find(|&j| pred(chars[j]))
}

/// Index of the next `**` at/after `from` (returns the index of the first `*`).
fn find_pair(chars: &[char], from: usize) -> Option<usize> {
    (from..chars.len().saturating_sub(1)).find(|&j| chars[j] == '*' && chars[j + 1] == '*')
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plain(line: &Line) -> String {
        line.spans.iter().map(|s| s.content.as_ref()).collect()
    }

    #[test]
    fn renders_bold_italic_code() {
        let t = Theme::dark();
        let lines = render_markdown("a **b** c *d* `e`", &t);
        assert_eq!(lines.len(), 1);
        assert_eq!(plain(&lines[0]), "a b c d e");
        // bold span present
        assert!(
            lines[0]
                .spans
                .iter()
                .any(|s| s.style.add_modifier.contains(Modifier::BOLD))
        );
        assert!(
            lines[0]
                .spans
                .iter()
                .any(|s| s.style.add_modifier.contains(Modifier::ITALIC))
        );
    }

    #[test]
    fn renders_heading_and_bullets() {
        let t = Theme::dark();
        let lines = render_markdown("# Title\n- one\n- two", &t);
        assert_eq!(plain(&lines[0]), "Title");
        assert!(
            lines[0].spans[0]
                .style
                .add_modifier
                .contains(Modifier::BOLD)
        );
        assert!(plain(&lines[1]).starts_with("• one"));
    }

    #[test]
    fn renders_code_block() {
        let t = Theme::dark();
        let lines = render_markdown("```rust\nfn main() {}\n```", &t);
        // fence, code line, fence
        assert!(lines.iter().any(|l| plain(l).contains("fn main() {}")));
    }
}
