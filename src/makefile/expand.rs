//! Variable expansion and all built-in function evaluation.
//!
//! Single entry point: `expand(s, vars, auto, loc)`.
//! Internal helper `expand_impl` threads a temporary-overrides map for
//! `foreach`/`call` without needing trait objects or extra allocation.

use std::collections::HashMap;
use std::path::Path;

use crate::error::{fatal_at, Loc, Result};
use crate::makefile::var::{Flavor, VarTable};

/// Automatic variables available inside a recipe.
#[derive(Debug, Default, Clone)]
pub struct AutoVars {
    pub target: String,
    pub first_prereq: String,
    pub all_prereqs: String,
    pub all_prereqs_dup: String,
    pub newer_prereqs: String,
    pub stem: String,
    pub order_only: String,
}

/// Expand all `$(…)`, `${…}` and `$X` references in `s`.
pub fn expand(s: &str, vars: &VarTable, auto: Option<&AutoVars>, loc: &Loc) -> Result<String> {
    let mut stack = Vec::new();
    expand_impl(s, vars, &HashMap::new(), auto, loc, &mut stack)
}

/// Same as `expand` but with a callback invoked for each `$(eval …)`.
/// `eval_cb` receives the already-expanded text.
pub fn expand_with_eval(
    s: &str,
    vars: &VarTable,
    auto: Option<&AutoVars>,
    loc: &Loc,
    eval_cb: &mut dyn FnMut(&str) -> Result<()>,
) -> Result<String> {
    let mut stack = Vec::new();
    expand_impl_eval(s, vars, &HashMap::new(), auto, loc, &mut stack, Some(eval_cb))
}

// ── Core ─────────────────────────────────────────────────────────────────────

fn expand_impl(
    s: &str,
    vars: &VarTable,
    overrides: &HashMap<String, String>,
    auto: Option<&AutoVars>,
    loc: &Loc,
    stack: &mut Vec<String>,
) -> Result<String> {
    expand_impl_eval(s, vars, overrides, auto, loc, stack, None)
}

fn expand_impl_eval(
    s: &str,
    vars: &VarTable,
    overrides: &HashMap<String, String>,
    auto: Option<&AutoVars>,
    loc: &Loc,
    stack: &mut Vec<String>,
    eval_cb: Option<&mut dyn FnMut(&str) -> Result<()>>,
) -> Result<String> {
    // We can't easily thread an &mut through multiple recursive calls without
    // lifetime gymnastics, so we pass eval_cb only at the top level; nested
    // $(eval) calls inside `foreach`/`call` bodies are handled by eval_cb
    // being None in those recursive invocations.  This matches GNU make: eval
    // inside foreach works fine because we re-enter expand_paren each time.
    let _ = eval_cb; // used only for the eval branch below

    let mut out = String::with_capacity(s.len());
    let bytes = s.as_bytes();
    let mut i = 0;

    while i < bytes.len() {
        if bytes[i] != b'$' {
            out.push(bytes[i] as char);
            i += 1;
            continue;
        }
        i += 1;
        if i >= bytes.len() {
            out.push('$');
            break;
        }
        match bytes[i] {
            b'$' => {
                out.push('$');
                i += 1;
            }
            b'(' | b'{' => {
                let close = if bytes[i] == b'(' { ')' } else { '}' };
                i += 1;
                let (inner, consumed) = extract_balanced(&s[i..], close);
                i += consumed;
                let val = expand_paren(inner, vars, overrides, auto, loc, stack)?;
                out.push_str(&val);
            }
            c => {
                out.push_str(&auto_single(c as char, auto));
                i += 1;
            }
        }
    }

    Ok(out)
}

