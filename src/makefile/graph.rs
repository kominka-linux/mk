//! Dependency graph, topological sort, and up-to-date checking.

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::time::SystemTime;

use crate::error::{Loc, Result};
use crate::makefile::rule::{PatternRule, Prereq, RecipeLine, Rule, apply_pattern, match_pattern};

/// Everything we know about a single target.
#[derive(Debug, Default, Clone)]
pub struct TargetEntry {
    /// Explicit single-colon rules whose prerequisites are merged.
    pub prereqs: Vec<Prereq>,
    /// Recipe from the last explicit rule that supplied one.
    pub recipe: Vec<RecipeLine>,
    pub recipe_loc: Option<Loc>,
    /// Double-colon rules — run independently, in order.
    pub dc_rules: Vec<Rule>,
    /// True when the target appears in `.PHONY`.
    pub phony: bool,
    /// True when the target appears in `.PRECIOUS`.
    pub precious: bool,
    /// True when the target is an intermediate file.
    pub intermediate: bool,
    /// True when the target is `.SECONDARY` (like intermediate but not deleted).
    pub secondary: bool,
    /// Location of the first definition (for error messages).
    pub loc: Option<Loc>,
    /// Resolved mtime cache (`None` means uncached / file missing).
    pub mtime: Option<Option<SystemTime>>,
    /// Applied stem from an implicit rule (for `$*`).
    pub stem: Option<String>,
    /// Applied pattern rule index (if found via implicit lookup).
    pub implicit_rule: Option<usize>,
    /// Whether this target was built in this run.
    pub built: bool,
    /// Target-specific variable assignments: `(name, raw_value, flavor, origin)`.
    pub target_vars: Vec<(String, String, crate::makefile::var::Flavor, crate::makefile::var::Origin)>,
}

pub struct BuildGraph {
    pub entries: HashMap<String, TargetEntry>,
    /// `.DEFAULT` recipe (used when no rule matches a target).
    pub default_recipe: Option<Vec<RecipeLine>>,
    pub pattern_rules: Vec<PatternRule>,
    pub suffixes: Vec<String>,
    /// Archive member targets: `lib(member)` syntax.
    pub archive_members: HashSet<String>,
}

impl BuildGraph {
    pub fn new() -> Self {
        Self {
            entries: HashMap::new(),
            default_recipe: None,
            pattern_rules: Vec::new(),
            suffixes: Vec::new(),
            archive_members: HashSet::new(),
        }
    }

    pub fn entry(&mut self, name: &str) -> &mut TargetEntry {
        self.entries.entry(name.to_string()).or_default()
    }

    /// Return all normal (non-order-only) prerequisites for a target,
    /// including those resolved through vpath and implicit rules.
    pub fn prereqs_of(&self, name: &str) -> Vec<String> {
        self.entries.get(name)
            .map(|e| e.prereqs.iter().filter(|p| !p.order_only).map(|p| p.name.clone()).collect())
            .unwrap_or_default()
    }

    pub fn order_only_of(&self, name: &str) -> Vec<String> {
        self.entries.get(name)
            .map(|e| e.prereqs.iter().filter(|p| p.order_only).map(|p| p.name.clone()).collect())
            .unwrap_or_default()
    }

    /// True if `target` needs to be rebuilt.
    pub fn needs_rebuild(&mut self, target: &str) -> bool {
        let phony = self.entries.get(target).map(|e| e.phony).unwrap_or(false);
        if phony { return true; }

        let target_mtime = self.get_mtime(target);
        match target_mtime {
            None => true, // doesn't exist
            Some(tm) => {
                let prereqs = self.prereqs_of(target);
                for prereq in prereqs {
                    if let Some(pm) = self.get_mtime(&prereq) {
                        if pm > tm { return true; }
                    } else {
                        return true; // prereq missing (will error or be built)
                    }
                }
                false
            }
        }
    }

    fn get_mtime(&mut self, target: &str) -> Option<SystemTime> {
        let entry = self.entries.entry(target.to_string()).or_default();
        if let Some(cached) = entry.mtime {
            return cached;
        }
        let mtime = Path::new(target).metadata().ok().and_then(|m| m.modified().ok());
        entry.mtime = Some(mtime);
        mtime
    }

    /// Invalidate the cached mtime (call after a target is built).
    pub fn invalidate_mtime(&mut self, target: &str) {
        if let Some(e) = self.entries.get_mut(target) {
            e.mtime = None;
        }
    }

    /// Topological sort starting from `goals`.
    /// Returns targets in build order (leaves first).
    pub fn topo_sort(&self, goals: &[String]) -> Result<Vec<String>> {
        let mut order = Vec::new();
        let mut visited = HashSet::new();
        let mut in_stack = HashSet::new();

        for goal in goals {
            self.dfs(goal, &mut order, &mut visited, &mut in_stack)?;
        }

        Ok(order)
    }

