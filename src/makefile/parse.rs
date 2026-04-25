//! Makefile parser: converts source text into the `Makefile` data structure.
//!
//! The parser is a line-oriented state machine that handles:
//!   - Variable assignments (all flavors, override, target-specific)
//!   - Rule headers (`:` and `::`, order-only `|`, archive members)
//!   - Recipe lines (tab-prefixed)
//!   - Directives: include/-include/sinclude, define/endef, export/unexport, undefine
//!   - Conditionals: ifeq/ifneq/ifdef/ifndef/else/endif (fully nested)

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::args::Args;
use crate::error::{fatal, fatal_at, Loc, Result};
use crate::makefile::expand::expand;
use crate::makefile::rule::{PatternRule, Prereq, RecipeLine, Rule};
use crate::makefile::var::{Flavor, Origin, Var};
use super::Makefile;

/// A conditional frame for ifeq/ifdef nesting.
#[derive(Debug)]
pub struct CondFrame {
    /// Are we executing this branch (vs. skipping it)?
    active: bool,
    /// Have we already seen an `else`?
    seen_else: bool,
    /// Was the initial condition true? (controls else-branch activation)
    was_true: bool,
}

/// State for accumulating a `define` block.
struct DefineBlock {
    name: String,
    flavor: Flavor,
    origin: Origin,
    lines: Vec<String>,
}

/// Tracks the most recently opened rule (which may still receive recipe lines).
struct RuleState {
    targets: Vec<String>,
    prereqs: Vec<Prereq>,
    double_colon: bool,
    loc: Loc,
    recipe: Vec<RecipeLine>,
    /// Static pattern rule pattern (for `targets: pattern: prereqs`).
    static_pattern: Option<String>,
    /// Target-specific variable: `(target, name, raw, flavor, origin)`.
    target_var: Option<(String, String, String, Flavor, Origin)>,
}

pub fn parse_source_pub(
    src: &str,
    file: &str,
    mf: &mut Makefile,
    args: &Args,
    cond_stack: &mut Vec<CondFrame>,
    include_stack: &mut Vec<PathBuf>,
) -> Result<()> {
    parse_source(src, file, mf, args, cond_stack, include_stack)
}

pub fn parse_file(path: &Path, args: &Args, env: &HashMap<String, String>) -> Result<Makefile> {
    let mut mf = Makefile::new();
    setup_initial_vars(&mut mf, args, env);
    mf.files_read.push(path.to_string_lossy().into_owned());

    let src = std::fs::read_to_string(path).map_err(|e| {
        fatal(format!("{}: {e}", path.display()))
    })?;

    let mut cond_stack: Vec<CondFrame> = Vec::new();
    let mut include_stack: Vec<PathBuf> = vec![path.to_path_buf()];
    parse_source(&src, &path.to_string_lossy(), &mut mf, args, &mut cond_stack, &mut include_stack)?;

    Ok(mf)
}

pub fn parse_str(src: &str, file: &str, mf: &mut Makefile, args: &Args) -> Result<()> {
    let mut cond_stack = Vec::new();
    let mut include_stack = Vec::new();
    parse_source(src, file, mf, args, &mut cond_stack, &mut include_stack)
}