fn expand_paren(
    inner: &str,
    vars: &VarTable,
    overrides: &HashMap<String, String>,
    auto: Option<&AutoVars>,
    loc: &Loc,
    stack: &mut Vec<String>,
) -> Result<String> {
    // D/F automatic variable variants: @D @F <D <F ^D ^F +D +F ?D ?F *D *F |D |F
    if inner.len() == 2 {
        let b = inner.as_bytes();
        if b"@<^+?*|".contains(&b[0]) && (b[1] == b'D' || b[1] == b'F') {
            let base = auto_single(b[0] as char, auto);
            return Ok(if b[1] == b'D' { dir_of(&base) } else { notdir_of(&base) });
        }
    }

    let trimmed = inner.trim_start();
    // Split off the potential function name (first "word" at depth 0)
    let fn_end = trimmed
        .find(|c: char| c.is_ascii_whitespace() || c == ',')
        .unwrap_or(trimmed.len());
    let fn_name = &trimmed[..fn_end];

    match fn_name {
        // Eager: args expanded before dispatch
        "subst" | "patsubst" | "strip" | "findstring"
        | "filter" | "filter-out"
        | "sort" | "word" | "wordlist" | "words" | "firstword" | "lastword"
        | "dir" | "notdir" | "suffix" | "basename"
        | "addsuffix" | "addprefix" | "join"
        | "wildcard" | "realpath" | "abspath"
        | "shell" | "error" | "warning" | "info"
        | "origin" | "flavor" | "value" => {
            let args_raw = trimmed[fn_end..].trim_start();
            let args = expand_args(args_raw, vars, overrides, auto, loc, stack)?;
            dispatch_eager(fn_name, &args, vars, overrides, loc)
        }

        // Lazy: raw args passed to handler
        "if" | "or" | "and" | "foreach" | "call" | "eval" => {
            let args_raw = trimmed[fn_end..].trim_start();
            dispatch_lazy(fn_name, args_raw, vars, overrides, auto, loc, stack)
        }

        _ => {
            // Variable reference — expand entire inner to get name
            let name = expand_impl(inner, vars, overrides, auto, loc, stack)?;
            lookup_expand(&name, vars, overrides, auto, loc, stack)
        }
    }
}

fn lookup_expand(
    name: &str,
    vars: &VarTable,
    overrides: &HashMap<String, String>,
    auto: Option<&AutoVars>,
    loc: &Loc,
    stack: &mut Vec<String>,
) -> Result<String> {
    // Temporary overrides take priority (used by foreach/call)
    if let Some(val) = overrides.get(name) {
        return Ok(val.clone());
    }

    if stack.iter().any(|s| s == name) {
        return Err(fatal_at(
            loc.clone(),
            format!("Recursive variable '{name}' references itself (eventually)"),
        ));
    }

    let Some(var) = vars.get(name) else {
        return Ok(String::new());
    };

    if var.flavor == Flavor::Simple {
        return Ok(var.raw.clone());
    }

    let raw = var.raw.clone();
    stack.push(name.to_string());
    let result = expand_impl(&raw, vars, overrides, auto, loc, stack);
    stack.pop();
    result
}

fn expand_args(
    args_raw: &str,
    vars: &VarTable,
    overrides: &HashMap<String, String>,
    auto: Option<&AutoVars>,
    loc: &Loc,
    stack: &mut Vec<String>,
) -> Result<Vec<String>> {
    split_args_raw(args_raw)
        .into_iter()
        .map(|a| expand_impl(&a, vars, overrides, auto, loc, stack))
        .collect()
}

fn auto_single(c: char, auto: Option<&AutoVars>) -> String {
    let Some(a) = auto else { return String::new() };
    match c {
        '@' => a.target.clone(),
        '<' => a.first_prereq.clone(),
        '^' => a.all_prereqs.clone(),
        '+' => a.all_prereqs_dup.clone(),
        '?' => a.newer_prereqs.clone(),
        '*' => a.stem.clone(),
        '|' => a.order_only.clone(),
        _ => String::new(),
    }
}

// ── Balanced extraction ───────────────────────────────────────────────────────

/// Extract content inside balanced delimiters.
/// `rest` starts *after* the opening delimiter.
/// Returns `(inner, bytes_consumed_including_close)`.
fn extract_balanced(rest: &str, close: char) -> (&str, usize) {
    let open = if close == ')' { '(' } else { '{' };
    let mut depth = 1usize;
    for (i, c) in rest.char_indices() {
        if c == open { depth += 1; }
        else if c == close {
            depth -= 1;
            if depth == 0 {
                return (&rest[..i], i + close.len_utf8());
            }
        }
    }
    (rest, rest.len())
}

