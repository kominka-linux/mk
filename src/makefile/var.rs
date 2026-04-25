use std::collections::HashMap;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Flavor {
    /// `=` — expanded at use time
    Recursive,
    /// `:=` — expanded at definition time (stored already-expanded)
    Simple,
    /// `?=` — only set if undefined
    Conditional,
    /// `+=` — append
    Append,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Origin {
    Default = 0,
    Environment = 1,
    File = 2,
    /// `override` in file
    Override = 3,
    CommandLine = 4,
    /// Automatic variables ($@, $<, …)
    Automatic = 5,
}

impl std::fmt::Display for Origin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Origin::Default => "default",
            Origin::Environment => "environment",
            Origin::File => "file",
            Origin::Override => "override",
            Origin::CommandLine => "command line",
            Origin::Automatic => "automatic",
        })
    }
}

#[derive(Debug, Clone)]
pub struct Var {
    pub flavor: Flavor,
    pub origin: Origin,
    /// Raw (possibly unexpanded) value.
    pub raw: String,
    /// `Some(true)` = explicitly exported, `Some(false)` = explicitly unexported,
    /// `None` = inherits from `.EXPORT_ALL_VARIABLES` / default.
    pub exported: Option<bool>,
}

impl Var {
    pub fn new(flavor: Flavor, origin: Origin, raw: impl Into<String>) -> Self {
        Self { flavor, origin, raw: raw.into(), exported: None }
    }
}

pub struct VarTable {
    vars: HashMap<String, Var>,
    /// Set by `.EXPORT_ALL_VARIABLES`
    pub export_all: bool,
}

impl Default for VarTable {
    fn default() -> Self {
        Self::new()
    }
}

impl VarTable {
    pub fn new() -> Self {
        Self { vars: HashMap::new(), export_all: false }
    }

    /// Look up a variable by name.
    pub fn get(&self, name: &str) -> Option<&Var> {
        self.vars.get(name)
    }

    /// Set a variable, respecting precedence.
    ///
    /// Lower-origin assignments cannot overwrite higher-origin ones,
    /// unless `force` is set (used internally for automatic vars).
    pub fn set(&mut self, name: impl Into<String>, var: Var) {
        let name = name.into();
        if let Some(existing) = self.vars.get(&name) {
            // CommandLine always wins; can't be overwritten by File/Environment
            if existing.origin > var.origin
                && existing.origin != Origin::Override
            {
                // If existing is CommandLine and new is not, skip
                if existing.origin == Origin::CommandLine
                    && var.origin < Origin::CommandLine
                {
                    return;
                }
            }
            // Override origin in file can overwrite File but not CommandLine
            if var.origin == Origin::Override
                && existing.origin == Origin::CommandLine
            {
                return;
            }
        }
        self.vars.insert(name, var);
    }

    /// Undefine a variable (respects origin precedence).
    pub fn undefine(&mut self, name: &str, origin: Origin) {
        if let Some(existing) = self.vars.get(name) {
            if existing.origin <= origin {
                self.vars.remove(name);
            }
        }
    }

    /// Raw value, or empty string if undefined.
    pub fn raw(&self, name: &str) -> &str {
        self.vars.get(name).map(|v| v.raw.as_str()).unwrap_or("")
    }

    /// Flavor for `$(flavor NAME)`.
    pub fn flavor_str(&self, name: &str) -> &'static str {
        match self.vars.get(name) {
            None => "undefined",
            Some(v) => match v.flavor {
                Flavor::Simple => "simple",
                _ => "recursive",
            },
        }
    }

    /// Origin string for `$(origin NAME)`.
    pub fn origin_str(&self, name: &str) -> String {
        match self.vars.get(name) {
            None => "undefined".into(),
            Some(v) => v.origin.to_string(),
        }
    }

    /// Set the export flag on a variable by name.
    pub fn set_exported(&mut self, name: &str, exported: Option<bool>) {
        if let Some(v) = self.vars.get_mut(name) {
            v.exported = exported;
        }
    }

    /// Iterate over all variables (for -p).
    pub fn iter(&self) -> impl Iterator<Item = (&str, &Var)> {
        self.vars.iter().map(|(k, v)| (k.as_str(), v))
    }

    /// Collect environment-exported variables into a map.
    pub fn env_exports(&self) -> HashMap<String, String> {
        let mut out = HashMap::new();
        for (name, var) in &self.vars {
            let should_export = match var.exported {
                Some(true) => true,
                Some(false) => false,
                None => self.export_all && var.origin != Origin::Default,
            };
            if should_export {
                out.insert(name.clone(), var.raw.clone());
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn basic_set_get() {
        let mut t = VarTable::new();
        t.set("CC", Var::new(Flavor::Simple, Origin::File, "gcc"));
        assert_eq!(t.raw("CC"), "gcc");
    }

    #[test]
    fn command_line_wins_over_file() {
        let mut t = VarTable::new();
        t.set("CC", Var::new(Flavor::Simple, Origin::CommandLine, "clang"));
        t.set("CC", Var::new(Flavor::Simple, Origin::File, "gcc"));
        assert_eq!(t.raw("CC"), "clang");
    }

    #[test]
    fn file_overwrites_environment() {
        let mut t = VarTable::new();
        t.set("CC", Var::new(Flavor::Simple, Origin::Environment, "env-cc"));
        t.set("CC", Var::new(Flavor::Simple, Origin::File, "file-cc"));
        assert_eq!(t.raw("CC"), "file-cc");
    }

    #[test]
    fn override_blocked_by_command_line() {
        let mut t = VarTable::new();
        t.set("CC", Var::new(Flavor::Simple, Origin::CommandLine, "clang"));
        t.set("CC", Var::new(Flavor::Simple, Origin::Override, "override-cc"));
        assert_eq!(t.raw("CC"), "clang");
    }

    #[test]
    fn undefine_file_var() {
        let mut t = VarTable::new();
        t.set("X", Var::new(Flavor::Simple, Origin::File, "foo"));
        t.undefine("X", Origin::File);
        assert_eq!(t.raw("X"), "");
        assert!(t.get("X").is_none());
    }

    #[test]
    fn undefine_blocked_by_cmdline() {
        let mut t = VarTable::new();
        t.set("X", Var::new(Flavor::Simple, Origin::CommandLine, "foo"));
        t.undefine("X", Origin::File);
        assert_eq!(t.raw("X"), "foo");
    }

    #[test]
    fn flavor_undefined() {
        let t = VarTable::new();
        assert_eq!(t.flavor_str("NOEXIST"), "undefined");
    }

    #[test]
    fn flavor_simple() {
        let mut t = VarTable::new();
        t.set("X", Var::new(Flavor::Simple, Origin::File, "v"));
        assert_eq!(t.flavor_str("X"), "simple");
    }

    #[test]
    fn flavor_recursive() {
        let mut t = VarTable::new();
        t.set("X", Var::new(Flavor::Recursive, Origin::File, "v"));
        assert_eq!(t.flavor_str("X"), "recursive");
    }

    #[test]
    fn origin_str_file() {
        let mut t = VarTable::new();
        t.set("X", Var::new(Flavor::Simple, Origin::File, "v"));
        assert_eq!(t.origin_str("X"), "file");
    }

    #[test]
    fn origin_str_cmdline() {
        let mut t = VarTable::new();
        t.set("X", Var::new(Flavor::Simple, Origin::CommandLine, "v"));
        assert_eq!(t.origin_str("X"), "command line");
    }
}
