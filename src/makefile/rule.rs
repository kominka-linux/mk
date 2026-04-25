use crate::error::Loc;

/// A single prerequisite, either normal or order-only (after `|`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Prereq {
    pub name: String,
    /// True when the prereq appeared after a `|` separator.
    pub order_only: bool,
}

impl Prereq {
    pub fn normal(name: impl Into<String>) -> Self {
        Self { name: name.into(), order_only: false }
    }
    pub fn order_only(name: impl Into<String>) -> Self {
        Self { name: name.into(), order_only: true }
    }
}

/// A single line of a recipe.
#[derive(Debug, Clone)]
pub struct RecipeLine {
    pub text: String,
    /// `@` prefix — do not print the command before running it.
    pub silent: bool,
    /// `-` prefix — continue even if the command exits non-zero.
    pub ignore_error: bool,
    /// `+` prefix — run even in dry-run (`-n`) mode.
    pub always_run: bool,
}

impl RecipeLine {
    pub fn new(raw: &str) -> Self {
        let mut text = raw;
        let mut silent = false;
        let mut ignore_error = false;
        let mut always_run = false;

        loop {
            match text.as_bytes().first() {
                Some(b'@') => { silent = true; text = &text[1..]; }
                Some(b'-') => { ignore_error = true; text = &text[1..]; }
                Some(b'+') => { always_run = true; text = &text[1..]; }
                _ => break,
            }
        }

        Self { text: text.to_string(), silent, ignore_error, always_run }
    }
}

/// A parsed explicit rule (single-colon or double-colon).
#[derive(Debug, Clone)]
pub struct Rule {
    pub targets: Vec<String>,
    pub prereqs: Vec<Prereq>,
    pub recipe: Vec<RecipeLine>,
    /// True when the rule used `::` instead of `:`.
    pub double_colon: bool,
    pub loc: Loc,
}

impl Rule {
    pub fn new(targets: Vec<String>, prereqs: Vec<Prereq>, double_colon: bool, loc: Loc) -> Self {
        Self { targets, prereqs, recipe: Vec::new(), double_colon, loc }
    }
}

/// A pattern rule (implicit): `%.o: %.c` or the translated form of a suffix rule.
#[derive(Debug, Clone)]
pub struct PatternRule {
    /// Target pattern, e.g. `"%.o"`.
    pub target: String,
    /// Prerequisite patterns, e.g. `["%.c"]`.
    pub prereqs: Vec<String>,
    pub recipe: Vec<RecipeLine>,
    pub is_builtin: bool,
    pub loc: Loc,
}

/// Match a target name against a pattern containing exactly one `%`.
/// Returns the stem (the part that `%` matched) on success.
pub fn match_pattern<'a>(target: &'a str, pattern: &str) -> Option<&'a str> {
    match pattern.find('%') {
        None => {
            if target == pattern { Some("") } else { None }
        }
        Some(pct) => {
            let prefix = &pattern[..pct];
            let suffix = &pattern[pct + 1..];
            if target.len() < prefix.len() + suffix.len() {
                return None;
            }
            if !target.starts_with(prefix) {
                return None;
            }
            let after_prefix = &target[prefix.len()..];
            if !after_prefix.ends_with(suffix) {
                return None;
            }
            let stem_end = after_prefix.len() - suffix.len();
            Some(&after_prefix[..stem_end])
        }
    }
}

/// Substitute the stem into a pattern (replace `%` with `stem`).
pub fn apply_pattern(pattern: &str, stem: &str) -> String {
    match pattern.find('%') {
        None => pattern.to_string(),
        Some(pct) => {
            let mut out = pattern[..pct].to_string();
            out.push_str(stem);
            out.push_str(&pattern[pct + 1..]);
            out
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recipe_line_modifiers() {
        let r = RecipeLine::new("@-+echo hi");
        assert!(r.silent);
        assert!(r.ignore_error);
        assert!(r.always_run);
        assert_eq!(r.text, "echo hi");
    }

    #[test]
    fn recipe_line_plain() {
        let r = RecipeLine::new("gcc -c foo.c");
        assert!(!r.silent);
        assert!(!r.ignore_error);
        assert!(!r.always_run);
        assert_eq!(r.text, "gcc -c foo.c");
    }

    #[test]
    fn match_pattern_basic() {
        assert_eq!(match_pattern("foo.o", "%.o"), Some("foo"));
        assert_eq!(match_pattern("foo.o", "%.c"), None);
        assert_eq!(match_pattern("foo.o", "foo.o"), Some(""));
    }

    #[test]
    fn match_pattern_prefix() {
        assert_eq!(match_pattern("src/foo.o", "src/%.o"), Some("foo"));
    }

    #[test]
    fn match_pattern_no_percent() {
        assert_eq!(match_pattern("exact", "exact"), Some(""));
        assert_eq!(match_pattern("other", "exact"), None);
    }

    #[test]
    fn apply_pattern_basic() {
        assert_eq!(apply_pattern("%.o", "foo"), "foo.o");
        assert_eq!(apply_pattern("src/%.c", "bar"), "src/bar.c");
    }

    #[test]
    fn apply_pattern_no_percent() {
        assert_eq!(apply_pattern("fixed", "stem"), "fixed");
    }
}
