//! The `apply_patch` engine: turn a patch into a set of file writes.
//!
//! Work happens in two steps. [`plan`] parses the patch, reads every file it touches, locates each
//! chunk (tolerantly — see [`seek`]) and produces the exact new contents; [`Plan::apply`] then
//! writes them. Splitting it this way means a patch is either fully valid before anything is
//! touched, or it fails with nothing half-written — and the preview shown to the user for approval
//! is the same content that will land.

pub mod parser;
pub mod seek;

use std::path::{Path, PathBuf};

use similar::TextDiff;

pub use parser::{Hunk, ParseError, UpdateFileChunk, parse_patch};

/// Why a patch could not be turned into file writes.
#[derive(Debug, Clone, PartialEq)]
pub enum PatchError {
    Parse(ParseError),
    /// The patch is fine but doesn't fit the files on disk.
    Mismatch(String),
    Io(String),
    /// A path escaped the working directory.
    Path(String),
}

impl std::fmt::Display for PatchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PatchError::Parse(e) => write!(f, "{e}"),
            PatchError::Mismatch(m) | PatchError::Io(m) | PatchError::Path(m) => write!(f, "{m}"),
        }
    }
}

impl From<ParseError> for PatchError {
    fn from(e: ParseError) -> Self {
        PatchError::Parse(e)
    }
}

/// One resolved file operation, with its final contents already computed.
#[derive(Debug, Clone, PartialEq)]
pub enum FileChange {
    Add {
        path: PathBuf,
        contents: String,
    },
    Delete {
        path: PathBuf,
        original: String,
    },
    Update {
        path: PathBuf,
        /// Set when the file is also renamed; `path` is where the content is read from.
        move_to: Option<PathBuf>,
        original: String,
        contents: String,
    },
}

impl FileChange {
    /// The path the change writes to.
    pub fn target(&self) -> &Path {
        match self {
            FileChange::Add { path, .. } | FileChange::Delete { path, .. } => path,
            FileChange::Update {
                move_to: Some(dest),
                ..
            } => dest,
            FileChange::Update { path, .. } => path,
        }
    }

    /// Every path the change reads or writes — what to snapshot before applying.
    pub fn touched(&self) -> Vec<PathBuf> {
        match self {
            FileChange::Add { path, .. } | FileChange::Delete { path, .. } => vec![path.clone()],
            FileChange::Update { path, move_to, .. } => match move_to {
                Some(dest) => vec![path.clone(), dest.clone()],
                None => vec![path.clone()],
            },
        }
    }

    fn label(&self) -> String {
        match self {
            FileChange::Add { path, .. } => format!("A {}", path.display()),
            FileChange::Delete { path, .. } => format!("D {}", path.display()),
            FileChange::Update {
                path,
                move_to: Some(dest),
                ..
            } => format!("M {} → {}", path.display(), dest.display()),
            FileChange::Update { path, .. } => format!("M {}", path.display()),
        }
    }

    /// Lines added and removed, for the one-line result summary.
    fn line_delta(&self) -> (usize, usize) {
        let (before, after) = match self {
            FileChange::Add { contents, .. } => ("", contents.as_str()),
            FileChange::Delete { original, .. } => (original.as_str(), ""),
            FileChange::Update {
                original, contents, ..
            } => (original.as_str(), contents.as_str()),
        };
        let diff = TextDiff::from_lines(before, after);
        let mut added = 0;
        let mut removed = 0;
        for change in diff.iter_all_changes() {
            match change.tag() {
                similar::ChangeTag::Insert => added += 1,
                similar::ChangeTag::Delete => removed += 1,
                similar::ChangeTag::Equal => {}
            }
        }
        (added, removed)
    }
}

/// A validated patch: every change resolved against the current files, nothing written yet.
#[derive(Debug, Clone, PartialEq)]
pub struct Plan {
    pub changes: Vec<FileChange>,
}

impl Plan {
    pub fn is_empty(&self) -> bool {
        self.changes.is_empty()
    }

    /// A unified diff of the whole patch, for the approval prompt.
    pub fn diff(&self) -> String {
        let mut out = String::new();
        for change in &self.changes {
            out.push_str(&change.label());
            out.push('\n');
            let (before, after) = match change {
                FileChange::Add { contents, .. } => ("", contents.as_str()),
                FileChange::Delete { original, .. } => (original.as_str(), ""),
                FileChange::Update {
                    original, contents, ..
                } => (original.as_str(), contents.as_str()),
            };
            let diff = TextDiff::from_lines(before, after);
            out.push_str(
                &diff
                    .unified_diff()
                    .context_radius(3)
                    .header("before", "after")
                    .to_string(),
            );
            if !out.ends_with('\n') {
                out.push('\n');
            }
        }
        out
    }