fn parse_source(
    src: &str,
    file: &str,
    mf: &mut Makefile,
    args: &Args,
    cond_stack: &mut Vec<CondFrame>,
    include_stack: &mut Vec<PathBuf>,
) -> Result<()> {
    use crate::makefile::reader::Reader;

    let mut define: Option<DefineBlock> = None;
    let mut rule: Option<RuleState> = None;

    for ll in Reader::new(src) {
        let loc = Loc::new(file, ll.line);
        let line = &ll.text;

        // ── Inside a define block ────────────────────────────────────────────
        if let Some(ref mut def) = define {
            let trimmed = line.trim();
            if trimmed == "endef" || trimmed.starts_with("endef ") || trimmed.starts_with("endef\t") {
                let body = def.lines.join("\n");
                mf.vars.set(
                    &def.name,
                    Var { flavor: def.flavor, origin: def.origin, raw: body, exported: None },
                );
                define = None;
            } else {
                def.lines.push(line.clone());
            }
            continue;
        }

        // ── Recipe line (starts with TAB) ────────────────────────────────────
        if line.starts_with('\t') {
            if is_skipping(cond_stack) { continue; }
            if let Some(ref mut rs) = rule {
                rs.recipe.push(RecipeLine::new(&line[1..]));
            }
            // Recipe line with no active rule: silently ignore (like GNU make)
            continue;
        }

        // Non-recipe line always commits the current rule
        if let Some(rs) = rule.take() {
            commit_rule(rs, mf, &loc)?;
        }

        let trimmed = line.trim();
        if trimmed.is_empty() { continue; }

        // ── Conditional directives (checked even while skipping) ─────────────
        if let Some(kw) = leading_keyword(trimmed, &["ifeq", "ifneq", "ifdef", "ifndef", "else", "endif"]) {
            let rest = trimmed[kw.len()..].trim();
            match kw {
                "ifeq" | "ifneq" => {
                    let skipping = is_skipping(cond_stack);
                    let result = if skipping {
                        false
                    } else {
                        let (a, b) = parse_ifeq_args(rest, &mf.vars, &loc)?;
                        let eq = a.trim() == b.trim();
                        if kw == "ifeq" { eq } else { !eq }
                    };
                    cond_stack.push(CondFrame { active: result, seen_else: false, was_true: result });
                }
                "ifdef" | "ifndef" => {
                    let skipping = is_skipping(cond_stack);
                    let result = if skipping {
                        false
                    } else {
                        let name = expand(rest, &mf.vars, None, &loc)?;
                        let defined = !mf.vars.raw(name.trim()).is_empty();
                        if kw == "ifdef" { defined } else { !defined }
                    };
                    cond_stack.push(CondFrame { active: result, seen_else: false, was_true: result });
                }
                "else" => {
                    if cond_stack.last().map(|f| f.seen_else).unwrap_or(false) {
                        return Err(fatal_at(loc.clone(), "extraneous else"));
                    }
                    if cond_stack.is_empty() {
                        return Err(fatal_at(loc.clone(), "extraneous else"));
                    }
                    // Compute outer_active BEFORE taking mutable borrow of last element
                    let outer_active = cond_stack.len() < 2
                        || cond_stack[..cond_stack.len() - 1].iter().all(|f| f.active);
                    let frame_was_true = cond_stack.last().map(|f| f.was_true).unwrap_or(false);

                    if rest.starts_with("ifeq") || rest.starts_with("ifneq")
                        || rest.starts_with("ifdef") || rest.starts_with("ifndef")
                    {
                        // else ifeq / else ifdef form
                        if outer_active && !frame_was_true {
                            let sub_rest = rest.trim_start_matches(|c: char| c.is_alphabetic()).trim();
                            let new_active = if rest.starts_with("ifeq") || rest.starts_with("ifneq") {
                                let (a, b) = parse_ifeq_args(sub_rest, &mf.vars, &loc)?;
                                let eq = a.trim() == b.trim();
                                if rest.starts_with("ifeq") { eq } else { !eq }
                            } else {
                                let name = expand(sub_rest, &mf.vars, None, &loc)?;
                                let defined = !mf.vars.raw(name.trim()).is_empty();
                                if rest.starts_with("ifdef") { defined } else { !defined }
                            };
                            if let Some(frame) = cond_stack.last_mut() {
                                frame.active = new_active;
                                if new_active { frame.was_true = true; }
                            }
                        } else if outer_active && frame_was_true {
                            if let Some(frame) = cond_stack.last_mut() {
                                frame.active = false;
                            }
                        }
                        // Don't set seen_else for else ifeq (another else can follow)
                    } else {
                        if let Some(frame) = cond_stack.last_mut() {
                            if outer_active {
                                frame.active = !frame.was_true;
                            }
                            frame.seen_else = true;
                        }
                    }
                }
                "endif" => {
                    if cond_stack.is_empty() {
                        return Err(fatal_at(loc.clone(), "extraneous endif"));
                    }
                    cond_stack.pop();
                }
                _ => unreachable!(),
            }
            continue;
        }

        if is_skipping(cond_stack) { continue; }

        // ── export / unexport ────────────────────────────────────────────────
        if let Some(rest) = strip_directive(trimmed, "export") {
            if rest.is_empty() {
                // `export` alone: export all variables
                mf.vars.export_all = true;
            } else if let Some(name) = strip_directive(rest, "override") {
                handle_assignment(name.trim(), Origin::Override, &mut mf.vars, &loc, args)?;
            } else if contains_assign(rest) {
                handle_assignment(rest, Origin::File, &mut mf.vars, &loc, args)?;
                // mark exported
                if let Some(name) = extract_assign_name(rest) {
                    mf.vars.set_exported(&name, Some(true));
                }
            } else {
                let name = expand(rest.trim(), &mf.vars, None, &loc)?;
                for n in name.split_whitespace() {
                    mf.vars.set_exported(n, Some(true));
                }
            }
            continue;
        }

        if let Some(rest) = strip_directive(trimmed, "unexport") {
            let name = expand(rest.trim(), &mf.vars, None, &loc)?;
            for n in name.split_whitespace() {
                mf.vars.set_exported(n, Some(false));
            }
            continue;
        }

        // ── undefine ─────────────────────────────────────────────────────────
        if let Some(rest) = strip_directive(trimmed, "undefine") {
            let is_override = rest.trim_start().starts_with("override ");
            let name_raw = if is_override { rest.trim_start()[9..].trim() } else { rest.trim() };
            let name = expand(name_raw, &mf.vars, None, &loc)?;
            let origin = if is_override { Origin::Override } else { Origin::File };
            mf.vars.undefine(name.trim(), origin);
            continue;
        }

        // ── define / endef ───────────────────────────────────────────────────
        if let Some(rest) = strip_directive(trimmed, "define") {
            let (name, flavor) = parse_define_header(rest);
            let name = expand(&name, &mf.vars, None, &loc)?;
            let origin = Origin::File;
            define = Some(DefineBlock { name: name.trim().to_string(), flavor, origin, lines: Vec::new() });
            continue;
        }

        // ── include ──────────────────────────────────────────────────────────
        if let Some(rest) = strip_directive(trimmed, "include")
            .or_else(|| strip_directive(trimmed, "-include"))
            .or_else(|| strip_directive(trimmed, "sinclude"))
        {
            let silent = trimmed.starts_with('-') || trimmed.starts_with("sinclude");
            let paths_raw = expand(rest.trim(), &mf.vars, None, &loc)?;
            for path_str in paths_raw.split_whitespace() {
                let path = resolve_include(path_str, &mf.include_dirs);
                match std::fs::read_to_string(&path) {
                    Ok(src) => {
                        mf.files_read.push(path.to_string_lossy().into_owned());
                        let sub_file = path.to_string_lossy().into_owned();
                        if include_stack.iter().any(|p| p == &path) {
                            return Err(fatal_at(loc.clone(), format!("circular include of '{}'", path.display())));
                        }
                        include_stack.push(path.clone());
                        parse_source(&src, &sub_file, mf, args, cond_stack, include_stack)?;
                        include_stack.pop();
                    }
                    Err(e) if silent => {
                        // -include: silently skip missing files
                    }
                    Err(e) => {
                        return Err(fatal_at(loc.clone(), format!("{}: {e}", path.display())));
                    }
                }
            }
            continue;
        }

        // ── override assignment ──────────────────────────────────────────────
        if let Some(rest) = strip_directive(trimmed, "override") {
            handle_assignment(rest.trim(), Origin::Override, &mut mf.vars, &loc, args)?;
            continue;
        }

        // ── Special targets (.PHONY etc.) and rule headers ───────────────────
        // Try to classify as assignment first
        if let Some(()) = try_parse_assignment(trimmed, Origin::File, &mut mf.vars, &loc, args)? {
            continue;
        }

        // Must be a rule header (or special target)
        if let Some(rs) = try_parse_rule(trimmed, &mf.vars, &loc)? {
            // Handle special targets
            let is_special = process_special_targets(&rs, mf);
            if !is_special {
                rule = Some(rs);
            }
            continue;
        }

        // Unknown line — emit warning like GNU make would
        // (GNU make: "*** missing separator")
        // Check if first non-space char is a recipe-like char
        eprintln!("{}: warning: ignoring unknown line: {}", loc, trimmed.chars().take(40).collect::<String>());
    }

    // Commit any trailing rule
    let final_loc = Loc::new(file, 0);
    if let Some(rs) = rule.take() {
        commit_rule(rs, mf, &final_loc)?;
    }

    // Check for unclosed conditionals
    if !cond_stack.is_empty() {
        return Err(fatal_at(final_loc, "missing endif"));
    }

    Ok(())
}

