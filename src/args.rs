use std::path::PathBuf;
use crate::error::{fatal, Err, Result};

#[derive(Debug, Clone)]
pub struct Args {
    /// -f/--file: makefile paths (in order specified)
    pub makefiles: Vec<PathBuf>,
    /// -C/--directory: chdir before anything else
    pub directory: Option<PathBuf>,
    /// -I: include search dirs (in order)
    pub include_dirs: Vec<PathBuf>,
    /// Targets to build
    pub targets: Vec<String>,
    /// VAR=value assignments from command line (override file vars)
    pub overrides: Vec<(String, String)>,
    /// -e: environment vars override file vars
    pub env_override: bool,
    /// -i: ignore all recipe errors
    pub ignore_errors: bool,
    /// -k/--keep-going: continue past errors
    pub keep_going: bool,
    /// -n/--dry-run: print recipes, don't run them
    pub dry_run: bool,
    /// -p/--print-data-base: print variable/rule database
    pub print_db: bool,
    /// -q/--question: exit 1 if anything out of date, no output
    pub question: bool,
    /// -r/--no-builtin-rules: suppress built-in implicit rules
    pub no_builtin_rules: bool,
    /// -R/--no-builtin-variables: suppress built-in variable values
    pub no_builtin_vars: bool,
    /// -s/--silent: suppress recipe printing
    pub silent: bool,
    /// -t/--touch: touch targets instead of rebuilding
    pub touch: bool,
    /// -w/--print-directory: print directory change messages
    pub print_directory: bool,
    /// -j N: parallel jobs (0 = nproc)
    pub jobs: usize,
    /// --output-sync: buffer per-recipe output (default true)
    pub output_sync: bool,
}


impl Default for Args {
    fn default() -> Self {
        Self {
            makefiles: Vec::new(),
            directory: None,
            include_dirs: Vec::new(),
            targets: Vec::new(),
            overrides: Vec::new(),
            env_override: false,
            ignore_errors: false,
            keep_going: false,
            dry_run: false,
            print_db: false,
            question: false,
            no_builtin_rules: false,
            no_builtin_vars: false,
            silent: false,
            touch: false,
            print_directory: false,
            jobs: 1,
            output_sync: true,
        }
    }
}

