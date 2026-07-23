//! Locating a block of lines in a file, with graduated tolerance.
//!
//! A model writing a patch reproduces context from memory, so it drifts: trailing whitespace goes
//! missing, indentation shifts, an em dash becomes a hyphen. Rather than reject the patch, we retry
//! the search with progressively looser comparisons — exact, ignoring trailing whitespace, ignoring
//! all surrounding whitespace, then normalizing typographic punctuation to ASCII. This is what makes
//! patch application land instead of bouncing back as "context not found".

/// Find `pattern` in `lines` at or after `start`, returning its start index.
///
/// With `eof` set, the search starts at the end of the file first, so a chunk meant to change the
/// tail of a file is anchored there.
pub fn seek_sequence(
    lines: &[String],
    pattern: &[String],
    start: usize,
    eof: bool,
) -> Option<usize> {
    if pattern.is_empty() {
        return Some(start);
    }
    // A pattern longer than the file can never match; bail before slicing out of bounds.
    if pattern.len() > lines.len() {
        return None;
    }
    let search_start = if eof && lines.len() >= pattern.len() {
        lines.len() - pattern.len()
    } else {
        start
    };
    let last_start = lines.len().saturating_sub(pattern.len());

    let passes: [fn(&str) -> String; 4] = [
        |s| s.to_string(),
        |s| s.trim_end().to_string(),
        |s| s.trim().to_string(),
        |s| normalize_punctuation(s.trim()),
    ];
    for compare in passes {
        for i in search_start..=last_start {
            if pattern
                .iter()
                .enumerate()
                .all(|(p, pat)| compare(&lines[i + p]) == compare(pat))
            {
                return Some(i);
            }
        }
    }
    None
}

/// Map typographic punctuation to its ASCII equivalent so a patch written in plain ASCII still
/// matches source that contains smart quotes or en dashes (mirrors `git apply`'s leniency).
fn normalize_punctuation(s: &str) -> String {
    s.chars()
        .map(|c| match c {
            // Dashes and hyphens.
            '\u{2010}' | '\u{2011}' | '\u{2012}' | '\u{2013}' | '\u{2014}' | '\u{2015}'
            | '\u{2212}' => '-',
            // Single quotes.
            '\u{2018}' | '\u{2019}' | '\u{201A}' | '\u{201B}' => '\'',
            // Double quotes.
            '\u{201C}' | '\u{201D}' | '\u{201E}' | '\u{201F}' => '"',
            // Exotic spaces.
            '\u{00A0}' | '\u{2002}' | '\u{2003}' | '\u{2004}' | '\u{2005}' | '\u{2006}'
            | '\u{2007}' | '\u{2008}' | '\u{2009}' | '\u{200A}' | '\u{202F}' | '\u{205F}'
            | '\u{3000}' => ' ',
            other => other,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::seek_sequence;

    fn v(strings: &[&str]) -> Vec<String> {
        strings.iter().map(|s| (*s).to_string()).collect()
    }

    #[test]
    fn exact_match_finds_sequence() {
        assert_eq!(
            seek_sequence(&v(&["foo", "bar", "baz"]), &v(&["bar", "baz"]), 0, false),
            Some(1)
        );
    }

    #[test]
    fn trailing_whitespace_is_ignored() {
        assert_eq!(
            seek_sequence(&v(&["foo   ", "bar\t\t"]), &v(&["foo", "bar"]), 0, false),
            Some(0)
        );
    }

    #[test]
    fn leading_and_trailing_whitespace_is_ignored() {
        assert_eq!(
            seek_sequence(
                &v(&["    foo   ", "   bar\t"]),
                &v(&["foo", "bar"]),
                0,
                false
            ),
            Some(0)
        );
    }

    #[test]
    fn typographic_punctuation_still_matches() {
        let lines = v(&["let s = \u{201C}hi\u{201D};", "// em\u{2014}dash"]);
        let pattern = v(&["let s = \"hi\";", "// em-dash"]);
        assert_eq!(seek_sequence(&lines, &pattern, 0, false), Some(0));
    }

    #[test]
    fn pattern_longer_than_input_returns_none() {
        assert_eq!(
            seek_sequence(
                &v(&["just one line"]),
                &v(&["too", "many", "lines"]),
                0,
                false
            ),
            None
        );
    }

    #[test]
    fn eof_anchors_the_search_at_the_end() {
        let lines = v(&["x", "x", "x"]);
        assert_eq!(seek_sequence(&lines, &v(&["x"]), 0, true), Some(2));
        assert_eq!(seek_sequence(&lines, &v(&["x"]), 0, false), Some(0));
    }

    #[test]
    fn search_starts_at_the_given_index() {
        let lines = v(&["a", "b", "a", "b"]);
        assert_eq!(seek_sequence(&lines, &v(&["a"]), 1, false), Some(2));
    }
}
