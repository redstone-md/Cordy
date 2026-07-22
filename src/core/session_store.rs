//! Session persistence.
//!
//! Each session is a `.cordy/sessions/<id>.jsonl` file: the first line is the [`SessionMeta`]
//! header, and every following line is one canonical [`Message`] appended as it happens. The
//! stored history is always the raw, uncompacted conversation, so `--resume` replays exactly
//! what was said even if the in-context copy was compacted.

use std::io::Write;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::core::types::{ContentBlock, Message};

/// Header describing a stored session.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionMeta {
    pub id: String,
    pub provider: String,
    pub model: String,
    pub cwd: String,
    pub created_at: u64,
    pub title: String,
}

/// A picker-friendly view of a session: its header, last-activity time, and a preview of the most
/// recent user message.
#[derive(Debug, Clone)]
pub struct SessionSummary {
    pub meta: SessionMeta,
    /// Unix seconds of the session file's last modification (i.e. last activity).
    pub updated: u64,
    /// The last user message's text (trimmed to one line), for a "last action" preview.
    pub last_user: String,
    /// Total message count.
    pub messages: usize,
}

/// Append-only JSONL session storage under a directory.
pub struct SessionStore {
    dir: PathBuf,
}

impl SessionStore {
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        SessionStore { dir: dir.into() }
    }

    /// A fresh session id derived from the wall clock (millis, base-36).
    pub fn new_id() -> String {
        let ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0);
        format!("s{ms}")
    }

    fn path(&self, id: &str) -> PathBuf {
        self.dir.join(format!("{id}.jsonl"))
    }

    /// Start a session file with its header.
    pub fn create(&self, meta: &SessionMeta) -> anyhow::Result<()> {
        std::fs::create_dir_all(&self.dir)?;
        let line = serde_json::to_string(meta)?;
        std::fs::write(self.path(&meta.id), format!("{line}\n"))?;
        Ok(())
    }

    /// Append one message to a session.
    pub fn append(&self, id: &str, msg: &Message) -> anyhow::Result<()> {
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(self.path(id))?;
        writeln!(f, "{}", serde_json::to_string(msg)?)?;
        Ok(())
    }

    /// Load a session's header and messages.
    pub fn load(&self, id: &str) -> anyhow::Result<(SessionMeta, Vec<Message>)> {
        let text = std::fs::read_to_string(self.path(id))?;
        let mut lines = text.lines();
        let meta: SessionMeta = match lines.next() {
            Some(l) => serde_json::from_str(l)?,
            None => anyhow::bail!("empty session file"),
        };
        let mut messages = Vec::new();
        for l in lines {
            if l.trim().is_empty() {
                continue;
            }
            messages.push(serde_json::from_str::<Message>(l)?);
        }
        Ok((meta, messages))
    }

    /// Headers of all stored sessions, newest first.
    pub fn list(&self) -> Vec<SessionMeta> {
        let mut metas: Vec<SessionMeta> = Vec::new();
        let Ok(entries) = std::fs::read_dir(&self.dir) else {
            return metas;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
                continue;
            }
            if let Ok(text) = std::fs::read_to_string(&path)
                && let Some(first) = text.lines().next()
                && let Ok(meta) = serde_json::from_str::<SessionMeta>(first)
            {
                metas.push(meta);
            }
        }
        metas.sort_by_key(|m| std::cmp::Reverse(m.created_at));
        metas
    }

    /// The id of the most recent session, if any.
    pub fn latest(&self) -> Option<String> {
        self.list().into_iter().next().map(|m| m.id)
    }

    /// Rich session summaries for the picker: header + last-activity time + last-user preview,
    /// sorted by most-recently-active first.
    pub fn list_summaries(&self) -> Vec<SessionSummary> {
        let mut out: Vec<SessionSummary> = Vec::new();
        let Ok(entries) = std::fs::read_dir(&self.dir) else {
            return out;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
                continue;
            }
            let Ok(text) = std::fs::read_to_string(&path) else {
                continue;
            };
            let mut lines = text.lines();
            let Some(meta) = lines
                .next()
                .and_then(|l| serde_json::from_str::<SessionMeta>(l).ok())
            else {
                continue;
            };
            let mut last_user = String::new();
            let mut messages = 0usize;
            for l in lines {
                if l.trim().is_empty() {
                    continue;
                }
                if let Ok(m) = serde_json::from_str::<Message>(l) {
                    messages += 1;
                    if matches!(m.role, crate::core::types::Role::User) {
                        let text: String = m
                            .content
                            .iter()
                            .filter_map(|b| match b {
                                ContentBlock::Text { text } => Some(text.as_str()),
                                _ => None,
                            })
                            .collect::<Vec<_>>()
                            .join(" ");
                        if !text.trim().is_empty() {
                            last_user = text.split_whitespace().collect::<Vec<_>>().join(" ");
                        }
                    }
                }
            }
            let updated = entry
                .metadata()
                .and_then(|m| m.modified())
                .ok()
                .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                .map(|d| d.as_secs())
                .unwrap_or(meta.created_at);
            out.push(SessionSummary {
                meta,
                updated,
                last_user,
                messages,
            });
        }
        out.sort_by_key(|s| std::cmp::Reverse(s.updated));
        out
    }

    /// Set a session's title (rewrites the header, preserving all messages).
    pub fn rename(&self, id: &str, title: &str) -> anyhow::Result<()> {
        let (mut meta, msgs) = self.load(id)?;
        meta.title = title.to_string();
        let mut out = serde_json::to_string(&meta)?;
        out.push('\n');
        for m in &msgs {
            out.push_str(&serde_json::to_string(m)?);
            out.push('\n');
        }
        std::fs::write(self.path(id), out)?;
        Ok(())
    }

    /// Delete a session file.
    pub fn delete(&self, id: &str) -> anyhow::Result<()> {
        std::fs::remove_file(self.path(id))?;
        Ok(())
    }

    /// Copy a session into a fresh one (branch/fork). Returns the new id.
    pub fn fork(&self, id: &str) -> anyhow::Result<String> {
        let (mut meta, msgs) = self.load(id)?;
        let new_id = Self::new_id();
        meta.id = new_id.clone();
        meta.created_at = now_unix();
        meta.title = if meta.title.is_empty() {
            "fork".into()
        } else {
            format!("{} (fork)", meta.title)
        };
        self.create(&meta)?;
        for m in &msgs {
            self.append(&new_id, m)?;
        }
        Ok(new_id)
    }
}