// ── Assignment handling ───────────────────────────────────────────────────────

fn try_parse_assignment(
    line: &str,
    origin: Origin,
    vars: &mut crate::makefile::var::VarTable,
    loc: &Loc,
    args: &Args,
) -> Result<Option<()>> {
    if !contains_assign(line) { return Ok(None); }
    handle_assignment(line, origin, vars, loc, args)?;
    Ok(Some(()))
}

fn contains_assign(line: &str) -> bool {
    // Determine if this line is an assignment.
    // An assignment contains = (possibly preceded by +, ?, :) before any :
    // that isn't followed by = or :
    let bytes = line.as_bytes();
    let mut depth = 0usize;
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'(' | b'{' => depth += 1,
            b')' | b'}' => depth = depth.saturating_sub(1),
            b'=' if depth == 0 => return true,
            b':' if depth == 0 => {
                // Check what follows
                match bytes.get(i + 1) {
                    Some(b'=') => return true,  // :=
                    Some(b':') => return false, // :: (double-colon rule)
                    _ => return false,          // : (rule)
                }
            }
            _ => {}
        }
        i += 1;
    }
    false
}

fn extract_assign_name(line: &str) -> Option<String> {
    let bytes = line.as_bytes();
    let mut depth = 0usize;
    for (i, &b) in bytes.iter().enumerate() {
        match b {
            b'(' | b'{' => depth += 1,
            b')' | b'}' => depth = depth.saturating_sub(1),
            b'+' | b'?' | b':' if depth == 0 => {
                if bytes.get(i + 1) == Some(&b'=') {
                    return Some(line[..i].trim().to_string());
                }
            }
            b'=' if depth == 0 => {
                return Some(line[..i].trim().to_string());
            }
            _ => {}
        }
    }
    None
}

