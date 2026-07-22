//! Recover tool calls that a model emitted as plain TEXT instead of the structured `tool_calls`
//! API field. Some model families (Gemma, Qwen/Hermes-style, and OpenAI-compatible endpoints whose
//! chat template renders tool calls as literal text) don't populate `tool_calls`; the call arrives
//! inside the assistant's `content` as one of a few conventions:
//!
//! - `<tool_call>{"name":"f","arguments":{...}}</tool_call>` (Gemma-alt / Qwen / Hermes / Mistral),
//!   including the pipe-delimited `<|tool_call> ... <tool_call|>` variant some templates print,
//! - a ```` ```tool_code ```` fenced Python call, e.g. `write(path="a", content="b")`,
//! - the `<|tool_call>call:NAME{key:<|">value<|">,...}<tool_call|>` DSL (string values delimited by
//!   the literal token `<|">`).
//!
//! [`recover_text_tool_calls`] runs only when a turn produced no structured tool call, so it never
//! interferes with providers that do function calling properly.

use serde_json::{Map, Value};

use crate::core::types::{ContentBlock, Message};

/// Convert any text-encoded tool calls in `msg`'s text blocks into [`ContentBlock::ToolUse`]
/// blocks (with synthetic ids) and strip them from the surrounding text. Returns how many were
/// recovered (0 = message left unchanged in spirit).
pub fn recover_text_tool_calls(msg: &mut Message) -> usize {
    let mut recovered = 0;
    let mut blocks: Vec<ContentBlock> = Vec::new();
    for block in std::mem::take(&mut msg.content) {
        match block {
            ContentBlock::Text { text } => {
                let (clean, calls) = extract(&text);
                if !clean.trim().is_empty() {
                    blocks.push(ContentBlock::text(clean.trim().to_string()));
                }
                for (name, input) in calls {
                    blocks.push(ContentBlock::ToolUse {
                        id: format!("call_{recovered}"),
                        name,
                        input,
                    });
                    recovered += 1;
                }
            }
            other => blocks.push(other),
        }
    }
    msg.content = blocks;
    recovered
}

/// Opener/closer pairs for tagged tool-call blocks, tried in order.
const TAGS: &[(&str, &str)] = &[
    ("<|tool_call>", "<tool_call|>"),
    ("<tool_call>", "</tool_call>"),
    ("[TOOL_CALL]", "[/TOOL_CALL]"),
    ("[TOOL_CALLS]", "[/TOOL_CALLS]"),
];

/// Split `text` into the leftover prose (call regions removed) and the parsed calls.
fn extract(text: &str) -> (String, Vec<(String, Value)>) {
    let mut calls = Vec::new();
    let mut clean = text.to_string();

    // Tagged blocks.
    for (open, close) in TAGS {
        while let Some(o) = clean.find(open) {
            let after = o + open.len();
            let Some(rel) = clean[after..].find(close) else {
                break;
            };
            let inner = clean[after..after + rel].to_string();
            let end = after + rel + close.len();
            if let Some(call) = parse_inner(&inner) {
                calls.push(call);
            }
            clean.replace_range(o..end, "");
        }
    }

    // Fenced ```tool_code python-style calls.
    for inner in fenced(&clean.clone(), "tool_code") {
        for c in parse_python_calls(&inner) {
            calls.push(c);
        }
    }
    // Strip the fences from the prose once parsed.
    while let Some((start, end)) = fenced_span(&clean, "tool_code") {
        clean.replace_range(start..end, "");
    }

    (clean, calls)
}

/// Parse the inside of a tool-call tag: JSON object, or the `call:NAME{...}` DSL, or a bare
/// Python-style call.
fn parse_inner(inner: &str) -> Option<(String, Value)> {
    let s = inner.trim();
    if s.starts_with('{') {
        return parse_json_call(s);
    }
    if let Some(rest) = s.strip_prefix("call:") {
        return parse_dsl_call(rest);
    }
    parse_python_calls(s).into_iter().next()
}

