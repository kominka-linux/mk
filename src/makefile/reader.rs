/// A logical line: one or more physical lines joined by backslash continuation.
/// The `line` field is the 1-based number of the *first* physical line.
#[derive(Debug, Clone)]
pub struct LogicalLine {
    pub text: String,
    pub line: usize,
}

/// Iterate over the logical lines of a makefile source string.
pub struct Reader<'a> {
    src: &'a str,
    pos: usize,
    line: usize,
}

impl<'a> Reader<'a> {
    pub fn new(src: &'a str) -> Self {
        Self { src, pos: 0, line: 1 }
    }
}

impl Iterator for Reader<'_> {
    type Item = LogicalLine;

    fn next(&mut self) -> Option<LogicalLine> {
        if self.pos >= self.src.len() {
            return None;
        }

        let start_line = self.line;
        let mut logical = String::new();

        loop {
            // Find the end of the current physical line
            let rest = &self.src[self.pos..];
            let (raw_line, consumed) = match rest.find('\n') {
                Some(i) => (&rest[..i], i + 1),
                None => (rest, rest.len()),
            };
            self.pos += consumed;
            self.line += 1;

            // Strip inline comment (# not preceded by \)
            let stripped = strip_comment(raw_line);

            if stripped.ends_with('\\') {
                // Continuation: drop the backslash, fold into one space
                let without_bs = stripped[..stripped.len() - 1].trim_end();
                logical.push_str(without_bs);
                logical.push(' ');
            } else {
                logical.push_str(stripped);
                break;
            }

            if self.pos >= self.src.len() {
                break;
            }
        }

        Some(LogicalLine { text: logical, line: start_line })
    }
}

/// Strip a `#` comment from a physical line, respecting `\#` (escaped hash).
fn strip_comment(s: &str) -> &str {
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'#' && (i == 0 || bytes[i - 1] != b'\\') {
            return &s[..i];
        }
        i += 1;
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lines(src: &str) -> Vec<String> {
        Reader::new(src).map(|l| l.text).collect()
    }

    #[test]
    fn simple_lines() {
        let src = "foo\nbar\nbaz\n";
        assert_eq!(lines(src), vec!["foo", "bar", "baz"]);
    }

    #[test]
    fn continuation() {
        let src = "foo \\\nbar\nbaz\n";
        assert_eq!(lines(src), vec!["foo bar", "baz"]);
    }

    #[test]
    fn three_way_continuation() {
        let src = "a \\\nb \\\nc\n";
        assert_eq!(lines(src), vec!["a b c"]);
    }

    #[test]
    fn inline_comment() {
        let src = "CC = gcc # the C compiler\n";
        assert_eq!(lines(src), vec!["CC = gcc "]);
    }

    #[test]
    fn escaped_hash_not_a_comment() {
        let src = "X = a\\#b\n";
        assert_eq!(lines(src), vec!["X = a\\#b"]);
    }

    #[test]
    fn no_trailing_newline() {
        let src = "foo";
        assert_eq!(lines(src), vec!["foo"]);
    }

    #[test]
    fn empty_source() {
        let src = "";
        assert!(lines(src).is_empty());
    }

    #[test]
    fn blank_lines_preserved() {
        let src = "a\n\nb\n";
        let got = lines(src);
        assert_eq!(got, vec!["a", "", "b"]);
    }

    #[test]
    fn line_numbers() {
        let src = "a\nb\\\nc\nd\n";
        let lls: Vec<_> = Reader::new(src).collect();
        assert_eq!(lls[0].line, 1); // "a"
        assert_eq!(lls[1].line, 2); // "b c" (joined)
        assert_eq!(lls[2].line, 4); // "d"
    }
}