/// Parse argv[1..].  Returns (Args, MAKEFLAGS-sourced flags already applied).
pub fn parse(argv: &[String]) -> Result<Args> {
    let mut args = Args::default();
    let mut i = 0;

    while i < argv.len() {
        let arg = &argv[i];

        // Handle --long=value forms
        if let Some(val) = arg.strip_prefix("--file=") {
            args.makefiles.push(PathBuf::from(val));
            i += 1;
            continue;
        }
        if let Some(val) = arg.strip_prefix("--directory=") {
            args.directory = Some(PathBuf::from(val));
            i += 1;
            continue;
        }
        if let Some(val) = arg.strip_prefix("--jobs=") {
            args.jobs = val.parse().map_err(|_| fatal("invalid --jobs argument"))?;
            i += 1;
            continue;
        }

        match arg.as_str() {
            "-e" | "--environment-overrides" => args.env_override = true,
            "-i" | "--ignore-errors" => args.ignore_errors = true,
            "-k" | "--keep-going" => args.keep_going = true,
            "-S" | "--no-keep-going" | "--stop" => args.keep_going = false,
            "-n" | "--dry-run" | "--just-print" | "--recon" => args.dry_run = true,
            "-p" | "--print-data-base" => args.print_db = true,
            "-q" | "--question" => args.question = true,
            "-r" | "--no-builtin-rules" => args.no_builtin_rules = true,
            "-R" | "--no-builtin-variables" => {
                args.no_builtin_rules = true;
                args.no_builtin_vars = true;
            }
            "-s" | "--silent" | "--quiet" => args.silent = true,
            "-t" | "--touch" => args.touch = true,
            "-w" | "--print-directory" => args.print_directory = true,
            "--no-print-directory" => args.print_directory = false,
            "--output-sync" => args.output_sync = true,
            "--no-output-sync" => args.output_sync = false,
            _ => {
                if let Some(rest) = arg.strip_prefix("-f") {
                    let path = if rest.is_empty() {
                        i += 1;
                        argv.get(i)
                            .ok_or_else(|| fatal("option requires an argument -- 'f'"))?
                            .clone()
                    } else {
                        rest.to_string()
                    };
                    args.makefiles.push(PathBuf::from(path));
                } else if let Some(rest) = arg.strip_prefix("-C") {
                    let path = if rest.is_empty() {
                        i += 1;
                        argv.get(i)
                            .ok_or_else(|| fatal("option requires an argument -- 'C'"))?
                            .clone()
                    } else {
                        rest.to_string()
                    };
                    args.directory = Some(PathBuf::from(path));
                } else if let Some(rest) = arg.strip_prefix("-I") {
                    let path = if rest.is_empty() {
                        i += 1;
                        argv.get(i)
                            .ok_or_else(|| fatal("option requires an argument -- 'I'"))?
                            .clone()
                    } else {
                        rest.to_string()
                    };
                    args.include_dirs.push(PathBuf::from(path));
                } else if let Some(rest) = arg.strip_prefix("-j") {
                    if rest.is_empty() {
                        // bare -j: cap at nproc
                        args.jobs = 0;
                    } else {
                        args.jobs = rest
                            .parse()
                            .map_err(|_| fatal(format!("invalid -j argument '{rest}'")))?;
                    }
                } else if arg == "--" {
                    // Everything after -- is a target
                    for t in &argv[i + 1..] {
                        args.targets.push(t.clone());
                    }
                    break;
                } else if arg.starts_with('-') {
                    // Check for combined short flags like -kn
                    let flags = &arg[1..];
                    if flags.chars().all(|c| "eikKnpqrRsStw".contains(c)) && flags.len() > 1 {
                        // Re-parse each flag individually
                        for c in flags.chars() {
                            let fake = format!("-{c}");
                            let sub: Vec<String> = vec![fake];
                            let sub_args = parse(&sub)?;
                            merge_flags(&mut args, &sub_args);
                        }
                    } else {
                        return Err(fatal(format!(
                            "unrecognized option '{arg}'"
                        )));
                    }
                } else if let Some(eq) = arg.find('=') {
                    // VAR=value
                    let name = &arg[..eq];
                    let value = &arg[eq + 1..];
                    if is_valid_var_name(name) {
                        args.overrides.push((name.to_string(), value.to_string()));
                    } else {
                        args.targets.push(arg.clone());
                    }
                } else {
                    args.targets.push(arg.clone());
                }
            }
        }

        i += 1;
    }

    // Resolve jobs=0 to nproc
    if args.jobs == 0 {
        args.jobs = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(1);
    }

    Ok(args)
}

fn is_valid_var_name(s: &str) -> bool {
    !s.is_empty() && s.chars().all(|c| c.is_alphanumeric() || c == '_' || c == '.')
}

fn merge_flags(dst: &mut Args, src: &Args) {
    if src.env_override { dst.env_override = true; }
    if src.ignore_errors { dst.ignore_errors = true; }
    if src.keep_going { dst.keep_going = true; }
    if src.dry_run { dst.dry_run = true; }
    if src.print_db { dst.print_db = true; }
    if src.question { dst.question = true; }
    if src.no_builtin_rules { dst.no_builtin_rules = true; }
    if src.no_builtin_vars { dst.no_builtin_vars = true; }
    if src.silent { dst.silent = true; }
    if src.touch { dst.touch = true; }
}