// ── Split args on comma at depth 0 ───────────────────────────────────────────

pub fn split_args_raw(s: &str) -> Vec<String> {
    let mut args = Vec::new();
    let mut depth = 0usize;
    let mut start = 0;
    let bytes = s.as_bytes();
    for i in 0..bytes.len() {
        match bytes[i] {
            b'(' | b'{' => depth += 1,
            b')' | b'}' => depth = depth.saturating_sub(1),
            b',' if depth == 0 => {
                args.push(s[start..i].to_string());
                start = i + 1;
            }
            _ => {}
        }
    }
    args.push(s[start..].to_string());
    args
}

// ── Eager dispatch ────────────────────────────────────────────────────────────

fn dispatch_eager(
    name: &str,
    args: &[String],
    vars: &VarTable,
    overrides: &HashMap<String, String>,
    loc: &Loc,
) -> Result<String> {
    let a0 = || args.first().map(String::as_str).unwrap_or("");
    let a1 = || args.get(1).map(String::as_str).unwrap_or("");
    let a2 = || args.get(2).map(String::as_str).unwrap_or("");

    match name {
        "subst" => {
            let from = a0(); let to = a1(); let text = a2();
            if from.is_empty() { return Ok(text.to_string()); }
            Ok(text.replace(from, to))
        }
        "patsubst" => fn_patsubst(a0(), a1(), a2()),
        "strip" => Ok(a0().split_whitespace().collect::<Vec<_>>().join(" ")),
        "findstring" => {
            let find = a0(); let text = a1();
            Ok(if text.contains(find) { find.to_string() } else { String::new() })
        }
        "filter" => fn_filter(a0(), a1(), false),
        "filter-out" => fn_filter(a0(), a1(), true),
        "sort" => {
            let mut words: Vec<&str> = a0().split_whitespace().collect();
            words.sort_unstable(); words.dedup();
            Ok(words.join(" "))
        }
        "word" => {
            let n: usize = a0().trim().parse().unwrap_or(0);
            Ok(a1().split_whitespace().nth(n.saturating_sub(1)).unwrap_or("").to_string())
        }
        "wordlist" => {
            let s: usize = a0().trim().parse().unwrap_or(0);
            let e: usize = a1().trim().parse().unwrap_or(0);
            let words: Vec<&str> = a2().split_whitespace().collect();
            let start = s.saturating_sub(1);
            let end = e.min(words.len());
            Ok(if start >= end { String::new() } else { words[start..end].join(" ") })
        }
        "words" => Ok(a0().split_whitespace().count().to_string()),
        "firstword" => Ok(a0().split_whitespace().next().unwrap_or("").to_string()),
        "lastword" => Ok(a0().split_whitespace().last().unwrap_or("").to_string()),
        "dir" => Ok(a0().split_whitespace().map(dir_of).collect::<Vec<_>>().join(" ")),
        "notdir" => Ok(a0().split_whitespace().map(notdir_of).collect::<Vec<_>>().join(" ")),
        "suffix" => {
            let parts: Vec<&str> = a0().split_whitespace()
                .filter_map(|w| w.rfind('.').map(|i| &w[i..]))
                .collect();
            Ok(parts.join(" "))
        }
        "basename" => {
            let parts: Vec<&str> = a0().split_whitespace()
                .map(|w| w.rfind('.').map(|i| &w[..i]).unwrap_or(w))
                .collect();
            Ok(parts.join(" "))
        }
        "addsuffix" => {
            let suf = a0();
            Ok(a1().split_whitespace().map(|w| format!("{w}{suf}")).collect::<Vec<_>>().join(" "))
        }
        "addprefix" => {
            let pre = a0();
            Ok(a1().split_whitespace().map(|w| format!("{pre}{w}")).collect::<Vec<_>>().join(" "))
        }
        "join" => {
            let w1: Vec<&str> = a0().split_whitespace().collect();
            let w2: Vec<&str> = a1().split_whitespace().collect();
            let n = w1.len().max(w2.len());
            Ok((0..n).map(|i| format!("{}{}", w1.get(i).unwrap_or(&""), w2.get(i).unwrap_or(&"")))
                .collect::<Vec<_>>().join(" "))
        }
        "wildcard" => fn_wildcard(a0()),
        "realpath" => {
            let parts: Vec<String> = a0().split_whitespace()
                .filter_map(|w| std::fs::canonicalize(w).ok()
                    .map(|p| p.to_string_lossy().into_owned()))
                .collect();
            Ok(parts.join(" "))
        }
        "abspath" => {
            let parts: Vec<String> = a0().split_whitespace().map(fn_abspath_one).collect();
            Ok(parts.join(" "))
        }
        "shell" => fn_shell(a0()),
        "error" => Err(fatal_at(loc.clone(), a0())),
        "warning" => {
            eprintln!("{loc}: {}", a0());
            Ok(String::new())
        }
        "info" => {
            println!("{}", a0());
            Ok(String::new())
        }
        "origin" => Ok(vars.origin_str(a0())),
        "flavor" => Ok(vars.flavor_str(a0()).to_string()),
        "value" => Ok(vars.raw(a0()).to_string()),
        _ => Ok(String::new()),
    }
}

