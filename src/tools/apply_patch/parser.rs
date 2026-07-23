//! Parses the `apply_patch` envelope into hunks. It validates the format only — whether the patch
//! *fits* the files on disk is decided later, in [`super::plan`].
//!
//! The grammar:
//!
//! ```text
//! patch      := "*** Begin Patch" LF hunk* "*** End Patch" LF?
//! hunk       := add | delete | update
//! add        := "*** Add File: " path LF ("+" line LF)*
//! delete     := "*** Delete File: " path LF
//! update     := "*** Update File: " path LF ("*** Move to: " path LF)? chunk*
//! chunk      := ("@@" | "@@ " context) LF ((" " | "+" | "-") line LF)* ("*** End of File" LF)?
//! ```
//!
//! Two deliberate leniencies: markers may carry surrounding whitespace, and an update hunk may omit
//! the leading `@@` for its first chunk (models routinely do).

use std::path::PathBuf;

const BEGIN_PATCH_MARKER: &str = "*** Begin Patch";
const END_PATCH_MARKER: &str = "*** End Patch";
const ADD_FILE_MARKER: &str = "*** Add File: ";
const DELETE_FILE_MARKER: &str = "*** Delete File: ";
const UPDATE_FILE_MARKER: &str = "*** Update File: ";
const MOVE_TO_MARKER: &str = "*** Move to: ";
const EOF_MARKER: &str = "*** End of File";
const CHANGE_CONTEXT_MARKER: &str = "@@ ";
const EMPTY_CHANGE_CONTEXT_MARKER: &str = "@@";

/// Why a patch could not be parsed. The messages are written for the model: they name the offending
/// line and what was expected, so the next attempt can be correct.
#[derive(Debug, Clone, PartialEq)]
pub enum ParseError {
    InvalidPatch(String),
    InvalidHunk { message: String, line_number: usize },
}

impl std::fmt::Display for ParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ParseError::InvalidPatch(m) => write!(f, "invalid patch: {m}"),
            ParseError::InvalidHunk {
                message,
                line_number,
            } => write!(f, "invalid hunk at line {line_number}, {message}"),
        }
    }
}

/// One file operation from the patch.
// `AddFile`/`DeleteFile`/`UpdateFile` are the patch format's own names for these operations.
#[allow(clippy::enum_variant_names)]
#[derive(Debug, Clone, PartialEq)]
pub enum Hunk {
    AddFile {
        path: PathBuf,
        contents: String,
    },
    DeleteFile {
        path: PathBuf,
    },
    UpdateFile {
        path: PathBuf,
        move_path: Option<PathBuf>,
        /// In file order: each chunk's context must appear after the previous chunk's.
        chunks: Vec<UpdateFileChunk>,
    },
}

impl Hunk {
    /// The path the hunk ends up affecting (the move destination for a rename).
    pub fn path(&self) -> &std::path::Path {
        match self {
            Hunk::AddFile { path, .. } | Hunk::DeleteFile { path } => path,
            Hunk::UpdateFile {
                move_path: Some(path),
                ..
            } => path,
            Hunk::UpdateFile { path, .. } => path,
        }
    }
}

/// A contiguous edit inside an update hunk.
#[derive(Debug, Clone, PartialEq)]
pub struct UpdateFileChunk {
    /// A `@@`-marked line (usually a function or class header) used to narrow where to search.
    pub change_context: Option<String>,
    /// Lines to find, and what to put in their place.
    pub old_lines: Vec<String>,
    pub new_lines: Vec<String>,
    /// The chunk must match at the end of the file.
    pub is_end_of_file: bool,
}

/// Parse a whole patch.
///
/// A patch wrapped in a heredoc (`<<'EOF' … EOF`) is unwrapped first: some models emit the shell
/// form even when the argument is passed directly.
pub fn parse_patch(patch: &str) -> Result<Vec<Hunk>, ParseError> {
    let lines: Vec<&str> = patch.trim().lines().collect();
    let lines = strip_heredoc(&lines)?;
    Parser::default().run(lines)
}

