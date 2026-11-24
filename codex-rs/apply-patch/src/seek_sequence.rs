/// Match strictness used to locate a patch sequence.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum MatchLevel {
    Exact,
    Rstrip,
    Trim,
    Unicode,
}

impl std::fmt::Display for MatchLevel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            MatchLevel::Exact => "exact",
            MatchLevel::Rstrip => "trailing-whitespace-insensitive",
            MatchLevel::Trim => "whitespace-insensitive",
            MatchLevel::Unicode => "unicode-normalized",
        })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct SequenceMatch {
    pub(crate) index: usize,
    pub(crate) level: MatchLevel,
    /// Number of locations that matched at `level`.
    pub(crate) match_count: usize,
}

/// Attempt to find the sequence of `pattern` lines within `lines` beginning at or after `start`.
/// Returns the first match and the strictness level used. Matches are attempted with
/// decreasing strictness: exact match, then ignoring trailing whitespace, then ignoring leading
/// and trailing whitespace. When `eof` is true, we first try starting at the end-of-file (so that
/// patterns intended to match file endings are applied at the end), and fall back to searching
/// from `start` if needed.
///
/// Special cases handled defensively:
///  • Empty `pattern` → returns `Some(start)` (no-op match)
///  • `pattern.len() > lines.len()` → returns `None` (cannot match, avoids
///    out‑of‑bounds panic that occurred pre‑2025‑04‑12)
pub(crate) fn seek_sequence(
    lines: &[String],
    pattern: &[String],
    start: usize,
    eof: bool,
) -> Option<SequenceMatch> {
    if pattern.is_empty() {
        return Some(SequenceMatch {
            index: start,
            level: MatchLevel::Exact,
            match_count: 1,
        });
    }

    // When the pattern is longer than the available input there is no possible
    // match. Early‑return to avoid the out‑of‑bounds slice that would occur in
    // the search loops below (previously caused a panic when
    // `pattern.len() > lines.len()`).
    if pattern.len() > lines.len() {
        return None;
    }
    let search_start = if eof && lines.len() >= pattern.len() {
        let eof_start = lines.len() - pattern.len();
        eof_start.max(start)
    } else {
        start
    };
    let find_matches = |matches: &mut dyn FnMut(usize, usize) -> bool| {
        let mut indices = Vec::new();
        for i in search_start..=lines.len().saturating_sub(pattern.len()) {
            if matches(i, pattern.len()) {
                indices.push(i);
            }
        }
        indices
    };

    let exact = find_matches(&mut |i, len| lines[i..i + len] == *pattern);
    if let Some(first) = exact.first() {
        return Some(SequenceMatch {
            index: *first,
            level: MatchLevel::Exact,
            match_count: exact.len(),
        });
    }

    let rstrip = find_matches(&mut |i, len| {
        pattern.iter().enumerate().all(|(pattern_index, expected)| {
            lines[i + pattern_index].trim_end() == expected.trim_end()
        }) && len == pattern.len()
    });
    if let Some(first) = rstrip.first() {
        return Some(SequenceMatch {
            index: *first,
            level: MatchLevel::Rstrip,
            match_count: rstrip.len(),
        });
    }

    let trim = find_matches(&mut |i, len| {
        pattern
            .iter()
            .enumerate()
            .all(|(pattern_index, expected)| lines[i + pattern_index].trim() == expected.trim())
            && len == pattern.len()
    });
    if let Some(first) = trim.first() {
        return Some(SequenceMatch {
            index: *first,
            level: MatchLevel::Trim,
            match_count: trim.len(),
        });
    }

    // ------------------------------------------------------------------
    // Final, most permissive pass – attempt to match after *normalising*
    // common Unicode punctuation to their ASCII equivalents so that diffs
    // authored with plain ASCII characters can still be applied to source
    // files that contain typographic dashes / quotes, etc.  This mirrors the
    // fuzzy behaviour of `git apply` which ignores minor byte-level
    // differences when locating context lines.
    // ------------------------------------------------------------------

    fn normalise(s: &str) -> String {
        s.trim()
            .chars()
            .map(|c| match c {
                // Various dash / hyphen code-points → ASCII '-'
                '\u{2010}' | '\u{2011}' | '\u{2012}' | '\u{2013}' | '\u{2014}' | '\u{2015}'
                | '\u{2212}' => '-',
                // Fancy single quotes → '\''
                '\u{2018}' | '\u{2019}' | '\u{201A}' | '\u{201B}' => '\'',
                // Fancy double quotes → '"'
                '\u{201C}' | '\u{201D}' | '\u{201E}' | '\u{201F}' => '"',
                // Non-breaking space and other odd spaces → normal space
                '\u{00A0}' | '\u{2002}' | '\u{2003}' | '\u{2004}' | '\u{2005}' | '\u{2006}'
                | '\u{2007}' | '\u{2008}' | '\u{2009}' | '\u{200A}' | '\u{202F}' | '\u{205F}'
                | '\u{3000}' => ' ',
                other => other,
            })
            .collect::<String>()
    }

    let unicode = find_matches(&mut |i, len| {
        pattern.iter().enumerate().all(|(pattern_index, expected)| {
            normalise(&lines[i + pattern_index]) == normalise(expected)
        }) && len == pattern.len()
    });
    unicode.first().map(|first| SequenceMatch {
        index: *first,
        level: MatchLevel::Unicode,
        match_count: unicode.len(),
    })
}

#[cfg(test)]
mod tests {
    use super::MatchLevel;
    use super::seek_sequence;
    use std::string::ToString;

    fn to_vec(strings: &[&str]) -> Vec<String> {
        strings.iter().map(ToString::to_string).collect()
    }

    #[test]
    fn test_exact_match_finds_sequence() {
        let lines = to_vec(&["foo", "bar", "baz"]);
        let pattern = to_vec(&["bar", "baz"]);
        assert_eq!(
            seek_sequence(&lines, &pattern, /*start*/ 0, /*eof*/ false)
                .map(|matched| (matched.index, matched.level)),
            Some((1, MatchLevel::Exact))
        );
    }

    #[test]
    fn test_rstrip_match_ignores_trailing_whitespace() {
        let lines = to_vec(&["foo   ", "bar\t\t"]);
        // Pattern omits trailing whitespace.
        let pattern = to_vec(&["foo", "bar"]);
        assert_eq!(
            seek_sequence(&lines, &pattern, /*start*/ 0, /*eof*/ false)
                .map(|matched| (matched.index, matched.level)),
            Some((0, MatchLevel::Rstrip))
        );
    }

    #[test]
    fn test_trim_match_ignores_leading_and_trailing_whitespace() {
        let lines = to_vec(&["    foo   ", "   bar\t"]);
        // Pattern omits any additional whitespace.
        let pattern = to_vec(&["foo", "bar"]);
        assert_eq!(
            seek_sequence(&lines, &pattern, /*start*/ 0, /*eof*/ false)
                .map(|matched| (matched.index, matched.level)),
            Some((0, MatchLevel::Trim))
        );
    }

    #[test]
    fn test_pattern_longer_than_input_returns_none() {
        let lines = to_vec(&["just one line"]);
        let pattern = to_vec(&["too", "many", "lines"]);
        // Should not panic – must return None when pattern cannot possibly fit.
        assert_eq!(
            seek_sequence(&lines, &pattern, /*start*/ 0, /*eof*/ false,),
            None
        );
    }
}