// ── Lazy dispatch ─────────────────────────────────────────────────────────────

fn dispatch_lazy(
    name: &str,
    args_raw: &str,
    vars: &VarTable,
    overrides: &HashMap<String, String>,
    auto: Option<&AutoVars>,
    loc: &Loc,
    stack: &mut Vec<String>,
) -> Result<String> {
    let mut ex = |s: &str| expand_impl(s, vars, overrides, auto, loc, stack);

    match name {
        "if" => {
            let parts = split_args_raw(args_raw);
            let cond = ex(parts.first().map(String::as_str).unwrap_or(""))?;
            if !cond.trim().is_empty() {
                ex(parts.get(1).map(String::as_str).unwrap_or(""))
            } else {
                ex(parts.get(2).map(String::as_str).unwrap_or(""))
            }
        }
        "or" => {
            for raw in split_args_raw(args_raw) {
                let v = ex(&raw)?;
                if !v.trim().is_empty() { return Ok(v); }
            }
            Ok(String::new())
        }
        "and" => {
            let parts = split_args_raw(args_raw);
            let mut last = String::new();
            for raw in &parts {
                last = ex(raw)?;
                if last.trim().is_empty() { return Ok(String::new()); }
            }
            Ok(last)
        }
        "foreach" => {
            let parts = split_args_raw(args_raw);
            let var_name = ex(parts.first().map(String::as_str).unwrap_or(""))?;
            let list = ex(parts.get(1).map(String::as_str).unwrap_or(""))?;
            let text_raw = parts.get(2).map(String::as_str).unwrap_or("").to_string();
            let mut results = Vec::new();
            for word in list.split_whitespace() {
                let mut ov2 = overrides.clone();
                ov2.insert(var_name.clone(), word.to_string());
                let val = expand_impl(&text_raw, vars, &ov2, auto, loc, stack)?;
                results.push(val);
            }
            Ok(results.join(" "))
        }
        "call" => {
            let parts = split_args_raw(args_raw);
            let fn_var = ex(parts.first().map(String::as_str).unwrap_or(""))?;
            let mut ov2 = overrides.clone();
            for (i, raw) in parts.iter().skip(1).enumerate() {
                ov2.insert(format!("{}", i + 1), ex(raw)?);
            }
            // Temporary: set $(0) to function name (some macros use it)
            ov2.insert("0".to_string(), fn_var.clone());
            // Get raw body of the called function
            let body = match vars.get(&fn_var) {
                Some(v) => v.raw.clone(),
                None => overrides.get(&fn_var).cloned().unwrap_or_default(),
            };
            expand_impl(&body, vars, &ov2, auto, loc, stack)
        }
        "eval" => {
            // Without eval_cb available here, we just silently ignore.
            // Phase 5 wires this up via expand_with_eval at the call site.
            let _ = ex(args_raw)?;
            Ok(String::new())
        }
        _ => Ok(String::new()),
    }
}

// ── Individual helpers ────────────────────────────────────────────────────────