/// `{"name":"f","arguments":{...}}` — `arguments`/`parameters` may itself be a JSON string.
fn parse_json_call(s: &str) -> Option<(String, Value)> {
    let v: Value = serde_json::from_str(s).ok()?;
    let name = v
        .get("name")
        .or_else(|| v.get("function"))
        .and_then(Value::as_str)?
        .to_string();
    let args = v
        .get("arguments")
        .or_else(|| v.get("parameters"))
        .or_else(|| v.get("args"))
        .cloned()
        .unwrap_or(Value::Object(Map::new()));
    let args = match args {
        // Some emitters double-encode the arguments as a JSON string.
        Value::String(s) => serde_json::from_str(&s).unwrap_or(Value::String(s)),
        other => other,
    };
    Some((name, args))
}

/// `NAME{key:<|">value<|">,key:<|">value<|">}` — every value is a string literal delimited by the
/// `<|">` token, so commas/braces inside a value (e.g. pasted JSON) are safe.
fn parse_dsl_call(rest: &str) -> Option<(String, Value)> {
    let brace = rest.find('{')?;
    let name = rest[..brace].trim().to_string();
    if name.is_empty() {
        return None;
    }
    let body = &rest[brace + 1..rest.rfind('}').unwrap_or(rest.len())];
    const DELIM: &str = "<|\">";
    let parts: Vec<&str> = body.split(DELIM).collect();
    let mut args = Map::new();
    let mut i = 0;
    while i + 1 < parts.len() {
        let key = parts[i]
            .trim()
            .trim_start_matches(',')
            .trim()
            .trim_end_matches(':')
            .trim();
        if !key.is_empty() {
            // Values are string literals; keep them as strings (a write's `content` is file text).
            args.insert(key.to_string(), Value::String(parts[i + 1].to_string()));
        }
        i += 2;
    }
    Some((name, Value::Object(args)))
}

/// Parse one or more Python-style calls: `func(k="v", n=1)`, optionally wrapped in `print(...)`.
fn parse_python_calls(text: &str) -> Vec<(String, Value)> {
    let mut out = Vec::new();
    for raw in text.lines() {
        let line = raw.trim();
        let line = line.strip_prefix("print(").unwrap_or(line);
        let Some(paren) = line.find('(') else {
            continue;
        };
        let name = line[..paren].trim();
        if name.is_empty() || !name.chars().all(|c| c.is_alphanumeric() || c == '_') {
            continue;
        }
        let Some(close) = line.rfind(')') else {
            continue;
        };
        if close < paren {
            continue;
        }
        let args = parse_kwargs(&line[paren + 1..close]);
        out.push((name.to_string(), Value::Object(args)));
    }
    out
}

/// Parse `k="v", n=1, b=true` into a JSON object, respecting quotes so a comma inside a string
/// doesn't split an argument.
fn parse_kwargs(s: &str) -> Map<String, Value> {
    let mut map = Map::new();
    for part in split_top_level(s) {
        let Some(eq) = part.find('=') else { continue };
        let key = part[..eq].trim().to_string();
        let val = part[eq + 1..].trim();
        if key.is_empty() {
            continue;
        }
        map.insert(key, py_value(val));
    }
    map
}

/// A Python literal → JSON value (string, int, float, bool, else raw string).
fn py_value(v: &str) -> Value {
    let v = v.trim();
    if (v.starts_with('"') && v.ends_with('"') && v.len() >= 2)
        || (v.starts_with('\'') && v.ends_with('\'') && v.len() >= 2)
    {
        return Value::String(v[1..v.len() - 1].to_string());
    }
    match v {
        "True" | "true" => return Value::Bool(true),
        "False" | "false" => return Value::Bool(false),
        _ => {}
    }
    if let Ok(i) = v.parse::<i64>() {
        return Value::Number(i.into());
    }
    if let Ok(f) = v.parse::<f64>()
        && let Some(n) = serde_json::Number::from_f64(f)
    {
        return Value::Number(n);
    }
    Value::String(v.to_string())
}

