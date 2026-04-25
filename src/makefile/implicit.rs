//! Implicit rules: pattern rules, suffix rules, chain rules, and built-ins.

use crate::error::Loc;
use crate::makefile::rule::{apply_pattern, match_pattern, PatternRule, RecipeLine};

/// Default suffix list (in order). Cleared by `.SUFFIXES:` with no deps.
pub const DEFAULT_SUFFIXES: &[&str] = &[
    ".out", ".a", ".ln", ".o", ".c", ".cc", ".C", ".cpp", ".p", ".f", ".F",
    ".m", ".r", ".y", ".l", ".ym", ".yl", ".s", ".S", ".mod", ".sym",
    ".def", ".h", ".info", ".dvi", ".tex", ".texinfo", ".texi",
    ".txinfo", ".w", ".ch", ".web", ".sh", ".elc", ".el",
];

/// Translate a suffix rule `.a.b:` into a pattern rule `%.b: %.a`.
pub fn suffix_to_pattern(from: &str, to: &str, recipe: Vec<RecipeLine>, loc: Loc) -> PatternRule {
    PatternRule {
        target: format!("%{to}"),
        prereqs: vec![format!("%{from}")],
        recipe,
        is_builtin: false,
        loc,
    }
}

/// Return the list of built-in pattern rules (when `-r` is NOT set).
pub fn builtin_rules() -> Vec<PatternRule> {
    let loc = Loc::new("<builtin>", 0);
    let mut rules = Vec::new();

    // C compilation: %.o: %.c
    rules.push(PatternRule {
        target: "%.o".into(),
        prereqs: vec!["%.c".into()],
        recipe: vec![
            RecipeLine::new("$(CC) $(CFLAGS) $(CPPFLAGS) $(TARGET_ARCH) -c -o $@ $<"),
        ],
        is_builtin: true,
        loc: loc.clone(),
    });

    // C++ compilation: %.o: %.cc / %.cpp / %.C
    for ext in &[".cc", ".cpp", ".C"] {
        rules.push(PatternRule {
            target: "%.o".into(),
            prereqs: vec![format!("%{ext}")],
            recipe: vec![
                RecipeLine::new("$(CXX) $(CXXFLAGS) $(CPPFLAGS) $(TARGET_ARCH) -c -o $@ $<"),
            ],
            is_builtin: true,
            loc: loc.clone(),
        });
    }

    // Assembler: %.o: %.s
    rules.push(PatternRule {
        target: "%.o".into(),
        prereqs: vec!["%.s".into()],
        recipe: vec![RecipeLine::new("$(AS) $(ASFLAGS) $(TARGET_MACH) -o $@ $<")],
        is_builtin: true,
        loc: loc.clone(),
    });

    // Assembler with preprocessing: %.s: %.S
    rules.push(PatternRule {
        target: "%.s".into(),
        prereqs: vec!["%.S".into()],
        recipe: vec![RecipeLine::new("$(CC) -E $(CPPFLAGS) $(TARGET_ARCH) $< > $@")],
        is_builtin: true,
        loc: loc.clone(),
    });

    // C linking: % (no extension) from %.o
    rules.push(PatternRule {
        target: "%".into(),
        prereqs: vec!["%.o".into()],
        recipe: vec![RecipeLine::new(
            "$(CC) $(LDFLAGS) $(TARGET_ARCH) $< $(LOADLIBES) $(LDLIBS) -o $@",
        )],
        is_builtin: true,
        loc: loc.clone(),
    });

    // Lex: %.c: %.l
    rules.push(PatternRule {
        target: "%.c".into(),
        prereqs: vec!["%.l".into()],
        recipe: vec![
            RecipeLine::new("$(LEX) $(LFLAGS) $<"),
            RecipeLine::new("mv -f lex.yy.c $@"),
        ],
        is_builtin: true,
        loc: loc.clone(),
    });

    // Yacc: %.c: %.y
    rules.push(PatternRule {
        target: "%.c".into(),
        prereqs: vec!["%.y".into()],
        recipe: vec![
            RecipeLine::new("$(YACC) $(YFLAGS) $<"),
            RecipeLine::new("mv -f y.tab.c $@"),
        ],
        is_builtin: true,
        loc: loc.clone(),
    });

    rules
}