fn handle_assignment(
    line: &str,
    origin: Origin,
    vars: &mut crate::makefile::var::VarTable,
    loc: &Loc,
    args: &Args,
) -> Result<()> {
    let (name_raw, flavor, value_raw) = split_assignment(line)
        .ok_or_else(|| fatal_at(loc.clone(), format!("could not parse assignment: {line}")))?;

    let name = expand(name_raw.trim(), vars, None, loc)?;
    let name = name.trim().to_string();

    // Command-line overrides block file/override assignments
    if let Some(existing) = vars.get(&name) {
        if existing.origin == Origin::CommandLine && origin < Origin::CommandLine {
            // Unless this is an += to a command-line var (GNU make appends)
            if flavor != Flavor::Append {
                return Ok(());
            }
        }
    }

    let raw = match flavor {
        Flavor::Simple => expand(value_raw.trim_start(), vars, None, loc)?,
        Flavor::Append => {
            // Append: if the variable exists, add a space and the new value
            let existing_raw = vars.raw(&name).to_string();
            let existing_flavor = vars.get(&name).map(|v| v.flavor).unwrap_or(Flavor::Recursive);
            let new_part = if existing_flavor == Flavor::Simple {
                expand(value_raw.trim_start(), vars, None, loc)?
            } else {
                value_raw.trim_start().to_string()
            };
            if existing_raw.is_empty() {
                new_part
            } else {
                format!("{existing_raw} {new_part}")
            }
        }
        _ => value_raw.trim_start().to_string(),
    };

    // For conditional assignment: only set if undefined
    if flavor == Flavor::Conditional && vars.get(&name).is_some() {
        return Ok(());
    }

    let effective_flavor = if flavor == Flavor::Append {
        vars.get(&name).map(|v| v.flavor).unwrap_or(Flavor::Recursive)
    } else if flavor == Flavor::Conditional {
        Flavor::Recursive
    } else {
        flavor
    };

    vars.set(&name, Var { flavor: effective_flavor, origin, raw, exported: None });
    Ok(())
}

/// Split `NAME = value`, `NAME := value`, `NAME ?= value`, `NAME += value`
/// into `(name_raw, Flavor, value_raw)`.
fn split_assignment(line: &str) -> Option<(&str, Flavor, &str)> {
    let bytes = line.as_bytes();
    let mut depth = 0usize;
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'(' | b'{' => depth += 1,
            b')' | b'}' => depth = depth.saturating_sub(1),
            b':' if depth == 0 => {
                if bytes.get(i + 1) == Some(&b'=') {
                    return Some((&line[..i], Flavor::Simple, &line[i + 2..]));
                }
            }
            b'+' if depth == 0 && bytes.get(i + 1) == Some(&b'=') => {
                return Some((&line[..i], Flavor::Append, &line[i + 2..]));
            }
            b'?' if depth == 0 && bytes.get(i + 1) == Some(&b'=') => {
                return Some((&line[..i], Flavor::Conditional, &line[i + 2..]));
            }
            b'=' if depth == 0 => {
                // Plain = (not preceded by +, ?, :)
                return Some((&line[..i], Flavor::Recursive, &line[i + 1..]));
            }
            _ => {}
        }
        i += 1;
    }
    None
}

// ── Rule header parsing ───────────────────────────────────────────────────────