/// Split on top-level commas only (ignoring commas inside single/double quotes or brackets).
fn split_top_level(s: &str) -> Vec<String> {
    let mut parts = Vec::new();
    let mut cur = String::new();
    let mut quote: Option<char> = None;
    let mut depth = 0i32;
    for c in s.chars() {
        match quote {
            Some(q) => {
                cur.push(c);
                if c == q {
                    quote = None;
                }
            }
            None => match c {
                '"' | '\'' => {
                    quote = Some(c);
                    cur.push(c);
                }
                '[' | '{' | '(' => {
                    depth += 1;
                    cur.push(c);
                }
                ']' | '}' | ')' => {
                    depth -= 1;
                    cur.push(c);
                }
                ',' if depth == 0 => {
                    parts.push(std::mem::take(&mut cur));
                }
                _ => cur.push(c),
            },
        }
    }
    if !cur.trim().is_empty() {
        parts.push(cur);
    }
    parts
}

/// The inner text of every ```` ```<lang> ... ``` ```` fence.
fn fenced(text: &str, lang: &str) -> Vec<String> {
    let mut out = Vec::new();
    let open = format!("```{lang}");
    let mut rest = text;
    while let Some(o) = rest.find(&open) {
        let after = o + open.len();
        if let Some(rel) = rest[after..].find("```") {
            out.push(rest[after..after + rel].trim().to_string());
            rest = &rest[after + rel + 3..];
        } else {
            break;
        }
    }
    out
}

/// The byte span of the first ```` ```<lang> ... ``` ```` fence (for stripping).
fn fenced_span(text: &str, lang: &str) -> Option<(usize, usize)> {
    let open = format!("```{lang}");
    let o = text.find(&open)?;
    let after = o + open.len();
    let rel = text[after..].find("```")?;
    Some((o, after + rel + 3))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assistant(text: &str) -> Message {
        Message {
            role: crate::core::types::Role::Assistant,
            content: vec![ContentBlock::text(text)],
        }
    }

    fn tool_uses(msg: &Message) -> Vec<(String, Value)> {
        msg.content
            .iter()
            .filter_map(|b| match b {
                ContentBlock::ToolUse { name, input, .. } => Some((name.clone(), input.clone())),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn json_tag_format() {
        let mut m = assistant(
            r#"Let me read it. <tool_call>{"name":"read","arguments":{"path":"a.rs"}}</tool_call>"#,
        );
        assert_eq!(recover_text_tool_calls(&mut m), 1);
        let calls = tool_uses(&m);
        assert_eq!(calls[0].0, "read");
        assert_eq!(calls[0].1["path"], "a.rs");
    }

    #[test]
    fn pipe_delimited_dsl_format() {
        // The exact shape the reported Gemma endpoint emits.
        let mut m = assistant(
            "Starting.\n<|tool_call>call:write{content:<|\">{\"a\":1}<|\">,path:<|\">pkg.json<|\">}<tool_call|>",
        );
        assert_eq!(recover_text_tool_calls(&mut m), 1);
        let calls = tool_uses(&m);
        assert_eq!(calls[0].0, "write");
        // The delimited value stays a string — a file's content, verbatim.
        assert_eq!(calls[0].1["content"], "{\"a\":1}");
        assert_eq!(calls[0].1["path"], "pkg.json");
        // Prose is preserved, the call region stripped.
        assert!(
            m.content
                .iter()
                .any(|b| matches!(b, ContentBlock::Text { text } if text.contains("Starting")))
        );
    }

    #[test]
    fn tool_code_python_format() {
        let mut m = assistant("```tool_code\nread(path=\"main.rs\", limit=10)\n```");
        assert_eq!(recover_text_tool_calls(&mut m), 1);
        let calls = tool_uses(&m);
        assert_eq!(calls[0].0, "read");
        assert_eq!(calls[0].1["path"], "main.rs");
        assert_eq!(calls[0].1["limit"], 10);
    }

    #[test]
    fn double_encoded_arguments_string() {
        let mut m = assistant(
            r#"<tool_call>{"name":"grep","arguments":"{\"pattern\":\"fn\"}"}</tool_call>"#,
        );
        assert_eq!(recover_text_tool_calls(&mut m), 1);
        assert_eq!(tool_uses(&m)[0].1["pattern"], "fn");
    }

    #[test]
    fn plain_text_is_untouched() {
        let mut m = assistant("Just a normal reply with no tool call.");
        assert_eq!(recover_text_tool_calls(&mut m), 0);
        assert!(
            matches!(&m.content[0], ContentBlock::Text { text } if text.contains("normal reply"))
        );
    }
}
