//! Sequential recipe execution.
//!
//! Phase 6 (parallel + jobserver + signals) will extend this module.

use std::collections::HashSet;
use std::path::Path;
use std::process::Command;

use crate::args::Args;
use crate::error::{fatal, fatal_at, Err, Loc, Result};
use crate::makefile::expand::{expand, AutoVars};
use crate::makefile::graph::BuildGraph;
use crate::makefile::rule::RecipeLine;
use crate::makefile::var::VarTable;

/// Outcome of attempting to build a target.
pub enum BuildResult {
    /// Target was (re)built successfully.
    Built,
    /// Target was already up to date.
    UpToDate,
    /// Build failed; if `keep_going`, we continue.
    Failed(Err),
}

pub struct Executor<'a> {
    pub args: &'a Args,
    pub vars: &'a VarTable,
    /// MAKELEVEL for recursive $(MAKE) calls.
    pub makelevel: usize,
    /// Targets currently being built (for detecting `.PRECIOUS` on interrupt).
    pub in_progress: HashSet<String>,
}

impl<'a> Executor<'a> {
    pub fn new(args: &'a Args, vars: &'a VarTable, makelevel: usize) -> Self {
        Self { args, vars, makelevel, in_progress: HashSet::new() }
    }

    /// Build all `goals` in dependency order.
    /// Returns the number of targets that failed.
    pub fn build_all(
        &mut self,
        goals: &[String],
        graph: &mut BuildGraph,
    ) -> Result<usize> {
        let order = graph.topo_sort(goals)?;
        let mut failures = 0;

        // Resolve implicit rules for targets without explicit recipes
        for target in &order {
            graph.resolve_implicit(target);
        }

        for target in &order {
            match self.build_one(target, graph)? {
                BuildResult::Built | BuildResult::UpToDate => {}
                BuildResult::Failed(e) => {
                    eprintln!("{e}");
                    failures += 1;
                    if !self.args.keep_going {
                        return Ok(failures);
                    }
                }
            }
        }

        Ok(failures)
    }

    fn build_one(&mut self, target: &str, graph: &mut BuildGraph) -> Result<BuildResult> {
        // Check if up to date (after all prereqs have been processed)
        if !graph.needs_rebuild(target) {
            return Ok(BuildResult::UpToDate);
        }

        let entry = graph.entries.get(target).cloned().unwrap_or_default();

        // Double-colon rules run independently
        if !entry.dc_rules.is_empty() {
            for rule in &entry.dc_rules {
                let auto = self.make_auto(target, &rule.prereqs.iter().map(|p| p.name.clone()).collect::<Vec<_>>(), "");
                if let Err(e) = self.run_recipe(&rule.recipe, &auto, target, &rule.loc) {
                    if !self.args.ignore_errors && !self.args.keep_going {
                        return Ok(BuildResult::Failed(e));
                    }
                    eprintln!("{e}");
                }
            }
            graph.invalidate_mtime(target);
            return Ok(BuildResult::Built);
        }

        let recipe = entry.recipe.clone();
        let loc = entry.recipe_loc.clone().unwrap_or_else(|| Loc::new(target, 0));

        if recipe.is_empty() {
            // Try .DEFAULT
            if let Some(default) = &graph.default_recipe.clone() {
                let auto = self.make_auto(target, &[], "");
                if let Err(e) = self.run_recipe(default, &auto, target, &loc) {
                    return Ok(BuildResult::Failed(e));
                }
            } else {
                // No rule: fatal unless the target is a file that exists
                if !Path::new(target).exists() {
                    return Ok(BuildResult::Failed(fatal(format!(
                        "No rule to make target '{target}'"
                    ))));
                }
            }
            return Ok(BuildResult::UpToDate);
        }

        // Compute newer prereqs for $?
        let all_prereqs: Vec<String> = entry.prereqs.iter()
            .filter(|p| !p.order_only)
            .map(|p| p.name.clone())
            .collect();
        let order_only: Vec<String> = entry.prereqs.iter()
            .filter(|p| p.order_only)
            .map(|p| p.name.clone())
            .collect();

        let target_mtime = Path::new(target).metadata().ok().and_then(|m| m.modified().ok());
        let newer: Vec<String> = all_prereqs.iter()
            .filter(|p| {
                if let Some(tm) = target_mtime {
                    Path::new(p).metadata().ok()
                        .and_then(|m| m.modified().ok())
                        .map(|pm| pm > tm)
                        .unwrap_or(true)
                } else {
                    true
                }
            })
            .cloned()
            .collect();

        let stem = entry.stem.clone().unwrap_or_default();
        let auto = self.make_auto_full(target, &all_prereqs, &order_only, &newer, &stem);

        if self.args.touch {
            return self.do_touch(target, &loc);
        }

        if self.args.question {
            // Any target needing a rebuild means exit 1
            return Ok(BuildResult::Failed(Err::Fatal("".to_string())));
        }

        if let Err(e) = self.run_recipe(&recipe, &auto, target, &loc) {
            if entry.precious {
                // Don't delete, but still propagate error
            }
            return Ok(BuildResult::Failed(e));
        }

        graph.invalidate_mtime(target);
        Ok(BuildResult::Built)
    }

