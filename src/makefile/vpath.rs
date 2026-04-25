use std::path::{Path, PathBuf};

/// Per-pattern search path entry.
struct VpathEntry {
    pattern: String,
    dirs: Vec<PathBuf>,
}

/// Manages both the `VPATH` variable (all-files) and `vpath` directive (per-pattern).
pub struct VpathTable {
    /// From the `VPATH` variable — searched for all files.
    global: Vec<PathBuf>,
    /// From `vpath pattern dirs` directives.
    patterns: Vec<VpathEntry>,
}

impl Default for VpathTable {
    fn default() -> Self {
        Self::new()
    }
}

impl VpathTable {
    pub fn new() -> Self {
        Self { global: Vec::new(), patterns: Vec::new() }
    }

    /// Update the global search path from the `VPATH` variable value.
    /// Colons and spaces both separate directories.
    pub fn set_vpath_var(&mut self, value: &str) {
        self.global = value
            .split(|c| c == ':' || c == ' ')
            .filter(|s| !s.is_empty())
            .map(PathBuf::from)
            .collect();
    }

    /// Add a `vpath PATTERN DIRS` directive.
    pub fn add_pattern(&mut self, pattern: &str, dirs_str: &str) {
        let dirs: Vec<PathBuf> = dirs_str
            .split(|c| c == ':' || c == ' ')
            .filter(|s| !s.is_empty())
            .map(PathBuf::from)
            .collect();

        // Update existing entry or add new one
        if let Some(entry) = self.patterns.iter_mut().find(|e| e.pattern == pattern) {
            entry.dirs = dirs;
        } else {
            self.patterns.push(VpathEntry { pattern: pattern.to_string(), dirs });
        }
    }

    /// `vpath PATTERN` with no dirs clears that pattern's entries.
    pub fn clear_pattern(&mut self, pattern: &str) {
        self.patterns.retain(|e| e.pattern != pattern);
    }

    /// `vpath` with no args clears all pattern entries.
    pub fn clear_all(&mut self) {
        self.patterns.clear();
    }

    /// Search for `name` in vpath directories.
    /// Returns the found path, or `None` if the file doesn't exist anywhere in vpath.
    /// Note: only searches if `name` does NOT exist in the current directory.
    pub fn find(&self, name: &str) -> Option<PathBuf> {
        let p = Path::new(name);
        if p.exists() {
            return None; // current dir takes priority
        }

        // Check pattern-specific paths first (in order added)
        for entry in &self.patterns {
            if crate::makefile::rule::match_pattern(name, &entry.pattern).is_some() {
                for dir in &entry.dirs {
                    let candidate = dir.join(name);
                    if candidate.exists() {
                        return Some(candidate);
                    }
                }
            }
        }

        // Fall back to global VPATH
        for dir in &self.global {
            let candidate = dir.join(name);
            if candidate.exists() {
                return Some(candidate);
            }
        }

        None
    }

    /// Find a file, returning the actual path to use (found path or original name).
    /// Also returns whether it was found via vpath (for $< etc.).
    pub fn resolve(&self, name: &str) -> PathBuf {
        self.find(name).unwrap_or_else(|| PathBuf::from(name))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn set_vpath_colon_separated() {
        let mut vp = VpathTable::new();
        vp.set_vpath_var("src:lib:include");
        assert_eq!(vp.global, vec![
            PathBuf::from("src"),
            PathBuf::from("lib"),
            PathBuf::from("include"),
        ]);
    }

    #[test]
    fn set_vpath_space_separated() {
        let mut vp = VpathTable::new();
        vp.set_vpath_var("src lib");
        assert_eq!(vp.global, vec![PathBuf::from("src"), PathBuf::from("lib")]);
    }

    #[test]
    fn add_and_clear_pattern() {
        let mut vp = VpathTable::new();
        vp.add_pattern("%.c", "src");
        assert_eq!(vp.patterns.len(), 1);
        vp.clear_pattern("%.c");
        assert!(vp.patterns.is_empty());
    }

    #[test]
    fn clear_all() {
        let mut vp = VpathTable::new();
        vp.add_pattern("%.c", "src");
        vp.add_pattern("%.h", "include");
        vp.clear_all();
        assert!(vp.patterns.is_empty());
    }

    #[test]
    fn resolve_existing_file_ignored() {
        // A file that exists locally should not be looked up in vpath
        let mut vp = VpathTable::new();
        vp.set_vpath_var("/nonexistent-dir");
        // "." exists as the current dir, so any existing file won't be overridden
        let result = vp.find(".");
        assert!(result.is_none()); // "." exists locally
    }
}