fn try_parse_rule(
    line: &str,
    vars: &crate::makefile::var::VarTable,
    loc: &Loc,
) -> Result<Option<RuleState>> {
    // Find the first `:` at depth 0 that isn't part of `:=`
    let colon_pos = find_rule_colon(line)?;
    let Some(colon) = colon_pos else { return Ok(None) };

    let targets_raw = &line[..colon];
    let after_colon = &line[colon + 1..];

    let double_colon = after_colon.starts_with(':');
    let rest = if double_colon { &after_colon[1..] } else { after_colon };

    // Expand targets
    let targets_expanded = expand(targets_raw.trim(), vars, None, loc)?;
    let targets: Vec<String> = targets_expanded.split_whitespace().map(str::to_string).collect();

    if targets.is_empty() { return Ok(None); }

    // Check for static pattern rule: `targets: pattern: prereqs`
    let (static_pattern, prereqs_raw) = parse_static_pattern(rest)?;

    // Expand prerequisite list
    let prereqs_expanded = expand(prereqs_raw.trim(), vars, None, loc)?;
    let prereqs = parse_prereqs(&prereqs_expanded);

    // Check for target-specific variable: `target: VAR = value`
    let target_var = if targets.len() == 1 && static_pattern.is_none() && prereqs.is_empty() {
        // The entire RHS might be an assignment
        if contains_assign(prereqs_raw.trim()) {
            let target = targets[0].clone();
            if let Some((name_raw, flavor, value_raw)) = split_assignment(prereqs_raw.trim()) {
                Some((target, name_raw.trim().to_string(), value_raw.trim_start().to_string(), flavor, Origin::File))
            } else {
                None
            }
        } else {
            None
        }
    } else {
        // Multi-target or normal rule — check if it's a target-specific var
        None
    };

    Ok(Some(RuleState {
        targets,
        prereqs,
        double_colon,
        loc: loc.clone(),
        recipe: Vec::new(),
        static_pattern,
        target_var,
    }))
}

fn find_rule_colon(line: &str) -> Result<Option<usize>> {
    let bytes = line.as_bytes();
    let mut depth = 0usize;
    for (i, &b) in bytes.iter().enumerate() {
        match b {
            b'(' | b'{' => depth += 1,
            b')' | b'}' => depth = depth.saturating_sub(1),
            b':' if depth == 0 => {
                // Not `:=` (that would be an assignment)
                if bytes.get(i + 1) != Some(&b'=') {
                    return Ok(Some(i));
                } else {
                    return Ok(None); // := is assignment, not rule
                }
            }
            b'=' if depth == 0 => {
                // Reached = first → assignment
                return Ok(None);
            }
            _ => {}
        }
    }
    Ok(None)
}

fn parse_static_pattern(rest: &str) -> Result<(Option<String>, &str)> {
    // Static pattern: `pattern: prereqs` — check for a second colon
    let bytes = rest.as_bytes();
    let mut depth = 0usize;
    for (i, &b) in bytes.iter().enumerate() {
        match b {
            b'(' | b'{' => depth += 1,
            b')' | b'}' => depth = depth.saturating_sub(1),
            b':' if depth == 0 => {
                if bytes.get(i + 1) != Some(&b'=') {
                    let pattern = rest[..i].trim().to_string();
                    let prereqs = &rest[i + 1..];
                    return Ok((Some(pattern), prereqs));
                }
            }
            _ => {}
        }
    }
    Ok((None, rest))
}

fn parse_prereqs(s: &str) -> Vec<Prereq> {
    let mut prereqs = Vec::new();
    let mut order_only = false;
    for word in s.split_whitespace() {
        if word == "|" {
            order_only = true;
            continue;
        }
        prereqs.push(if order_only { Prereq::order_only(word) } else { Prereq::normal(word) });
    }
    prereqs
}

// ── Committing a rule ─────────────────────────────────────────────────────────

fn commit_rule(rs: RuleState, mf: &mut Makefile, loc: &Loc) -> Result<()> {
    // Target-specific variable
    if let Some((target, name, raw, flavor, origin)) = rs.target_var {
        let entry = mf.graph.entry(&target);
        entry.target_vars.push((name, raw, flavor, origin));
        return Ok(());
    }

    for target in &rs.targets {
        if target.is_empty() { continue; }

        // Expand static pattern prereqs if applicable
        let prereqs = if let Some(ref pat) = rs.static_pattern {
            use crate::makefile::rule::{apply_pattern, match_pattern};
            if let Some(stem) = match_pattern(target, pat) {
                rs.prereqs.iter().map(|p| {
                    let name = if p.name.contains('%') {
                        apply_pattern(&p.name, stem)
                    } else {
                        p.name.clone()
                    };
                    Prereq { name, order_only: p.order_only }
                }).collect()
            } else {
                rs.prereqs.clone()
            }
        } else {
            rs.prereqs.clone()
        };

        if rs.double_colon {
            let dc_rule = Rule {
                targets: vec![target.clone()],
                prereqs: prereqs.clone(),
                recipe: rs.recipe.clone(),
                double_colon: true,
                loc: rs.loc.clone(),
            };
            mf.graph.entry(target).dc_rules.push(dc_rule);
        } else {
            let entry = mf.graph.entry(target);

            // Warn if replacing a recipe (GNU make compat)
            if !entry.recipe.is_empty() && !rs.recipe.is_empty() {
                eprintln!("{}: warning: overriding recipe for target '{target}'", rs.loc);
                eprintln!("{}: warning: ignoring old recipe for target '{target}'",
                    entry.recipe_loc.as_ref().unwrap_or(&rs.loc));
            }

            // Merge prerequisites (single-colon: they accumulate)
            for p in prereqs {
                if !entry.prereqs.iter().any(|ep| ep.name == p.name) {
                    entry.prereqs.push(p);
                }
            }

            if !rs.recipe.is_empty() {
                entry.recipe = rs.recipe.clone();
                entry.recipe_loc = Some(rs.loc.clone());
            }

            if entry.loc.is_none() {
                entry.loc = Some(rs.loc.clone());
            }

            // Track first target for default goal
            if mf.first_target.is_none() && !target.starts_with('.') {
                mf.first_target = Some(target.clone());
            }
        }
    }

    Ok(())
}