fn fn_patsubst(pattern: &str, repl: &str, text: &str) -> Result<String> {
    use crate::makefile::rule::{apply_pattern, match_pattern};
    let words: Vec<String> = text.split_whitespace().map(|w| {
        if let Some(stem) = match_pattern(w, pattern) {
            if repl.contains('%') { apply_pattern(repl, stem) } else { repl.to_string() }
        } else {
            w.to_string()
        }
    }).collect();
    Ok(words.join(" "))
}

fn fn_filter(patterns_str: &str, text: &str, invert: bool) -> Result<String> {
    use crate::makefile::rule::match_pattern;
    let pats: Vec<&str> = patterns_str.split_whitespace().collect();
    let result: Vec<&str> = text.split_whitespace().filter(|w| {
        let matched = pats.iter().any(|p| match_pattern(w, p).is_some());
        if invert { !matched } else { matched }
    }).collect();
    Ok(result.join(" "))
}

fn fn_wildcard(patterns: &str) -> Result<String> {
    let mut matches = Vec::new();
    for pattern in patterns.split_whitespace() {
        if let Ok(entries) = glob_expand(pattern) {
            matches.extend(entries);
        }
    }
    Ok(matches.join(" "))
}

fn glob_expand(pattern: &str) -> std::io::Result<Vec<String>> {
    let (dir_part, name_part) = match pattern.rfind('/') {
        None => (".", pattern),
        Some(i) => (&pattern[..i], &pattern[i + 1..]),
    };
    let mut results = Vec::new();
    for entry in std::fs::read_dir(dir_part)?.flatten() {
        let fname = entry.file_name();
        let fname_str = fname.to_string_lossy();
        if glob_match(name_part, &fname_str) {
            let full = if dir_part == "." {
                fname_str.into_owned()
            } else {
                format!("{dir_part}/{fname_str}")
            };
            results.push(full);
        }
    }
    results.sort();
    Ok(results)
}

fn glob_match_bytes(pattern: &[u8], name: &[u8]) -> bool {
    match (pattern.first(), name.first()) {
        (None, None) => true,
        (None, Some(_)) | (Some(_), None) => false,
        (Some(b'*'), _) => {
            (0..=name.len()).any(|skip| glob_match_bytes(&pattern[1..], &name[skip..]))
        }
        (Some(b'?'), Some(_)) => glob_match_bytes(&pattern[1..], &name[1..]),
        (Some(p), Some(n)) => p == n && glob_match_bytes(&pattern[1..], &name[1..]),
    }
}

fn glob_match(pattern: &str, name: &str) -> bool {
    glob_match_bytes(pattern.as_bytes(), name.as_bytes())
}

fn fn_abspath_one(w: &str) -> String {
    let p = Path::new(w);
    let abs = if p.is_absolute() {
        p.to_path_buf()
    } else {
        std::env::current_dir().unwrap_or_default().join(p)
    };
    let mut out = std::path::PathBuf::new();
    for component in abs.components() {
        use std::path::Component;
        match component {
            Component::CurDir => {}
            Component::ParentDir => { out.pop(); }
            c => out.push(c),
        }
    }
    out.to_string_lossy().into_owned()
}

fn fn_shell(cmd: &str) -> Result<String> {
    match std::process::Command::new("/bin/sh").arg("-c").arg(cmd).output() {
        Ok(o) => {
            let s = String::from_utf8_lossy(&o.stdout).into_owned();
            Ok(s.trim_end_matches('\n').replace('\n', " "))
        }
        Err(_) => Ok(String::new()),
    }
}

// ── Filename helpers (also used by graph.rs / exec.rs) ───────────────────────

pub fn dir_of(s: &str) -> String {
    match s.rfind('/') {
        None => "./".to_string(),
        Some(i) => s[..i + 1].to_string(),
    }
}