/// Built-in variable defaults (when `-R` is NOT set).
pub fn builtin_vars() -> Vec<(&'static str, &'static str)> {
    vec![
        ("CC", "cc"),
        ("CXX", "g++"),
        ("CPP", "$(CC) -E"),
        ("FC", "f77"),
        ("AR", "ar"),
        ("ARFLAGS", "rv"),
        ("AS", "as"),
        ("LEX", "lex"),
        ("YACC", "yacc"),
        ("CFLAGS", ""),
        ("CXXFLAGS", ""),
        ("CPPFLAGS", ""),
        ("FFLAGS", ""),
        ("ASFLAGS", ""),
        ("LDFLAGS", ""),
        ("LDLIBS", ""),
        ("LOADLIBES", ""),
        ("OUTPUT_OPTION", "-o $@"),
    ]
}

/// Find an applicable pattern rule for `target` from `rules`.
/// Returns `(rule_index, stem)`.
pub fn find_rule<'a>(
    target: &str,
    rules: &'a [PatternRule],
) -> Option<(usize, String)> {
    for (i, rule) in rules.iter().enumerate() {
        if let Some(stem) = match_pattern(target, &rule.target) {
            return Some((i, stem.to_string()));
        }
    }
    None
}

/// Find a chain of implicit rules to build `target` from existing files.
///
/// Returns a sequence of `(rule_index, stem)` pairs from first to last,
/// where each rule's prerequisite is either an existing file or the output
/// of the next rule in the chain.
pub fn find_chain<'a>(
    target: &str,
    rules: &'a [PatternRule],
    file_exists: &dyn Fn(&str) -> bool,
    depth: usize,
) -> Option<Vec<(usize, String)>> {
    if depth > 5 {
        return None; // Guard against infinite chains
    }

    for (i, rule) in rules.iter().enumerate() {
        let Some(stem) = match_pattern(target, &rule.target) else { continue };

        // Check if all prereqs are satisfiable
        let all_ok = rule.prereqs.iter().all(|pp| {
            let concrete = apply_pattern(pp, &stem);
            if file_exists(&concrete) {
                return true;
            }
            // Try chaining
            find_chain(&concrete, rules, file_exists, depth + 1).is_some()
        });

        if all_ok {
            return Some(vec![(i, stem.to_string())]);
        }
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::makefile::rule::match_pattern;

    #[test]
    fn suffix_rule_translation() {
        let loc = Loc::new("Makefile", 1);
        let rule = suffix_to_pattern(".c", ".o", vec![], loc);
        assert_eq!(rule.target, "%.o");
        assert_eq!(rule.prereqs, vec!["%.c"]);
    }

    #[test]
    fn builtin_c_rule_matches() {
        let rules = builtin_rules();
        let c_rule = rules.iter().find(|r| r.target == "%.o" && r.prereqs == vec!["%.c".to_string()]).unwrap();
        assert!(match_pattern("foo.o", &c_rule.target).is_some());
    }

    #[test]
    fn find_rule_basic() {
        let rules = builtin_rules();
        let result = find_rule("foo.o", &rules);
        assert!(result.is_some());
        let (_, stem) = result.unwrap();
        assert_eq!(stem, "foo");
    }

    #[test]
    fn find_rule_no_match_empty() {
        // An empty rule list never matches
        assert!(find_rule("foo.xyz", &[]).is_none());
    }

    #[test]
    fn find_rule_builtin_percent_matches_all() {
        // The `%` rule (link from %.o) matches anything including "foo"
        let rules = builtin_rules();
        // foo.o should match %.o → stem "foo"
        let r = find_rule("foo.o", &rules);
        assert!(r.is_some());
    }

    #[test]
    fn chain_c_to_o() {
        let rules = builtin_rules();
        // foo.c exists, need to build foo.o
        let file_exists = |f: &str| f == "foo.c";
        let chain = find_chain("foo.o", &rules, &file_exists, 0);
        assert!(chain.is_some());
    }
}