// ── Special targets ───────────────────────────────────────────────────────────

fn process_special_targets(rs: &RuleState, mf: &mut Makefile) -> bool {
    let mut is_special = false;
    for target in &rs.targets {
        match target.as_str() {
            ".PHONY" => {
                for p in &rs.prereqs {
                    mf.graph.entry(&p.name).phony = true;
                }
                is_special = true;
            }
            ".PRECIOUS" => {
                for p in &rs.prereqs {
                    mf.graph.entry(&p.name).precious = true;
                }
                is_special = true;
            }
            ".INTERMEDIATE" => {
                for p in &rs.prereqs {
                    mf.graph.entry(&p.name).intermediate = true;
                }
                is_special = true;
            }
            ".SECONDARY" => {
                for p in &rs.prereqs {
                    mf.graph.entry(&p.name).secondary = true;
                }
                is_special = true;
            }
            ".SUFFIXES" => {
                if rs.prereqs.is_empty() {
                    mf.graph.suffixes.clear();
                } else {
                    for p in &rs.prereqs {
                        if !mf.graph.suffixes.contains(&p.name) {
                            mf.graph.suffixes.push(p.name.clone());
                        }
                    }
                }
                is_special = true;
            }
            ".DEFAULT" => {
                if !rs.recipe.is_empty() {
                    mf.graph.default_recipe = Some(rs.recipe.clone());
                }
                is_special = true;
            }
            ".SECONDEXPANSION" => {
                mf.second_expansion = true;
                is_special = true;
            }
            ".EXPORT_ALL_VARIABLES" => {
                mf.vars.export_all = true;
                is_special = true;
            }
            ".NOTPARALLEL" => {
                mf.not_parallel = true;
                is_special = true;
            }
            ".IGNORE" => {
                if rs.prereqs.is_empty() {
                    // Global ignore
                    mf.global_ignore = true;
                }
                is_special = true;
            }
            ".SILENT" => {
                if rs.prereqs.is_empty() {
                    mf.global_silent = true;
                }
                is_special = true;
            }
            _ => {}
        }
    }
    // Also check if all targets were special targets
    // If the line was purely pattern rules (like %.o: %.c), not special
    if !is_special {
        // Check if any target is a pattern rule (contains %)
        for target in &rs.targets {
            if target.contains('%') {
                // This is a pattern rule
                let pr = PatternRule {
                    target: target.clone(),
                    prereqs: rs.prereqs.iter().map(|p| p.name.clone()).collect(),
                    recipe: rs.recipe.clone(),
                    is_builtin: false,
                    loc: rs.loc.clone(),
                };
                mf.graph.pattern_rules.push(pr);
                return true; // handled as pattern rule
            }
        }
    }
    is_special
}

// ── Helpers ───────────────────────────────────────────────────────────────────

fn is_skipping(stack: &[CondFrame]) -> bool {
    stack.iter().any(|f| !f.active)
}

fn leading_keyword<'a>(s: &str, keywords: &[&'a str]) -> Option<&'a str> {
    for &kw in keywords {
        if s == kw || s.starts_with(&format!("{kw} ")) || s.starts_with(&format!("{kw}\t")) {
            return Some(kw);
        }
    }
    None
}

fn strip_directive<'a>(line: &'a str, directive: &str) -> Option<&'a str> {
    if line == directive {
        return Some("");
    }
    if line.starts_with(directive) {
        let after = &line[directive.len()..];
        if after.starts_with(' ') || after.starts_with('\t') {
            return Some(after.trim_start());
        }
    }
    None
}