    /// Write every change. Callers snapshot [`FileChange::touched`] first so this stays undoable.
    pub fn apply(&self) -> Result<String, PatchError> {
        let mut lines = Vec::new();
        for change in &self.changes {
            match change {
                FileChange::Add { path, contents } => {
                    if let Some(parent) = path.parent() {
                        std::fs::create_dir_all(parent).map_err(|e| io_err(path, e))?;
                    }
                    std::fs::write(path, contents).map_err(|e| io_err(path, e))?;
                }
                FileChange::Delete { path, .. } => {
                    std::fs::remove_file(path).map_err(|e| io_err(path, e))?;
                }
                FileChange::Update {
                    path,
                    move_to,
                    contents,
                    ..
                } => {
                    let dest = move_to.as_deref().unwrap_or(path);
                    if let Some(parent) = dest.parent() {
                        std::fs::create_dir_all(parent).map_err(|e| io_err(dest, e))?;
                    }
                    std::fs::write(dest, contents).map_err(|e| io_err(dest, e))?;
                    if move_to.is_some() {
                        std::fs::remove_file(path).map_err(|e| io_err(path, e))?;
                    }
                }
            }
            let (added, removed) = change.line_delta();
            lines.push(format!("{} (+{added} -{removed})", change.label()));
        }
        Ok(lines.join("\n"))
    }
}

fn io_err(path: &Path, e: std::io::Error) -> PatchError {
    PatchError::Io(format!("{}: {e}", path.display()))
}

/// Parse `patch` and resolve it against the files under `cwd`.
///
/// Paths are resolved relative to `cwd` and must stay inside it: a patch is a bulk edit, and one
/// stray `../` would put it outside everything the permission prompt described.
pub fn plan(patch: &str, cwd: &Path) -> Result<Plan, PatchError> {
    let hunks = parse_patch(patch)?;
    let mut changes = Vec::with_capacity(hunks.len());
    for hunk in hunks {
        let change = match hunk {
            Hunk::AddFile { path, contents } => {
                let path = resolve(cwd, &path)?;
                if path.exists() {
                    return Err(PatchError::Mismatch(format!(
                        "{} already exists — use '*** Update File:' to change it",
                        path.display()
                    )));
                }
                FileChange::Add { path, contents }
            }
            Hunk::DeleteFile { path } => {
                let path = resolve(cwd, &path)?;
                let original = read(&path)?;
                FileChange::Delete { path, original }
            }
            Hunk::UpdateFile {
                path,
                move_path,
                chunks,
            } => {
                let path = resolve(cwd, &path)?;
                let move_to = move_path.map(|p| resolve(cwd, &p)).transpose()?;
                let original = read(&path)?;
                let contents = apply_chunks(&original, &chunks, &path)?;
                FileChange::Update {
                    path,
                    move_to,
                    original,
                    contents,
                }
            }
        };
        changes.push(change);
    }
    Ok(Plan { changes })
}

fn read(path: &Path) -> Result<String, PatchError> {
    std::fs::read_to_string(path).map_err(|e| {
        PatchError::Io(format!(
            "failed to read {} to patch it: {e}",
            path.display()
        ))
    })
}

/// Resolve a patch path against `cwd`, refusing anything that leaves it.
fn resolve(cwd: &Path, path: &Path) -> Result<PathBuf, PatchError> {
    if path.is_absolute() {
        return Err(PatchError::Path(format!(
            "{}: patch paths must be relative to the working directory",
            path.display()
        )));
    }
    let mut resolved = cwd.to_path_buf();
    for part in path.components() {
        match part {
            std::path::Component::Normal(p) => resolved.push(p),
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                if !resolved.pop() || !resolved.starts_with(cwd) {
                    return Err(PatchError::Path(format!(
                        "{}: patch paths must stay inside the working directory",
                        path.display()
                    )));
                }
            }
            _ => {
                return Err(PatchError::Path(format!(
                    "{}: unsupported path component",
                    path.display()
                )));
            }
        }
    }
    Ok(resolved)
}

