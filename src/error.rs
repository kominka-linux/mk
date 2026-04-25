use std::fmt;

#[derive(Debug, Clone)]
pub struct Loc {
    pub file: String,
    pub line: usize,
}

impl Loc {
    pub fn new(file: impl Into<String>, line: usize) -> Self {
        Self { file: file.into(), line }
    }
}

impl fmt::Display for Loc {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}", self.file, self.line)
    }
}

/// The current recursion depth, used to format the program name (mk vs mk[N]).
static MAKELEVEL: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

pub fn set_makelevel(n: usize) {
    MAKELEVEL.store(n, std::sync::atomic::Ordering::Relaxed);
}

pub fn prog() -> String {
    let lvl = MAKELEVEL.load(std::sync::atomic::Ordering::Relaxed);
    if lvl == 0 { "mk".into() } else { format!("mk[{}]", lvl) }
}

#[derive(Debug)]
pub enum Err {
    /// `mk: *** {msg}.  Stop.`
    Fatal(String),
    /// `file:line: *** {msg}.  Stop.`
    FatalAt(Loc, String),
    /// `mk: *** [{loc}: {target}] Error {code}`
    RecipeError { loc: Loc, target: String, code: i32 },
    /// `mk: *** [{loc}: {target}] Interrupt`
    RecipeInterrupt { loc: Loc, target: String },
    /// Warning emitted to stderr; execution continues
    Warning(Loc, String),
}

impl Err {
    pub fn is_warning(&self) -> bool {
        matches!(self, Err::Warning(..))
    }

    /// Exit code this error should produce (2 for fatal, 1 for question-mode).
    pub fn exit_code(&self) -> i32 {
        match self {
            Err::Warning(..) => 0,
            Err::RecipeError { code, .. } => *code,
            _ => 2,
        }
    }
}

impl fmt::Display for Err {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let p = prog();
        match self {
            Err::Fatal(msg) => write!(f, "{p}: *** {msg}.  Stop."),
            Err::FatalAt(loc, msg) => write!(f, "{loc}: *** {msg}.  Stop."),
            Err::RecipeError { loc, target, code } => {
                write!(f, "{p}: *** [{loc}: {target}] Error {code}")
            }
            Err::RecipeInterrupt { loc, target } => {
                write!(f, "{p}: *** [{loc}: {target}] Interrupt")
            }
            Err::Warning(loc, msg) => write!(f, "{loc}: {msg}"),
        }
    }
}

impl From<std::io::Error> for Err {
    fn from(e: std::io::Error) -> Self {
        Err::Fatal(e.to_string())
    }
}

pub type Result<T> = std::result::Result<T, Err>;

pub fn fatal(msg: impl Into<String>) -> Err {
    Err::Fatal(msg.into())
}

pub fn fatal_at(loc: Loc, msg: impl Into<String>) -> Err {
    Err::FatalAt(loc, msg.into())
}

pub fn warning(loc: Loc, msg: impl Into<String>) -> Err {
    Err::Warning(loc, msg.into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fatal_format() {
        set_makelevel(0);
        assert_eq!(
            format!("{}", fatal("No targets specified")),
            "mk: *** No targets specified.  Stop."
        );
    }

    #[test]
    fn fatal_at_format() {
        let loc = Loc::new("Makefile", 5);
        assert_eq!(
            format!("{}", fatal_at(loc, "missing separator")),
            "Makefile:5: *** missing separator.  Stop."
        );
    }

    #[test]
    fn recipe_error_format() {
        set_makelevel(0);
        let e = Err::RecipeError {
            loc: Loc::new("Makefile", 10),
            target: "all".into(),
            code: 1,
        };
        assert_eq!(format!("{e}"), "mk: *** [Makefile:10: all] Error 1");
    }

    #[test]
    fn warning_format() {
        let e = warning(Loc::new("Makefile", 3), "overriding recipe for 'foo'");
        assert_eq!(format!("{e}"), "Makefile:3: overriding recipe for 'foo'");
    }
}