fn parse_ifeq_args(rest: &str, vars: &crate::makefile::var::VarTable, loc: &Loc) -> Result<(String, String)> {
    let rest = rest.trim();
    if rest.starts_with('(') {
        // `ifeq (A,B)` form
        let inner = rest.trim_start_matches('(');
        let (content, _) = crate::makefile::expand::split_args_raw(inner).into_iter()
            .fold((String::new(), 0usize), |_, _| (String::new(), 0));
        // Simple approach: find the balanced ')' and split at first comma
        if let Some(close) = rest.rfind(')') {
            let content = &rest[1..close];
            let comma = find_comma_depth0(content);
            let (a_raw, b_raw) = match comma {
                Some(i) => (&content[..i], &content[i + 1..]),
                None => (content, ""),
            };
            let a = expand(a_raw, vars, None, loc)?;
            let b = expand(b_raw, vars, None, loc)?;
            return Ok((a, b));
        }
        Ok((String::new(), String::new()))
    } else {
        // `ifeq "A" "B"` or `ifeq 'A' 'B'` form
        let parts = parse_quoted_pair(rest);
        let a = expand(&parts.0, vars, None, loc)?;
        let b = expand(&parts.1, vars, None, loc)?;
        Ok((a, b))
    }
}

fn find_comma_depth0(s: &str) -> Option<usize> {
    let mut depth = 0usize;
    for (i, c) in s.char_indices() {
        match c {
            '(' | '{' => depth += 1,
            ')' | '}' => depth = depth.saturating_sub(1),
            ',' if depth == 0 => return Some(i),
            _ => {}
        }
    }
    None
}

fn parse_quoted_pair(s: &str) -> (String, String) {
    fn unquote(s: &str) -> (String, &str) {
        let s = s.trim_start();
        let quote = s.chars().next().unwrap_or(' ');
        if quote == '"' || quote == '\'' {
            if let Some(end) = s[1..].find(quote) {
                return (s[1..end + 1].to_string(), &s[end + 2..]);
            }
        }
        // Space-separated
        let end = s.find(char::is_whitespace).unwrap_or(s.len());
        (s[..end].to_string(), &s[end..])
    }
    let (a, rest) = unquote(s);
    let (b, _) = unquote(rest.trim_start());
    (a, b)
}

fn parse_define_header(rest: &str) -> (String, Flavor) {
    let rest = rest.trim();
    // In GNU make 3.81, `define` always creates recursive variables
    // Later versions support `define VAR :=` etc. We follow 3.81.
    // Strip any trailing flavor operator for forward compat
    for (op, flavor) in &[(":=", Flavor::Simple), ("=", Flavor::Recursive),
                           ("+=", Flavor::Append), ("?=", Flavor::Conditional)] {
        if let Some(name) = rest.strip_suffix(op) {
            return (name.trim().to_string(), *flavor);
        }
    }
    (rest.to_string(), Flavor::Recursive)
}

fn resolve_include(path: &str, include_dirs: &[PathBuf]) -> PathBuf {
    let p = Path::new(path);
    if p.is_absolute() || p.exists() {
        return p.to_path_buf();
    }
    for dir in include_dirs {
        let candidate = dir.join(path);
        if candidate.exists() {
            return candidate;
        }
    }
    p.to_path_buf()
}

// ── Initial variable setup ────────────────────────────────────────────────────

