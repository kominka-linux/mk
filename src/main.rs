mod args;
mod error;
mod makefile;

use std::collections::HashMap;
use std::process;

use args::Args;
use error::{fatal, set_makelevel, Result};

fn main() {
    let argv: Vec<String> = std::env::args().collect();
    match run(&argv) {
        Ok(exit) => process::exit(exit),
        Err(e) => {
            eprintln!("{e}");
            process::exit(e.exit_code().max(2))
        }
    }
}

fn run(argv: &[String]) -> Result<i32> {
    // Parse MAKELEVEL from environment
    let makelevel: usize = std::env::var("MAKELEVEL")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    set_makelevel(makelevel);

    // Parse args
    let args = args::parse(&argv[1..])?;

    // Apply -C (chdir) before anything else
    if let Some(ref dir) = args.directory {
        std::env::set_current_dir(dir).map_err(|e| {
            fatal(format!("{}: {e}", dir.display()))
        })?;
    }

    // Collect environment
    let env: HashMap<String, String> = std::env::vars().collect();

    // Find and load makefiles
    let paths = makefile::find_makefiles(&args)?;
    let mut mf = makefile::load(&paths, &args, &env)?;

    // Apply -e: environment overrides file variables
    if args.env_override {
        use makefile::var::{Flavor, Origin, Var};
        for (k, v) in &env {
            if mf.vars.get(k).map(|v| v.origin < Origin::CommandLine).unwrap_or(true) {
                mf.vars.set(k, Var { flavor: Flavor::Simple, origin: Origin::Environment, raw: v.clone(), exported: None });
            }
        }
    }

    // -p: print database then exit
    if args.print_db {
        mf.print_db();
        return Ok(0);
    }

    // Select goals
    let goals: Vec<String> = if args.targets.is_empty() {
        match mf.default_goal() {
            Some(g) => vec![g.to_string()],
            None => return Err(fatal("No targets")),
        }
    } else {
        args.targets.clone()
    };

    // Set MAKECMDGOALS
    {
        use makefile::var::{Flavor, Origin, Var};
        mf.vars.set(
            "MAKECMDGOALS",
            Var::new(Flavor::Simple, Origin::Default, goals.join(" ")),
        );
    }

    // Print entering-directory message if -w or -C
    if args.print_directory || args.directory.is_some() {
        let dir = std::env::current_dir()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_default();
        let prog = error::prog();
        eprintln!("{prog}: Entering directory '{dir}'");
    }

    // Apply global flags from .IGNORE and .SILENT before building
    let effective_args = apply_global_flags(&args, &mf);
    let mut exec = makefile::exec::Executor::new(&effective_args, &mf.vars, makelevel);

    let failures = exec.build_all(&goals, &mut mf.graph)?;

    if args.print_directory || args.directory.is_some() {
        let dir = std::env::current_dir()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_default();
        let prog = error::prog();
        eprintln!("{prog}: Leaving directory '{dir}'");
    }

    if failures > 0 {
        Ok(2)
    } else {
        Ok(0)
    }
}

fn apply_global_flags(args: &Args, mf: &makefile::Makefile) -> Args {
    let mut a = args.clone();
    if mf.global_ignore { a.ignore_errors = true; }
    if mf.global_silent { a.silent = true; }
    a
}
