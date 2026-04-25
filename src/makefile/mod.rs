pub mod exec;
pub mod expand;
pub mod graph;
pub mod implicit;
pub mod parse;
pub mod reader;
pub mod rule;
pub mod var;
pub mod vpath;

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::args::Args;
use crate::error::{fatal, Result};
use crate::makefile::graph::BuildGraph;
use crate::makefile::var::{Flavor, Origin, Var, VarTable};
use crate::makefile::vpath::VpathTable;

/// The central data structure built up during Makefile parsing.
pub struct Makefile {
    pub vars: VarTable,
    pub graph: BuildGraph,
    /// First non-special-target (for default goal selection).
    pub first_target: Option<String>,
    /// Overridden by `.DEFAULT_GOAL = name`.
    pub default_goal: Option<String>,
    /// All files read, in order (MAKEFILE_LIST).
    pub files_read: Vec<String>,
    /// `.SECONDEXPANSION` was seen.
    pub second_expansion: bool,
    /// `.NOTPARALLEL` was seen.
    pub not_parallel: bool,
    /// `.IGNORE` with no prerequisites (global ignore-errors).
    pub global_ignore: bool,
    /// `.SILENT` with no prerequisites (global silent mode).
    pub global_silent: bool,
    pub include_dirs: Vec<PathBuf>,
    pub vpath: VpathTable,
    pub restarts: u32,
}

impl Makefile {
    pub fn new() -> Self {
        Self {
            vars: VarTable::new(),
            graph: BuildGraph::new(),
            first_target: None,
            default_goal: None,
            files_read: Vec::new(),
            second_expansion: false,
            not_parallel: false,
            global_ignore: false,
            global_silent: false,
            include_dirs: Vec::new(),
            vpath: VpathTable::new(),
            restarts: 0,
        }
    }

    /// Select the default goal: .DEFAULT_GOAL variable, then first_target.
    pub fn default_goal(&self) -> Option<&str> {
        self.default_goal
            .as_deref()
            .or(self.first_target.as_deref())
    }

    /// Print the variable/rule database (-p flag output).
    pub fn print_db(&self) {
        println!("# Variables");
        let mut names: Vec<&str> = self.vars.iter().map(|(n, _)| n).collect();
        names.sort_unstable();
        for name in names {
            let v = self.vars.get(name).unwrap();
            let origin = self.vars.origin_str(name);
            let flavor = match v.flavor {
                var::Flavor::Simple => ":=",
                var::Flavor::Recursive => "=",
                var::Flavor::Append => "+=",
                var::Flavor::Conditional => "?=",
            };
            println!("# {origin}");
            println!("{name} {flavor} {}", v.raw);
        }
        println!();
        println!("# Pattern rules");
        for pr in &self.graph.pattern_rules {
            println!("{}: {}", pr.target, pr.prereqs.join(" "));
        }
        println!();
        println!("# Explicit rules");
        let mut targets: Vec<&str> = self.graph.entries.keys().map(String::as_str).collect();
        targets.sort_unstable();
        for t in targets {
            let e = &self.graph.entries[t];
            let prereqs: Vec<&str> = e.prereqs.iter().map(|p| p.name.as_str()).collect();
            if e.double_colon_exists() {
                for r in &e.dc_rules {
                    let pnames: Vec<&str> = r.prereqs.iter().map(|p| p.name.as_str()).collect();
                    println!("{t}:: {}", pnames.join(" "));
                }
            } else {
                println!("{t}: {}", prereqs.join(" "));
            }
        }
    }
}

impl graph::TargetEntry {
    pub fn double_colon_exists(&self) -> bool {
        !self.dc_rules.is_empty()
    }
}

// ── Makefile discovery ────────────────────────────────────────────────────────

/// Find the makefile to use: explicit `-f` args, then auto-discovery.
pub fn find_makefiles(args: &Args) -> Result<Vec<PathBuf>> {
    if !args.makefiles.is_empty() {
        // Validate all specified files exist (except "-" for stdin)
        for path in &args.makefiles {
            if path.as_os_str() == "-" { continue; }
            if !path.exists() {
                return Err(fatal(format!("{}: No such file or directory", path.display())));
            }
        }
        return Ok(args.makefiles.clone());
    }

    // Auto-discover in order: GNUmakefile, makefile, Makefile
    for name in &["GNUmakefile", "makefile", "Makefile"] {
        let p = PathBuf::from(name);
        if p.exists() {
            return Ok(vec![p]);
        }
    }

    Err(fatal("No targets specified and no makefile found"))
}

/// Parse one or more makefiles and merge them into a single `Makefile`.
pub fn load(paths: &[PathBuf], args: &Args, env: &HashMap<String, String>) -> Result<Makefile> {
    let mut mf = Makefile::new();
    parse::setup_initial_vars(&mut mf, args, env);

    for path in paths {
        if path.as_os_str() == "-" {
            use std::io::Read;
            let mut src = String::new();
            std::io::stdin().read_to_string(&mut src)?;
            parse::parse_str(&src, "<stdin>", &mut mf, args)?;
        } else {
            mf.files_read.push(path.to_string_lossy().into_owned());
            let src = std::fs::read_to_string(path).map_err(|e| {
                fatal(format!("{}: {e}", path.display()))
            })?;
            let mut cond_stack = Vec::new();
            let mut include_stack = vec![path.clone()];
            parse::parse_source_pub(&src, &path.to_string_lossy(), &mut mf, args, &mut cond_stack, &mut include_stack)?;
        }
    }

    // Update MAKEFILE_LIST
    let list = mf.files_read.join(" ");
    mf.vars.set("MAKEFILE_LIST", var::Var::new(var::Flavor::Simple, var::Origin::Default, list));

    // Check for makefile self-rebuild (Phase 5 — stub for now)

    Ok(mf)
}