/// Apply an update hunk's chunks to a file's contents.
fn apply_chunks(
    original: &str,
    chunks: &[UpdateFileChunk],
    path: &Path,
) -> Result<String, PatchError> {
    let mut lines: Vec<String> = original.split('\n').map(String::from).collect();
    // `split` leaves a trailing empty element for the final newline; drop it so line indexes match
    // what a diff tool would report, and re-add it at the end.
    if lines.last().is_some_and(String::is_empty) {
        lines.pop();
    }

    let replacements = compute_replacements(&lines, chunks, path)?;
    let mut new_lines = apply_replacements(lines, &replacements);
    if !new_lines.last().is_some_and(String::is_empty) {
        new_lines.push(String::new());
    }
    Ok(new_lines.join("\n"))
}

/// Locate each chunk, producing `(start, old_len, new_lines)` edits in file order.
fn compute_replacements(
    original_lines: &[String],
    chunks: &[UpdateFileChunk],
    path: &Path,
) -> Result<Vec<(usize, usize, Vec<String>)>, PatchError> {
    let mut replacements = Vec::new();
    let mut line_index = 0usize;

    for chunk in chunks {
        // `@@ context` narrows the search window before matching the chunk itself.
        if let Some(ctx) = &chunk.change_context {
            match seek::seek_sequence(
                original_lines,
                std::slice::from_ref(ctx),
                line_index,
                /*eof*/ false,
            ) {
                Some(idx) => line_index = idx + 1,
                None => {
                    return Err(PatchError::Mismatch(format!(
                        "failed to find context '{ctx}' in {}",
                        path.display()
                    )));
                }
            }
        }

        if chunk.old_lines.is_empty() {
            // A pure insertion goes at the end of the file (before a trailing blank line).
            let at = if original_lines.last().is_some_and(String::is_empty) {
                original_lines.len() - 1
            } else {
                original_lines.len()
            };
            replacements.push((at, 0, chunk.new_lines.clone()));
            continue;
        }

        let mut pattern: &[String] = &chunk.old_lines;
        let mut new_slice: &[String] = &chunk.new_lines;
        let mut found =
            seek::seek_sequence(original_lines, pattern, line_index, chunk.is_end_of_file);

        // A chunk that reaches the end of the file often carries a trailing empty line standing in
        // for the final newline, which isn't in `original_lines`. Retry without it.
        if found.is_none() && pattern.last().is_some_and(String::is_empty) {
            pattern = &pattern[..pattern.len() - 1];
            if new_slice.last().is_some_and(String::is_empty) {
                new_slice = &new_slice[..new_slice.len() - 1];
            }
            found = seek::seek_sequence(original_lines, pattern, line_index, chunk.is_end_of_file);
        }

        match found {
            Some(start) => {
                replacements.push((start, pattern.len(), new_slice.to_vec()));
                line_index = start + pattern.len();
            }
            None => {
                return Err(PatchError::Mismatch(format!(
                    "failed to find these lines in {}:\n{}",
                    path.display(),
                    chunk.old_lines.join("\n")
                )));
            }
        }
    }

    replacements.sort_by_key(|(index, _, _)| *index);
    Ok(replacements)
}