    fn dfs(
        &self,
        target: &str,
        order: &mut Vec<String>,
        visited: &mut HashSet<String>,
        in_stack: &mut HashSet<String>,
    ) -> Result<()> {
        if visited.contains(target) { return Ok(()); }
        if in_stack.contains(target) {
            // Cycle: GNU make drops it with a warning and continues
            // We emit the same message format
            eprintln!("mk: Circular {target} <- {target} dependency dropped.");
            return Ok(());
        }

        in_stack.insert(target.to_string());

        // Visit all prerequisites
        if let Some(entry) = self.entries.get(target) {
            for prereq in &entry.prereqs {
                self.dfs(&prereq.name, order, visited, in_stack)?;
            }
        }

        in_stack.remove(target);
        visited.insert(target.to_string());
        order.push(target.to_string());

        Ok(())
    }

    /// Find which implicit rule (if any) applies to `target`.
    /// Mutates the entry to store the resolved stem and rule index.
    pub fn resolve_implicit(&mut self, target: &str) -> bool {
        // Skip if we already have an explicit recipe
        if let Some(e) = self.entries.get(target) {
            if !e.recipe.is_empty() || !e.dc_rules.is_empty() {
                return true;
            }
        }

        for (i, rule) in self.pattern_rules.iter().enumerate() {
            let Some(stem) = match_pattern(target, &rule.target) else { continue };

            // Check that all pattern prereqs are satisfiable
            let all_ok = rule.prereqs.iter().all(|pp| {
                let concrete = apply_pattern(pp, &stem);
                Path::new(&concrete).exists()
                    || self.entries.contains_key(&concrete)
                    || self.can_build_implicit(&concrete, i + 1)
            });

            if all_ok {
                let stem = stem.to_string();
                let rule_clone = rule.clone();
                let prereqs: Vec<Prereq> = rule_clone.prereqs.iter().map(|pp| {
                    Prereq::normal(apply_pattern(pp, &stem))
                }).collect();

                let entry = self.entry(target);
                if entry.recipe.is_empty() {
                    entry.recipe = rule_clone.recipe.clone();
                    entry.recipe_loc = Some(rule_clone.loc.clone());
                    entry.stem = Some(stem.clone());
                    entry.implicit_rule = Some(i);
                    // Add prereqs not already listed
                    for p in prereqs {
                        if !entry.prereqs.iter().any(|ep| ep.name == p.name) {
                            entry.prereqs.push(p);
                        }
                    }
                }
                return true;
            }
        }

        false
    }

    fn can_build_implicit(&self, target: &str, start_at: usize) -> bool {
        for rule in &self.pattern_rules[start_at..] {
            if let Some(stem) = match_pattern(target, &rule.target) {
                let all_ok = rule.prereqs.iter().all(|pp| {
                    Path::new(&apply_pattern(pp, stem)).exists()
                });
                if all_ok { return true; }
            }
        }
        false
    }
}

/// Parse an archive member target like `libfoo.a(bar.o)` into `(lib, member)`.
pub fn parse_archive_target(target: &str) -> Option<(&str, &str)> {
    let paren = target.find('(')?;
    if !target.ends_with(')') { return None; }
    let lib = &target[..paren];
    let member = &target[paren + 1..target.len() - 1];
    Some((lib, member))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::makefile::rule::Prereq;
    use crate::error::Loc;

    fn loc() -> Loc { Loc::new("Makefile", 1) }

    #[test]
    fn topo_sort_simple() {
        let mut g = BuildGraph::new();
        g.entry("all").prereqs = vec![Prereq::normal("foo.o")];
        g.entry("foo.o").prereqs = vec![Prereq::normal("foo.c")];
        g.entry("foo.c");

        let order = g.topo_sort(&["all".to_string()]).unwrap();
        assert_eq!(order, vec!["foo.c", "foo.o", "all"]);
    }

    #[test]
    fn phony_always_rebuilds() {
        let mut g = BuildGraph::new();
        g.entry("clean").phony = true;
        assert!(g.needs_rebuild("clean"));
    }

    #[test]
    fn missing_target_needs_rebuild() {
        let mut g = BuildGraph::new();
        g.entry("/nonexistent-file-mk-test");
        assert!(g.needs_rebuild("/nonexistent-file-mk-test"));
    }

    #[test]
    fn parse_archive_target_basic() {
        assert_eq!(parse_archive_target("libfoo.a(bar.o)"), Some(("libfoo.a", "bar.o")));
    }

    #[test]
    fn parse_archive_target_no_match() {
        assert_eq!(parse_archive_target("libfoo.a"), None);
        assert_eq!(parse_archive_target("lib("), None);
    }

    #[test]
    fn cycle_does_not_panic() {
        let mut g = BuildGraph::new();
        g.entry("a").prereqs = vec![Prereq::normal("b")];
        g.entry("b").prereqs = vec![Prereq::normal("a")];
        // Should not panic, just emit a warning
        let _ = g.topo_sort(&["a".to_string()]);
    }
}