/// Accept `<<EOF … EOF` wrappers around an otherwise valid patch.
fn strip_heredoc<'a>(lines: &'a [&'a str]) -> Result<&'a [&'a str], ParseError> {
    let boundaries_ok = |lines: &[&str]| -> Result<(), ParseError> {
        let (first, last) = match lines {
            [] => (None, None),
            [only] => (Some(*only), Some(*only)),
            [first, .., last] => (Some(*first), Some(*last)),
        };
        match (first.map(str::trim), last.map(str::trim)) {
            (Some(f), Some(l)) if f == BEGIN_PATCH_MARKER && l == END_PATCH_MARKER => Ok(()),
            (Some(f), _) if f != BEGIN_PATCH_MARKER => Err(ParseError::InvalidPatch(
                "The first line of the patch must be '*** Begin Patch'".to_string(),
            )),
            _ => Err(ParseError::InvalidPatch(
                "The last line of the patch must be '*** End Patch'".to_string(),
            )),
        }
    };

    let outer = match boundaries_ok(lines) {
        Ok(()) => return Ok(lines),
        Err(e) => e,
    };
    match lines {
        [first, .., last]
            if matches!(*first, "<<EOF" | "<<'EOF'" | "<<\"EOF\"")
                && last.ends_with("EOF")
                && lines.len() >= 4 =>
        {
            let inner = &lines[1..lines.len() - 1];
            boundaries_ok(inner)?;
            Ok(inner)
        }
        _ => Err(outer),
    }
}

#[derive(Default)]
struct Parser {
    mode: Mode,
    hunks: Vec<Hunk>,
    line_number: usize,
}

#[derive(Default, Clone, Copy)]
enum Mode {
    #[default]
    NotStarted,
    Started,
    AddFile,
    DeleteFile,
    UpdateFile {
        hunk_line_number: usize,
    },
    Ended,
}

const HUNK_HEADER_HELP: &str = "is not a valid hunk header. Valid hunk headers: '*** Add File: {path}', '*** Delete File: {path}', '*** Update File: {path}'";
const DIFF_LINE_HELP: &str =
    "Every line should start with ' ' (context line), '+' (added line), or '-' (removed line)";

impl Parser {
    fn run(mut self, lines: &[&str]) -> Result<Vec<Hunk>, ParseError> {
        for line in lines {
            self.line_number += 1;
            self.process_line(line)?;
        }
        if !matches!(self.mode, Mode::Ended) {
            return Err(ParseError::InvalidPatch(
                "The last line of the patch must be '*** End Patch'".to_string(),
            ));
        }
        Ok(self.hunks)
    }

    /// An update hunk that never got any lines is a mistake worth reporting precisely — otherwise it
    /// would silently apply as a no-op.
    fn ensure_update_hunk_is_not_empty(&self, line: &str) -> Result<(), ParseError> {
        let Some(Hunk::UpdateFile { path, chunks, .. }) = self.hunks.last() else {
            return Ok(());
        };
        if chunks.is_empty()
            && let Mode::UpdateFile { hunk_line_number } = self.mode
        {
            return Err(ParseError::InvalidHunk {
                message: format!("Update file hunk for path '{}' is empty", path.display()),
                line_number: hunk_line_number,
            });
        }
        if chunks
            .last()
            .is_some_and(|c| c.old_lines.is_empty() && c.new_lines.is_empty())
        {
            let message = if line == END_PATCH_MARKER {
                "Update hunk does not contain any lines".to_string()
            } else {
                format!("Unexpected line found in update hunk: '{line}'. {DIFF_LINE_HELP}")
            };
            return Err(ParseError::InvalidHunk {
                message,
                line_number: self.line_number,
            });
        }
        Ok(())
    }

    /// Handle the markers that can appear between hunks. Returns true when the line was consumed.
    fn handle_header(&mut self, trimmed: &str) -> Result<bool, ParseError> {
        if trimmed == END_PATCH_MARKER {
            self.ensure_update_hunk_is_not_empty(trimmed)?;
            self.mode = Mode::Ended;
            return Ok(true);
        }
        if let Some(path) = trimmed.strip_prefix(ADD_FILE_MARKER) {
            self.ensure_update_hunk_is_not_empty(trimmed)?;
            self.hunks.push(Hunk::AddFile {
                path: PathBuf::from(path),
                contents: String::new(),
            });
            self.mode = Mode::AddFile;
            return Ok(true);
        }
        if let Some(path) = trimmed.strip_prefix(DELETE_FILE_MARKER) {
            self.ensure_update_hunk_is_not_empty(trimmed)?;
            self.hunks.push(Hunk::DeleteFile {
                path: PathBuf::from(path),
            });
            self.mode = Mode::DeleteFile;
            return Ok(true);
        }
        if let Some(path) = trimmed.strip_prefix(UPDATE_FILE_MARKER) {
            self.ensure_update_hunk_is_not_empty(trimmed)?;
            self.hunks.push(Hunk::UpdateFile {
                path: PathBuf::from(path),
                move_path: None,
                chunks: Vec::new(),
            });
            self.mode = Mode::UpdateFile {
                hunk_line_number: self.line_number,
            };
            return Ok(true);
        }
        Ok(false)
    }

    fn bad_header(&self, trimmed: &str) -> ParseError {
        ParseError::InvalidHunk {
            message: format!("'{trimmed}' {HUNK_HEADER_HELP}"),
            line_number: self.line_number,
        }
    }

    fn process_line(&mut self, line: &str) -> Result<(), ParseError> {
        let trimmed = line.trim();
        match self.mode {
            Mode::NotStarted => {
                if trimmed == BEGIN_PATCH_MARKER {
                    self.mode = Mode::Started;
                    return Ok(());
                }
                Err(ParseError::InvalidPatch(
                    "The first line of the patch must be '*** Begin Patch'".to_string(),
                ))
            }
            Mode::Started | Mode::DeleteFile => {
                if self.handle_header(trimmed)? {
                    return Ok(());
                }
                Err(self.bad_header(trimmed))
            }
            Mode::AddFile => {
                if self.handle_header(trimmed)? {
                    return Ok(());
                }
                if let Some(added) = line.strip_prefix('+')
                    && let Some(Hunk::AddFile { contents, .. }) = self.hunks.last_mut()
                {
                    contents.push_str(added);
                    contents.push('\n');
                    return Ok(());
                }
                Err(self.bad_header(trimmed))
            }
            Mode::UpdateFile { hunk_line_number } => {
                self.process_update_line(line, hunk_line_number)
            }
            Mode::Ended => {
                if trimmed.is_empty() {
                    Ok(())
                } else {
                    Err(ParseError::InvalidPatch(format!(
                        "Unexpected line after the end of the patch: '{trimmed}'"
                    )))
                }
            }
        }
    }

    fn process_update_line(
        &mut self,
        line: &str,
        hunk_line_number: usize,
    ) -> Result<(), ParseError> {
        let update_line = line.trim_end();
        if self.handle_header(update_line)? {
            return Ok(());
        }
        let line_number = self.line_number;
        let Some(Hunk::UpdateFile {
            move_path, chunks, ..
        }) = self.hunks.last_mut()
        else {
            return Err(ParseError::InvalidHunk {
                message: format!(
                    "Unexpected line found in update hunk: '{line}'. {DIFF_LINE_HELP}"
                ),
                line_number,
            });
        };

        // After `*** End of File`, only a new `@@` chunk (or a blank line) may follow.
        if chunks.last().is_some_and(|c| c.is_end_of_file) {
            if update_line.is_empty() {
                return Ok(());
            }
            if update_line != EMPTY_CHANGE_CONTEXT_MARKER
                && !update_line.starts_with(CHANGE_CONTEXT_MARKER)
            {
                return Err(ParseError::InvalidHunk {
                    message: format!(
                        "Expected update hunk to start with a @@ context marker, got: '{line}'"
                    ),
                    line_number,
                });
            }
        }

        // `*** Move to:` must come before any chunk.
        if chunks.is_empty()
            && move_path.is_none()
            && let Some(dest) = update_line.strip_prefix(MOVE_TO_MARKER)
        {
            *move_path = Some(PathBuf::from(dest));
            return Ok(());
        }

        let is_context_marker = update_line == EMPTY_CHANGE_CONTEXT_MARKER
            || update_line.starts_with(CHANGE_CONTEXT_MARKER);
        let last_chunk_is_empty = chunks
            .last()
            .is_some_and(|c| c.old_lines.is_empty() && c.new_lines.is_empty());
        if is_context_marker && last_chunk_is_empty {
            return Err(ParseError::InvalidHunk {
                message: format!(
                    "Unexpected line found in update hunk: '{line}'. {DIFF_LINE_HELP}"
                ),
                line_number,
            });
        }

        if update_line == EMPTY_CHANGE_CONTEXT_MARKER {
            chunks.push(new_chunk(None));
            return Ok(());
        }
        if let Some(context) = update_line.strip_prefix(CHANGE_CONTEXT_MARKER) {
            chunks.push(new_chunk(Some(context.to_string())));
            return Ok(());
        }
        if update_line == EOF_MARKER {
            if last_chunk_is_empty || chunks.is_empty() {
                return Err(ParseError::InvalidHunk {
                    message: "Update hunk does not contain any lines".to_string(),
                    line_number,
                });
            }
            if let Some(chunk) = chunks.last_mut() {
                chunk.is_end_of_file = true;
            }
            return Ok(());
        }

        // Diff lines. The first chunk may be implicit — models often omit the leading `@@`.
        let (old, new) = if line.is_empty() {
            (Some(String::new()), Some(String::new()))
        } else if let Some(rest) = line.strip_prefix(' ') {
            (Some(rest.to_string()), Some(rest.to_string()))
        } else if let Some(rest) = line.strip_prefix('+') {
            (None, Some(rest.to_string()))
        } else if let Some(rest) = line.strip_prefix('-') {
            (Some(rest.to_string()), None)
        } else {
            let message = if chunks
                .last()
                .is_some_and(|c| !c.old_lines.is_empty() || !c.new_lines.is_empty())
            {
                format!("Expected update hunk to start with a @@ context marker, got: '{line}'")
            } else {
                format!("Unexpected line found in update hunk: '{line}'. {DIFF_LINE_HELP}")
            };
            return Err(ParseError::InvalidHunk {
                message,
                line_number,
            });
        };

        if chunks.is_empty() {
            chunks.push(new_chunk(None));
        }
        if let Some(chunk) = chunks.last_mut() {
            if let Some(old) = old {
                chunk.old_lines.push(old);
            }
            if let Some(new) = new {
                chunk.new_lines.push(new);
            }
        }
        let _ = hunk_line_number;
        Ok(())
    }
}

fn new_chunk(change_context: Option<String>) -> UpdateFileChunk {
    UpdateFileChunk {
        change_context,
        old_lines: Vec::new(),
        new_lines: Vec::new(),
        is_end_of_file: false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requires_begin_and_end_markers() {
        assert_eq!(
            parse_patch("bad"),
            Err(ParseError::InvalidPatch(
                "The first line of the patch must be '*** Begin Patch'".into()
            ))
        );
        assert_eq!(
            parse_patch("*** Begin Patch\nbad"),
            Err(ParseError::InvalidPatch(
                "The last line of the patch must be '*** End Patch'".into()
            ))
        );
    }

    #[test]
    fn parses_add_delete_and_update_with_move() {
        let hunks = parse_patch(
            "*** Begin Patch\n\
             *** Add File: path/add.py\n\
             +abc\n\
             +def\n\
             *** Delete File: path/delete.py\n\
             *** Update File: path/update.py\n\
             *** Move to: path/update2.py\n\
             @@ def f():\n\
             -    pass\n\
             +    return 123\n\
             *** End Patch",
        )
        .unwrap();
        assert_eq!(
            hunks,
            vec![
                Hunk::AddFile {
                    path: PathBuf::from("path/add.py"),
                    contents: "abc\ndef\n".into(),
                },
                Hunk::DeleteFile {
                    path: PathBuf::from("path/delete.py"),
                },
                Hunk::UpdateFile {
                    path: PathBuf::from("path/update.py"),
                    move_path: Some(PathBuf::from("path/update2.py")),
                    chunks: vec![UpdateFileChunk {
                        change_context: Some("def f():".into()),
                        old_lines: vec!["    pass".into()],
                        new_lines: vec!["    return 123".into()],
                        is_end_of_file: false,
                    }],
                },
            ]
        );
    }

    #[test]
    fn an_update_hunk_may_omit_the_first_context_marker() {
        let hunks = parse_patch(
            "*** Begin Patch\n*** Update File: file2.py\n import foo\n+bar\n*** End Patch",
        )
        .unwrap();
        assert_eq!(
            hunks,
            vec![Hunk::UpdateFile {
                path: PathBuf::from("file2.py"),
                move_path: None,
                chunks: vec![UpdateFileChunk {
                    change_context: None,
                    old_lines: vec!["import foo".into()],
                    new_lines: vec!["import foo".into(), "bar".into()],
                    is_end_of_file: false,
                }],
            }]
        );
    }

    #[test]
    fn preserves_the_end_of_file_marker() {
        let hunks = parse_patch(
            "*** Begin Patch\n*** Update File: file.txt\n@@\n+quux\n*** End of File\n\n*** End Patch",
        )
        .unwrap();
        let Hunk::UpdateFile { chunks, .. } = &hunks[0] else {
            panic!("expected an update hunk");
        };
        assert!(chunks[0].is_end_of_file);
    }

    #[test]
    fn an_empty_update_hunk_is_rejected() {
        assert_eq!(
            parse_patch("*** Begin Patch\n*** Update File: test.py\n*** End Patch"),
            Err(ParseError::InvalidHunk {
                message: "Update file hunk for path 'test.py' is empty".into(),
                line_number: 2,
            })
        );
    }

    #[test]
    fn an_unmarked_diff_line_is_rejected_with_guidance() {
        let err = parse_patch(
            "*** Begin Patch\n*** Update File: a.txt\n@@\n+ok\nnot a diff line\n*** End Patch",
        )
        .unwrap_err();
        let ParseError::InvalidHunk { message, .. } = err else {
            panic!("expected a hunk error");
        };
        assert!(message.contains("@@ context marker"), "{message}");
    }

    #[test]
    fn an_empty_patch_is_valid_and_does_nothing() {
        assert_eq!(
            parse_patch("*** Begin Patch\n*** End Patch").unwrap(),
            vec![]
        );
    }

    #[test]
    fn a_heredoc_wrapper_is_stripped() {
        let inner = "*** Begin Patch\n*** Add File: a.txt\n+hi\n*** End Patch";
        for wrapper in ["<<EOF", "<<'EOF'", "<<\"EOF\""] {
            let hunks = parse_patch(&format!("{wrapper}\n{inner}\nEOF\n")).unwrap();
            assert_eq!(
                hunks,
                vec![Hunk::AddFile {
                    path: PathBuf::from("a.txt"),
                    contents: "hi\n".into(),
                }]
            );
        }
        // A mismatched wrapper is still a plain parse error.
        assert!(parse_patch(&format!("<<\"EOF'\n{inner}\nEOF\n")).is_err());
    }

    #[test]
    fn markers_tolerate_surrounding_whitespace() {
        let hunks = parse_patch("  *** Begin Patch  \n*** Add File: a.txt\n+hi\n  *** End Patch  ")
            .unwrap();
        assert_eq!(hunks.len(), 1);
    }
}