/// Current unix seconds (for `created_at`).
pub fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::types::Message;

    fn meta(id: &str) -> SessionMeta {
        SessionMeta {
            id: id.into(),
            provider: "openai-chat".into(),
            model: "gpt-4o".into(),
            cwd: "/proj".into(),
            created_at: 1000,
            title: "test".into(),
        }
    }

    #[test]
    fn create_append_load_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let store = SessionStore::new(dir.path());
        store.create(&meta("s1")).unwrap();
        store.append("s1", &Message::user("hello")).unwrap();
        store
            .append(
                "s1",
                &Message::assistant(vec![crate::core::types::ContentBlock::text("hi")]),
            )
            .unwrap();

        let (m, msgs) = store.load("s1").unwrap();
        assert_eq!(m.model, "gpt-4o");
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[0], Message::user("hello"));
    }

    #[test]
    fn rename_fork_delete() {
        let dir = tempfile::tempdir().unwrap();
        let store = SessionStore::new(dir.path());
        store.create(&meta("s1")).unwrap();
        store.append("s1", &Message::user("hello")).unwrap();

        store.rename("s1", "renamed").unwrap();
        let (m, msgs) = store.load("s1").unwrap();
        assert_eq!(m.title, "renamed");
        assert_eq!(msgs.len(), 1); // messages preserved

        let fork_id = store.fork("s1").unwrap();
        assert_ne!(fork_id, "s1");
        let (fm, fmsgs) = store.load(&fork_id).unwrap();
        assert_eq!(fm.title, "renamed (fork)");
        assert_eq!(fmsgs.len(), 1);

        store.delete("s1").unwrap();
        assert!(store.load("s1").is_err());
        assert!(store.load(&fork_id).is_ok()); // fork survives
    }

    #[test]
    fn list_sorts_newest_first() {
        let dir = tempfile::tempdir().unwrap();
        let store = SessionStore::new(dir.path());
        let mut older = meta("old");
        older.created_at = 100;
        let mut newer = meta("new");
        newer.created_at = 200;
        store.create(&older).unwrap();
        store.create(&newer).unwrap();

        let list = store.list();
        assert_eq!(list.len(), 2);
        assert_eq!(list[0].id, "new");
        assert_eq!(store.latest().as_deref(), Some("new"));
    }
}