/// Splice the edits in, back to front so earlier indexes stay valid.
fn apply_replacements(
    mut lines: Vec<String>,
    replacements: &[(usize, usize, Vec<String>)],
) -> Vec<String> {
    for (start, old_len, new_segment) in replacements.iter().rev() {
        let start = *start;
        for _ in 0..*old_len {
            if start < lines.len() {
                lines.remove(start);
            }
        }
        for (offset, line) in new_segment.iter().enumerate() {
            lines.insert(start + offset, line.clone());
        }
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(dir: &Path, name: &str, contents: &str) {
        let path = dir.join(name);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(path, contents).unwrap();
    }

    #[test]
    fn adds_updates_and_deletes_in_one_patch() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path();
        write(cwd, "keep.txt", "one\ntwo\nthree\n");
        write(cwd, "gone.txt", "bye\n");

        let patch = "*** Begin Patch\n\
             *** Add File: new/hello.txt\n\
             +hello\n\
             *** Update File: keep.txt\n\
             @@\n\
             -two\n\
             +TWO\n\
             *** Delete File: gone.txt\n\
             *** End Patch";
        let plan = plan(patch, cwd).unwrap();
        let summary = plan.apply().unwrap();

        assert_eq!(
            std::fs::read_to_string(cwd.join("new/hello.txt")).unwrap(),
            "hello\n"
        );
        assert_eq!(
            std::fs::read_to_string(cwd.join("keep.txt")).unwrap(),
            "one\nTWO\nthree\n"
        );
        assert!(!cwd.join("gone.txt").exists());
        assert!(summary.contains("A "), "{summary}");
        assert!(summary.contains("D "), "{summary}");
    }

    #[test]
    fn updates_with_a_rename_move_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path();
        write(cwd, "src/old.rs", "fn main() {}\n");

        let patch = "*** Begin Patch\n\
             *** Update File: src/old.rs\n\
             *** Move to: src/new.rs\n\
             @@\n\
             -fn main() {}\n\
             +fn main() { println!(\"hi\"); }\n\
             *** End Patch";
        plan(patch, cwd).unwrap().apply().unwrap();

        assert!(!cwd.join("src/old.rs").exists());
        assert_eq!(
            std::fs::read_to_string(cwd.join("src/new.rs")).unwrap(),
            "fn main() { println!(\"hi\"); }\n"
        );
    }

    #[test]
    fn context_markers_disambiguate_repeated_code() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path();
        write(cwd, "a.py", "def a():\n    pass\n\ndef b():\n    pass\n");

        let patch = "*** Begin Patch\n\
             *** Update File: a.py\n\
             @@ def b():\n\
             -    pass\n\
             +    return 2\n\
             *** End Patch";
        plan(patch, cwd).unwrap().apply().unwrap();
        assert_eq!(
            std::fs::read_to_string(cwd.join("a.py")).unwrap(),
            "def a():\n    pass\n\ndef b():\n    return 2\n"
        );
    }

    #[test]
    fn drifted_context_still_matches() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path();
        // The file has trailing whitespace and a smart quote the patch doesn't reproduce.
        write(cwd, "a.rs", "let s = \u{201C}hi\u{201D};   \nlet t = 1;\n");

        let patch = "*** Begin Patch\n\
             *** Update File: a.rs\n\
             @@\n\
             \x20let s = \"hi\";\n\
             -let t = 1;\n\
             +let t = 2;\n\
             *** End Patch";
        plan(patch, cwd).unwrap().apply().unwrap();
        assert!(
            std::fs::read_to_string(cwd.join("a.rs"))
                .unwrap()
                .contains("let t = 2;")
        );
    }

    #[test]
    fn nothing_is_written_when_one_hunk_does_not_apply() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path();
        write(cwd, "a.txt", "one\n");

        let patch = "*** Begin Patch\n\
             *** Add File: b.txt\n\
             +new file\n\
             *** Update File: a.txt\n\
             @@\n\
             -this line is not there\n\
             +replacement\n\
             *** End Patch";
        let err = plan(patch, cwd).unwrap_err();
        assert!(matches!(err, PatchError::Mismatch(_)), "{err}");
        assert!(
            !cwd.join("b.txt").exists(),
            "a failed patch writes nothing at all"
        );
    }

    #[test]
    fn paths_may_not_escape_the_working_directory() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path().join("project");
        std::fs::create_dir_all(&cwd).unwrap();

        for path in ["../outside.txt", "a/../../outside.txt"] {
            let patch = format!("*** Begin Patch\n*** Add File: {path}\n+nope\n*** End Patch");
            let err = plan(&patch, &cwd).unwrap_err();
            assert!(matches!(err, PatchError::Path(_)), "{path}: {err}");
        }
    }

    #[test]
    fn adding_over_an_existing_file_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "a.txt", "already here\n");
        let err = plan(
            "*** Begin Patch\n*** Add File: a.txt\n+new\n*** End Patch",
            dir.path(),
        )
        .unwrap_err();
        assert!(matches!(err, PatchError::Mismatch(_)), "{err}");
    }

    #[test]
    fn end_of_file_chunks_append_at_the_end() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "a.txt", "one\ntwo\n");
        let patch = "*** Begin Patch\n\
             *** Update File: a.txt\n\
             @@\n\
             +three\n\
             *** End of File\n\
             *** End Patch";
        plan(patch, dir.path()).unwrap().apply().unwrap();
        assert_eq!(
            std::fs::read_to_string(dir.path().join("a.txt")).unwrap(),
            "one\ntwo\nthree\n"
        );
    }

    #[test]
    fn the_preview_diff_names_each_file() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "a.txt", "one\n");
        let plan = plan(
            "*** Begin Patch\n*** Update File: a.txt\n@@\n-one\n+ONE\n*** End Patch",
            dir.path(),
        )
        .unwrap();
        let diff = plan.diff();
        assert!(diff.contains("M "), "{diff}");
        assert!(diff.contains("-one"), "{diff}");
        assert!(diff.contains("+ONE"), "{diff}");
    }
}
