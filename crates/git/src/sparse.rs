//! Sparse-checkout patterns, as `sparse:oid=<blob>` partial clones use them:
//! the blob holds lines in `.gitignore` syntax, and a file is wanted if the
//! last pattern that says anything about it, or about a directory it is in,
//! is not negated.

/// One line of a pattern file.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Pattern {
    glob: Vec<u8>,
    negated: bool,
    /// Ended in `/`: matches directories only.
    directory: bool,
    /// Holds a `/` other than a trailing one: matched against the whole
    /// path. Otherwise against the last component, at any depth.
    anchored: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Patterns {
    patterns: Vec<Pattern>,
}

impl Patterns {
    pub fn parse(text: &[u8]) -> Patterns {
        let mut patterns = Vec::new();
        for line in text.split(|byte| *byte == b'\n') {
            let mut line = line.strip_suffix(b"\r").unwrap_or(line);
            // Trailing spaces do not count unless escaped.
            while line.ends_with(b" ") && !line.ends_with(b"\\ ") {
                line = &line[..line.len() - 1];
            }
            if line.is_empty() || line.starts_with(b"#") {
                continue;
            }
            let negated = line.starts_with(b"!");
            if negated {
                line = &line[1..];
            }
            let directory = line.ends_with(b"/");
            if directory {
                line = &line[..line.len() - 1];
            }
            let anchored = line.contains(&b'/');
            let glob = line.strip_prefix(b"/").unwrap_or(line).to_vec();
            if !glob.is_empty() {
                patterns.push(Pattern { glob, negated, directory, anchored });
            }
        }
        Patterns { patterns }
    }

    /// What the patterns say about `path` itself, if anything.
    fn verdict(&self, path: &[u8], is_directory: bool) -> Option<bool> {
        let name = path.rsplit(|byte| *byte == b'/').next().unwrap_or(path);
        self.patterns.iter().rev().find_map(|pattern| {
            if pattern.directory && !is_directory {
                return None;
            }
            let subject = if pattern.anchored { path } else { name };
            glob_match(&pattern.glob, subject).then_some(!pattern.negated)
        })
    }

    /// Whether the file at `path` (slash-separated, from the root) is wanted.
    pub fn includes(&self, path: &[u8]) -> bool {
        let mut end = path.len();
        let mut is_directory = false;
        loop {
            if let Some(verdict) = self.verdict(&path[..end], is_directory) {
                return verdict;
            }
            // Nothing about this one: what goes for its directory goes for it.
            match path[..end].iter().rposition(|byte| *byte == b'/') {
                Some(slash) => end = slash,
                None => return false,
            }
            is_directory = true;
        }
    }
}

/// Shell-style matching where `*` and `?` stop at `/`, and `**` does not.
fn glob_match(pattern: &[u8], text: &[u8]) -> bool {
    let Some((&first, rest)) = pattern.split_first() else { return text.is_empty() };
    match first {
        b'*' if rest.first() == Some(&b'*') => {
            let rest = &rest[1..];
            // "**/" also matches no directory at all.
            if let Some(after) = rest.strip_prefix(b"/") {
                if glob_match(after, text) {
                    return true;
                }
            }
            (0..=text.len()).any(|skip| glob_match(rest, &text[skip..]))
        }
        b'*' => {
            let reach = text.iter().position(|byte| *byte == b'/').unwrap_or(text.len());
            (0..=reach).any(|skip| glob_match(rest, &text[skip..]))
        }
        b'?' => text.first().is_some_and(|byte| *byte != b'/') && glob_match(rest, &text[1..]),
        b'[' => {
            let Some((&byte, text_rest)) = text.split_first() else { return false };
            match class_match(rest, byte) {
                Some((matched, after)) => matched && byte != b'/' && glob_match(after, text_rest),
                // No closing bracket: an ordinary character.
                None => byte == b'[' && glob_match(rest, text_rest),
            }
        }
        b'\\' if !rest.is_empty() => text.first() == Some(&rest[0]) && glob_match(&rest[1..], &text[1..]),
        _ => text.first() == Some(&first) && glob_match(rest, &text[1..]),
    }
}

/// Matches `byte` against the bracket expression whose inside starts at
/// `pattern`. Returns the verdict and what follows the closing bracket.
fn class_match(pattern: &[u8], byte: u8) -> Option<(bool, &[u8])> {
    let negated = matches!(pattern.first(), Some(b'!' | b'^'));
    let mut at = usize::from(negated);
    let mut matched = false;
    let mut first = true;
    loop {
        let low = *pattern.get(at)?;
        if low == b']' && !first {
            return Some((matched != negated, &pattern[at + 1..]));
        }
        first = false;
        if pattern.get(at + 1) == Some(&b'-') && pattern.get(at + 2).is_some_and(|high| *high != b']') {
            matched |= (low..=pattern[at + 2]).contains(&byte);
            at += 3;
        } else {
            matched |= low == byte;
            at += 1;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{glob_match, Patterns};

    #[test]
    fn globs() {
        for (pattern, text, expected) in [
            ("*.rs", "main.rs", true),
            ("*.rs", "src/main.rs", false),
            ("src/*.rs", "src/main.rs", true),
            ("src/*.rs", "src/a/main.rs", false),
            ("src/**/*.rs", "src/a/b/main.rs", true),
            ("src/**/*.rs", "src/main.rs", true),
            ("**/test", "a/b/test", true),
            ("**/test", "test", true),
            ("docs/**", "docs/a/b", true),
            ("a?c", "abc", true),
            ("a?c", "a/c", false),
            ("[a-c]x", "bx", true),
            ("[!a-c]x", "bx", false),
            ("[]]x", "]x", true),
            ("\\*", "*", true),
            ("\\*", "a", false),
        ] {
            assert_eq!(glob_match(pattern.as_bytes(), text.as_bytes()), expected, "{pattern} against {text}");
        }
    }

    #[test]
    fn pattern_lists() {
        let patterns = Patterns::parse(b"# what to keep\n/docs/\n*.md\n!/docs/internal/\nsrc/lib.rs\n\n!CHANGELOG.md\n");
        for (path, expected) in [
            ("docs/guide.txt", true),
            ("docs/deep/er/file", true),
            ("docs/internal/plan.txt", false),
            // What is said about the file itself outweighs what is said about its directory.
            ("docs/internal/plan.md", true),
            ("README.md", true),
            ("src/notes.md", true),
            ("CHANGELOG.md", false),
            ("src/lib.rs", true),
            ("src/main.rs", false),
            ("other/src/lib.rs", false),
            ("more/docs/guide.txt", false),
        ] {
            assert_eq!(patterns.includes(path.as_bytes()), expected, "{path}");
        }
        // Everything, then exceptions: the usual shape of a sparse-checkout file.
        let patterns = Patterns::parse(b"/*\n!/big/\n");
        assert!(patterns.includes(b"a.txt"));
        assert!(patterns.includes(b"small/a.txt"));
        assert!(!patterns.includes(b"big/a.txt"));
    }
}