    fn run_recipe(
        &mut self,
        recipe: &[RecipeLine],
        auto: &AutoVars,
        target: &str,
        loc: &Loc,
    ) -> Result<()> {
        for line in recipe {
            let expanded = expand(&line.text, self.vars, Some(auto), loc)?;
            let expanded = self.replace_make_var(&expanded);

            let silent = line.silent || self.args.silent;
            let ignore_err = line.ignore_error || self.args.ignore_errors;
            let always_run = line.always_run;

            if !silent {
                println!("{expanded}");
            }

            if self.args.dry_run && !always_run {
                continue;
            }

            let shell = self.vars.raw("SHELL");
            let shell = if shell.is_empty() { "/bin/sh" } else { shell };

            let mut env_exports = self.vars.env_exports();
            env_exports.insert("MAKELEVEL".to_string(), (self.makelevel + 1).to_string());

            let status = Command::new(shell)
                .arg("-c")
                .arg(&expanded)
                .envs(&env_exports)
                .status();

            match status {
                Err(e) => {
                    if !ignore_err {
                        return Err(fatal_at(loc.clone(), format!("recipe for target '{target}' failed: {e}")));
                    }
                }
                Ok(s) if !s.success() => {
                    let code = s.code().unwrap_or(1);
                    let e = Err::RecipeError { loc: loc.clone(), target: target.to_string(), code };
                    if !ignore_err {
                        eprintln!("{e}");
                        return Err(fatal_at(loc.clone(), format!("[{target}] Error {code}")));
                    }
                    eprintln!("{}", Err::Warning(loc.clone(), format!("ignoring errors from recipe for '{target}'")));
                }
                Ok(_) => {}
            }
        }
        Ok(())
    }

    fn do_touch(&self, target: &str, loc: &Loc) -> Result<BuildResult> {
        if !self.args.silent {
            println!("touch {target}");
        }
        if !self.args.dry_run {
            if Path::new(target).exists() {
                let _ = Command::new("touch").arg(target).status();
            } else {
                let _ = std::fs::File::create(target);
            }
        }
        Ok(BuildResult::Built)
    }

    fn replace_make_var(&self, cmd: &str) -> String {
        // Replace $(MAKE) / ${MAKE} with our own binary path
        let mk_path = std::env::current_exe()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_else(|_| "mk".to_string());
        cmd.replace("$(MAKE)", &mk_path).replace("${MAKE}", &mk_path)
    }

    fn make_auto(&self, target: &str, prereqs: &[String], stem: &str) -> AutoVars {
        self.make_auto_full(target, prereqs, &[], prereqs, stem)
    }

    fn make_auto_full(
        &self,
        target: &str,
        normal: &[String],
        order_only: &[String],
        newer: &[String],
        stem: &str,
    ) -> AutoVars {
        // Deduplicate for $^
        let mut seen = std::collections::HashSet::new();
        let deduped: Vec<&str> = normal.iter()
            .filter(|n| seen.insert(n.as_str()))
            .map(String::as_str)
            .collect();

        AutoVars {
            target: target.to_string(),
            first_prereq: normal.first().cloned().unwrap_or_default(),
            all_prereqs: deduped.join(" "),
            all_prereqs_dup: normal.join(" "),
            newer_prereqs: newer.join(" "),
            stem: stem.to_string(),
            order_only: order_only.join(" "),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::makefile::expand::AutoVars;

    #[test]
    fn auto_vars_computed() {
        let args = Args::default();
        let vars = VarTable::new();
        let exec = Executor::new(&args, &vars, 0);
        let auto = exec.make_auto_full(
            "foo.o",
            &["foo.c".to_string(), "foo.h".to_string()],
            &[],
            &["foo.c".to_string()],
            "foo",
        );
        assert_eq!(auto.target, "foo.o");
        assert_eq!(auto.first_prereq, "foo.c");
        assert_eq!(auto.all_prereqs, "foo.c foo.h");
        assert_eq!(auto.newer_prereqs, "foo.c");
        assert_eq!(auto.stem, "foo");
    }

    #[test]
    fn auto_vars_dedup() {
        let args = Args::default();
        let vars = VarTable::new();
        let exec = Executor::new(&args, &vars, 0);
        let auto = exec.make_auto("foo.o", &["a.c".to_string(), "a.c".to_string(), "b.c".to_string()], "");
        assert_eq!(auto.all_prereqs, "a.c b.c");
        assert_eq!(auto.all_prereqs_dup, "a.c a.c b.c");
    }
}