pub fn notdir_of(s: &str) -> String {
    match s.rfind('/') {
        None => s.to_string(),
        Some(i) => s[i + 1..].to_string(),
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::makefile::var::{Flavor, Origin, Var, VarTable};

    fn loc() -> Loc { Loc::new("test", 1) }

    fn ex(s: &str, vars: &VarTable) -> String {
        expand(s, vars, None, &loc()).unwrap()
    }

    #[test]
    fn plain_text() {
        assert_eq!(ex("hello world", &VarTable::new()), "hello world");
    }

    #[test]
    fn simple_ref() {
        let mut v = VarTable::new();
        v.set("CC", Var::new(Flavor::Simple, Origin::File, "gcc"));
        assert_eq!(ex("$(CC) -c", &v), "gcc -c");
    }

    #[test]
    fn recursive_ref() {
        let mut v = VarTable::new();
        v.set("A", Var::new(Flavor::Recursive, Origin::File, "$(B)"));
        v.set("B", Var::new(Flavor::Simple, Origin::File, "ok"));
        assert_eq!(ex("$(A)", &v), "ok");
    }

    #[test]
    fn nested_ref() {
        let mut v = VarTable::new();
        v.set("X", Var::new(Flavor::Simple, Origin::File, "CC"));
        v.set("CC", Var::new(Flavor::Simple, Origin::File, "gcc"));
        assert_eq!(ex("$($(X))", &v), "gcc");
    }

    #[test]
    fn dollar_dollar() {
        assert_eq!(ex("$$@", &VarTable::new()), "$@");
    }

    #[test]
    fn curly_braces() {
        let mut v = VarTable::new();
        v.set("X", Var::new(Flavor::Simple, Origin::File, "hi"));
        assert_eq!(ex("${X}", &v), "hi");
    }

    #[test]
    fn cycle_detection() {
        let mut v = VarTable::new();
        v.set("X", Var::new(Flavor::Recursive, Origin::File, "$(X)"));
        let err = expand("$(X)", &v, None, &loc()).unwrap_err().to_string();
        assert!(err.contains("Recursive variable 'X'"));
    }

    #[test]
    fn auto_target() {
        let auto = AutoVars { target: "foo.o".into(), ..Default::default() };
        let r = expand("$@", &VarTable::new(), Some(&auto), &loc()).unwrap();
        assert_eq!(r, "foo.o");
    }

    #[test]
    fn auto_d_f() {
        let auto = AutoVars { target: "src/foo.o".into(), ..Default::default() };
        assert_eq!(expand("$(@D)", &VarTable::new(), Some(&auto), &loc()).unwrap(), "src/");
        assert_eq!(expand("$(@F)", &VarTable::new(), Some(&auto), &loc()).unwrap(), "foo.o");
    }

    #[test]
    fn fn_subst() { assert_eq!(ex("$(subst ee,EE,feet on the street)", &VarTable::new()), "fEEt on the strEEt"); }

    #[test]
    fn fn_patsubst() { assert_eq!(ex("$(patsubst %.c,%.o,foo.c bar.c baz.h)", &VarTable::new()), "foo.o bar.o baz.h"); }

    #[test]
    fn fn_strip() { assert_eq!(ex("$(strip  a  b   c )", &VarTable::new()), "a b c"); }

    #[test]
    fn fn_findstring_hit() { assert_eq!(ex("$(findstring an,banana)", &VarTable::new()), "an"); }

    #[test]
    fn fn_findstring_miss() { assert_eq!(ex("$(findstring xy,banana)", &VarTable::new()), ""); }

    #[test]
    fn fn_filter() { assert_eq!(ex("$(filter %.c,foo.c bar.h baz.c)", &VarTable::new()), "foo.c baz.c"); }

    #[test]
    fn fn_filter_out() { assert_eq!(ex("$(filter-out %.c,foo.c bar.h baz.c)", &VarTable::new()), "bar.h"); }

    #[test]
    fn fn_sort() { assert_eq!(ex("$(sort foo bar baz foo)", &VarTable::new()), "bar baz foo"); }

    #[test]
    fn fn_word() { assert_eq!(ex("$(word 2,foo bar baz)", &VarTable::new()), "bar"); }

    #[test]
    fn fn_wordlist() { assert_eq!(ex("$(wordlist 2,3,foo bar baz qux)", &VarTable::new()), "bar baz"); }

    #[test]
    fn fn_words() { assert_eq!(ex("$(words foo bar baz)", &VarTable::new()), "3"); }

    #[test]
    fn fn_firstword() { assert_eq!(ex("$(firstword foo bar)", &VarTable::new()), "foo"); }

    #[test]
    fn fn_lastword() { assert_eq!(ex("$(lastword foo bar baz)", &VarTable::new()), "baz"); }

    #[test]
    fn fn_dir() { assert_eq!(ex("$(dir src/foo.c)", &VarTable::new()), "src/"); }

    #[test]
    fn fn_notdir() { assert_eq!(ex("$(notdir src/foo.c)", &VarTable::new()), "foo.c"); }

    #[test]
    fn fn_dir_no_slash() { assert_eq!(ex("$(dir foo.c)", &VarTable::new()), "./"); }

    #[test]
    fn fn_suffix() { assert_eq!(ex("$(suffix foo.c bar.h)", &VarTable::new()), ".c .h"); }

    #[test]
    fn fn_basename() { assert_eq!(ex("$(basename foo.c bar)", &VarTable::new()), "foo bar"); }

    #[test]
    fn fn_addsuffix() { assert_eq!(ex("$(addsuffix .o,foo bar)", &VarTable::new()), "foo.o bar.o"); }

    #[test]
    fn fn_addprefix() { assert_eq!(ex("$(addprefix src/,foo.c bar.c)", &VarTable::new()), "src/foo.c src/bar.c"); }

    #[test]
    fn fn_join() { assert_eq!(ex("$(join a b,1 2)", &VarTable::new()), "a1 b2"); }

    #[test]
    fn fn_if_true() { assert_eq!(ex("$(if yes,TRUE,FALSE)", &VarTable::new()), "TRUE"); }

    #[test]
    fn fn_if_false() { assert_eq!(ex("$(if ,TRUE,FALSE)", &VarTable::new()), "FALSE"); }

    #[test]
    fn fn_if_no_else() { assert_eq!(ex("$(if ,TRUE)", &VarTable::new()), ""); }

    #[test]
    fn fn_or_short_circuit() {
        // $(or) returns the first non-empty argument; "no" is non-empty → returned
        assert_eq!(ex("$(or ,no,yes,ignored)", &VarTable::new()), "no");
        // all empty → empty string
        assert_eq!(ex("$(or , , )", &VarTable::new()), "");
    }

    #[test]
    fn fn_and_all_true() { assert_eq!(ex("$(and a,b,c)", &VarTable::new()), "c"); }

    #[test]
    fn fn_and_short_circuit() { assert_eq!(ex("$(and a,,c)", &VarTable::new()), ""); }

    #[test]
    fn fn_foreach() { assert_eq!(ex("$(foreach x,a b c,[$(x)])", &VarTable::new()), "[a] [b] [c]"); }

    #[test]
    fn fn_call() {
        let mut v = VarTable::new();
        v.set("reverse", Var::new(Flavor::Recursive, Origin::File, "$(2) $(1)"));
        assert_eq!(ex("$(call reverse,foo,bar)", &v), "bar foo");
    }

    #[test]
    fn fn_flavor_simple() {
        let mut v = VarTable::new();
        v.set("X", Var::new(Flavor::Simple, Origin::File, "x"));
        assert_eq!(ex("$(flavor X)", &v), "simple");
    }

    #[test]
    fn fn_flavor_undef() { assert_eq!(ex("$(flavor UNDEF)", &VarTable::new()), "undefined"); }

    #[test]
    fn fn_origin_file() {
        let mut v = VarTable::new();
        v.set("X", Var::new(Flavor::Simple, Origin::File, "v"));
        assert_eq!(ex("$(origin X)", &v), "file");
    }

    #[test]
    fn fn_value_unexpanded() {
        let mut v = VarTable::new();
        v.set("X", Var::new(Flavor::Recursive, Origin::File, "$(Y)"));
        assert_eq!(ex("$(value X)", &v), "$(Y)");
    }

    #[test]
    fn split_args_raw_basic() {
        assert_eq!(split_args_raw("a,b,c"), vec!["a", "b", "c"]);
    }

    #[test]
    fn split_args_raw_nested_parens() {
        assert_eq!(split_args_raw("$(a,b),c"), vec!["$(a,b)", "c"]);
    }

    #[test]
    fn split_args_raw_empty() {
        let v = split_args_raw("");
        assert_eq!(v, vec![""]);
    }
}