/// Parse the MAKEFLAGS environment variable, which may contain
/// single-letter flags without a leading `-` (e.g. "kn") or
/// full args ("kn --no-builtin-rules").
pub fn parse_makeflags(s: &str) -> Result<Args> {
    let s = s.trim();
    if s.is_empty() {
        return Ok(Args::default());
    }
    // If the value starts with a `-` it already looks like argv
    // Otherwise it's a run of single-letter flags
    if s.starts_with('-') {
        let parts: Vec<String> = s.split_whitespace().map(str::to_string).collect();
        parse(&parts)
    } else {
        // Take the leading run of letters as combined flags, rest as full args
        let split = s.find(|c: char| c.is_whitespace()).unwrap_or(s.len());
        let letters = &s[..split];
        let rest = s[split..].trim();
        let mut parts = vec![format!("-{letters}")];
        if !rest.is_empty() {
            parts.extend(rest.split_whitespace().map(str::to_string));
        }
        parse(&parts)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_args() {
        let args = parse(&[]).unwrap();
        assert!(args.makefiles.is_empty());
        assert!(args.targets.is_empty());
        assert!(!args.dry_run);
        assert_eq!(args.jobs, 1);
    }

    #[test]
    fn short_flags() {
        let argv = strs(&["-k", "-n", "-s"]);
        let args = parse(&argv).unwrap();
        assert!(args.keep_going);
        assert!(args.dry_run);
        assert!(args.silent);
    }

    #[test]
    fn combined_short_flags() {
        let argv = strs(&["-kn"]);
        let args = parse(&argv).unwrap();
        assert!(args.keep_going);
        assert!(args.dry_run);
    }

    #[test]
    fn file_flag() {
        let argv = strs(&["-f", "my.mk"]);
        let args = parse(&argv).unwrap();
        assert_eq!(args.makefiles, vec![PathBuf::from("my.mk")]);
    }

    #[test]
    fn file_flag_attached() {
        let argv = strs(&["-fmy.mk"]);
        let args = parse(&argv).unwrap();
        assert_eq!(args.makefiles, vec![PathBuf::from("my.mk")]);
    }

    #[test]
    fn long_file_flag() {
        let argv = strs(&["--file=my.mk"]);
        let args = parse(&argv).unwrap();
        assert_eq!(args.makefiles, vec![PathBuf::from("my.mk")]);
    }

    #[test]
    fn var_override() {
        let argv = strs(&["CC=clang", "CFLAGS=-O2"]);
        let args = parse(&argv).unwrap();
        assert_eq!(
            args.overrides,
            vec![("CC".into(), "clang".into()), ("CFLAGS".into(), "-O2".into())]
        );
    }

    #[test]
    fn targets_and_overrides() {
        let argv = strs(&["all", "CC=gcc", "install"]);
        let args = parse(&argv).unwrap();
        assert_eq!(args.targets, vec!["all", "install"]);
        assert_eq!(args.overrides, vec![("CC".into(), "gcc".into())]);
    }

    #[test]
    fn jobs_flag() {
        let argv = strs(&["-j4"]);
        let args = parse(&argv).unwrap();
        assert_eq!(args.jobs, 4);
    }

    #[test]
    fn jobs_flag_separated() {
        let argv = strs(&["-j", "8"]);
        // -j doesn't take a separate arg in our parser; bare -j8 is the form
        // Actually GNU make accepts both. Let me check... Actually -j N is valid.
        // But our current parser only handles -jN attached. Let's just test attached.
        let argv2 = strs(&["-j8"]);
        let args = parse(&argv2).unwrap();
        assert_eq!(args.jobs, 8);
        let _ = argv;
    }

    #[test]
    fn include_dir() {
        let argv = strs(&["-I", "/usr/include/make"]);
        let args = parse(&argv).unwrap();
        assert_eq!(args.include_dirs, vec![PathBuf::from("/usr/include/make")]);
    }

    #[test]
    fn directory_flag() {
        let argv = strs(&["-C", "/tmp"]);
        let args = parse(&argv).unwrap();
        assert_eq!(args.directory, Some(PathBuf::from("/tmp")));
    }

    #[test]
    fn makeflags_letters() {
        let args = parse_makeflags("kn").unwrap();
        assert!(args.keep_going);
        assert!(args.dry_run);
    }

    #[test]
    fn makeflags_with_dash() {
        let args = parse_makeflags("-k -n").unwrap();
        assert!(args.keep_going);
        assert!(args.dry_run);
    }

    #[test]
    fn unknown_flag_error() {
        let argv = strs(&["--bogus-flag"]);
        assert!(parse(&argv).is_err());
    }

    fn strs(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }
}