pub fn setup_initial_vars(mf: &mut Makefile, args: &Args, env: &HashMap<String, String>) {
    use crate::makefile::implicit::{builtin_rules, builtin_vars, DEFAULT_SUFFIXES};

    // Default suffixes
    for s in DEFAULT_SUFFIXES {
        mf.graph.suffixes.push(s.to_string());
    }

    // Built-in variables
    if !args.no_builtin_vars {
        for (name, val) in builtin_vars() {
            mf.vars.set(name, Var::new(Flavor::Recursive, Origin::Default, val));
        }
    }

    // MAKE variable (path to our binary)
    let make_path = std::env::current_exe()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|_| "mk".to_string());
    mf.vars.set("MAKE", Var::new(Flavor::Simple, Origin::Default, make_path));
    mf.vars.set("MAKEFILE_LIST", Var::new(Flavor::Simple, Origin::Default, ""));
    mf.vars.set("CURDIR", Var::new(
        Flavor::Simple, Origin::Default,
        std::env::current_dir().map(|p| p.to_string_lossy().into_owned()).unwrap_or_default()
    ));
    mf.vars.set("MAKELEVEL", Var::new(
        Flavor::Simple, Origin::Default,
        std::env::var("MAKELEVEL").unwrap_or_else(|_| "0".to_string())
    ));

    // Environment variables (lower priority than file variables)
    for (k, v) in env {
        mf.vars.set(k, Var { flavor: Flavor::Simple, origin: Origin::Environment, raw: v.clone(), exported: None });
    }

    // If -e, re-set environment at a higher priority than file
    // (handled later; we just track the flag)

    // Command-line variable overrides (highest priority)
    for (name, value) in &args.overrides {
        mf.vars.set(name, Var::new(Flavor::Simple, Origin::CommandLine, value.clone()));
    }

    // Built-in implicit rules
    if !args.no_builtin_rules {
        for rule in builtin_rules() {
            mf.graph.pattern_rules.push(rule);
        }
    }

    // Store include dirs
    mf.include_dirs = args.include_dirs.clone();
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::makefile::Makefile;

    fn parse(src: &str) -> Makefile {
        let mut mf = Makefile::new();
        let args = Args::default();
        parse_str(src, "test", &mut mf, &args).unwrap();
        mf
    }

    #[test]
    fn simple_var_assignment() {
        let mf = parse("CC = gcc\n");
        assert_eq!(mf.vars.raw("CC"), "gcc");
    }

    #[test]
    fn immediate_var_assignment() {
        let mf = parse("CC := gcc\n");
        assert_eq!(mf.vars.raw("CC"), "gcc");
    }

    #[test]
    fn conditional_var_not_set() {
        let mf = parse("CC = gcc\nCC ?= clang\n");
        assert_eq!(mf.vars.raw("CC"), "gcc");
    }

    #[test]
    fn conditional_var_set() {
        let mf = parse("CC ?= clang\n");
        assert_eq!(mf.vars.raw("CC"), "clang");
    }

    #[test]
    fn append_var() {
        let mf = parse("CFLAGS = -O2\nCFLAGS += -g\n");
        assert_eq!(mf.vars.raw("CFLAGS"), "-O2 -g");
    }

    #[test]
    fn conditional_ifeq_true() {
        let mf = parse("ifeq (a,a)\nX = yes\nendif\n");
        assert_eq!(mf.vars.raw("X"), "yes");
    }

    #[test]
    fn conditional_ifeq_false() {
        let mf = parse("ifeq (a,b)\nX = yes\nendif\n");
        assert_eq!(mf.vars.raw("X"), "");
    }

    #[test]
    fn conditional_else() {
        let mf = parse("ifeq (a,b)\nX = yes\nelse\nX = no\nendif\n");
        assert_eq!(mf.vars.raw("X"), "no");
    }

    #[test]
    fn conditional_nested() {
        let mf = parse("ifeq (a,a)\nifeq (b,b)\nX = both\nendif\nendif\n");
        assert_eq!(mf.vars.raw("X"), "both");
    }

    #[test]
    fn define_block() {
        let mf = parse("define MYMACRO\nfoo\nbar\nendef\n");
        assert_eq!(mf.vars.raw("MYMACRO"), "foo\nbar");
    }

    #[test]
    fn simple_rule() {
        let mf = parse("all:\n\t@echo done\n");
        assert!(!mf.graph.entries.is_empty());
        assert!(mf.graph.entries.contains_key("all"));
    }

    #[test]
    fn phony_target() {
        let mf = parse(".PHONY: clean\nclean:\n\trm -f *.o\n");
        assert!(mf.graph.entries.get("clean").map(|e| e.phony).unwrap_or(false));
    }

    #[test]
    fn order_only_prereqs() {
        let mf = parse("foo.o: foo.c | build/\n");
        let entry = mf.graph.entries.get("foo.o").unwrap();
        let normal: Vec<_> = entry.prereqs.iter().filter(|p| !p.order_only).collect();
        let order: Vec<_> = entry.prereqs.iter().filter(|p| p.order_only).collect();
        assert_eq!(normal.len(), 1);
        assert_eq!(order.len(), 1);
        assert_eq!(order[0].name, "build/");
    }

    #[test]
    fn double_colon_rule() {
        let mf = parse("all:: foo\nall:: bar\n");
        let entry = mf.graph.entries.get("all").unwrap();
        assert_eq!(entry.dc_rules.len(), 2);
    }

    #[test]
    fn ifdef_defined() {
        let mf = parse("CC = gcc\nifdef CC\nX = yes\nendif\n");
        assert_eq!(mf.vars.raw("X"), "yes");
    }

    #[test]
    fn ifdef_undefined() {
        let mf = parse("ifdef UNDEFINED_VAR\nX = yes\nendif\n");
        assert_eq!(mf.vars.raw("X"), "");
    }
}
