//! Safety gate: every deletion must pass through `Safety::check`.
//!
//! Ports mole's file_ops triple gate: path validation (absolute, no `..`,
//! minimum depth), a protected-root deny list, an allowed-base allowlist
//! (fail-closed), and a user whitelist for opt-out paths.

use std::env;
use std::fs;
use std::path::{Component, Path, PathBuf};

use crate::targets::wildcard_match;

pub struct Safety {
    /// Paths that must never be deleted themselves (their children may be,
    /// if under an allowed base).
    protected_roots: Vec<PathBuf>,
    /// Deletions must happen strictly inside one of these bases.
    allowed_bases: Vec<PathBuf>,
    /// Subtrees that are entirely off-limits (e.g. Program Files).
    denied_trees: Vec<PathBuf>,
    /// User whitelist patterns (skip, not an error).
    whitelist: Vec<String>,
}

impl Safety {
    pub fn load() -> Safety {
        let var = |v: &str| env::var(v).ok().map(PathBuf::from);

        let mut protected_roots = Vec::new();
        for v in [
            "USERPROFILE",
            "APPDATA",
            "LOCALAPPDATA",
            "SystemRoot",
            "ProgramData",
            "HOME",
        ] {
            if let Some(p) = var(v) {
                protected_roots.push(p);
            }
        }

        let mut allowed_bases = Vec::new();
        for v in ["USERPROFILE", "APPDATA", "LOCALAPPDATA", "TEMP", "TMP"] {
            if let Some(p) = var(v) {
                allowed_bases.push(p);
            }
        }
        if let Some(root) = var("SystemRoot") {
            allowed_bases.push(root.join("Temp"));
        }

        let mut denied_trees = Vec::new();
        for v in ["ProgramFiles", "ProgramFiles(x86)", "ProgramData"] {
            if let Some(p) = var(v) {
                denied_trees.push(p);
            }
        }
        // Everything under SystemRoot except SystemRoot\Temp.
        if let Some(root) = var("SystemRoot") {
            for sub in ["System32", "SysWOW64", "WinSxS", "servicing"] {
                denied_trees.push(root.join(sub));
            }
        }

        Safety {
            protected_roots,
            allowed_bases,
            denied_trees,
            whitelist: load_whitelist(),
        }
    }

    #[cfg(test)]
    pub fn for_test(allowed_bases: Vec<PathBuf>, whitelist: Vec<String>) -> Safety {
        Safety {
            protected_roots: Vec::new(),
            allowed_bases,
            denied_trees: Vec::new(),
            whitelist,
        }
    }

    /// Hard safety check. `Err` means the path must not be deleted.
    pub fn check(&self, path: &Path) -> Result<(), &'static str> {
        if !path.is_absolute() {
            return Err("not an absolute path");
        }
        let mut normals = 0usize;
        for c in path.components() {
            match c {
                Component::ParentDir => return Err("contains '..'"),
                Component::Normal(_) => normals += 1,
                _ => {}
            }
        }
        if normals < 2 {
            return Err("path too shallow");
        }
        for root in &self.protected_roots {
            if same_path(path, root) {
                return Err("protected root");
            }
        }
        for tree in &self.denied_trees {
            if path_starts_with(path, tree) {
                return Err("inside a protected system tree");
            }
        }
        // Fail closed: no allowed bases means nothing is deletable.
        if !self
            .allowed_bases
            .iter()
            .any(|b| path_starts_with(path, b) && !same_path(path, b))
        {
            return Err("outside allowed base directories");
        }
        Ok(())
    }

    /// User whitelist: entries without `*` protect the path and its subtree;
    /// entries with `*` are matched as globs against the full path.
    pub fn is_whitelisted(&self, path: &Path) -> bool {
        let text = path.to_string_lossy();
        self.whitelist.iter().any(|pat| {
            if pat.contains('*') {
                wildcard_match(pat, &text)
            } else {
                let base = Path::new(pat);
                same_path(path, base) || path_starts_with(path, base)
            }
        })
    }
}

/// `%APPDATA%\dustpan\whitelist.txt` on Windows, `~/.config/dustpan/whitelist.txt`
/// elsewhere. One path or glob per line, `#` comments, `~` and `%VAR%` expanded.
pub fn whitelist_path() -> Option<PathBuf> {
    if let Ok(appdata) = env::var("APPDATA") {
        return Some(PathBuf::from(appdata).join("dustpan").join("whitelist.txt"));
    }
    env::var("HOME").ok().map(|h| {
        PathBuf::from(h)
            .join(".config")
            .join("dustpan")
            .join("whitelist.txt")
    })
}

fn load_whitelist() -> Vec<String> {
    let Some(path) = whitelist_path() else {
        return Vec::new();
    };
    let Ok(content) = fs::read_to_string(&path) else {
        return Vec::new();
    };
    content
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .filter_map(expand_whitelist_entry)
        .collect()
}

fn expand_whitelist_entry(line: &str) -> Option<String> {
    let line = if let Some(rest) = line.strip_prefix("~") {
        let home = env::var("USERPROFILE").or_else(|_| env::var("HOME")).ok()?;
        format!("{home}{rest}")
    } else {
        line.to_string()
    };
    crate::targets::expand_vars(&line, &|v| env::var(v).ok())
}

/// Lowercased components for case-insensitive comparison (Windows paths).
fn norm(p: &Path) -> Vec<String> {
    p.components()
        .map(|c| c.as_os_str().to_string_lossy().to_lowercase())
        .collect()
}

fn same_path(a: &Path, b: &Path) -> bool {
    norm(a) == norm(b)
}

fn path_starts_with(child: &Path, base: &Path) -> bool {
    let c = norm(child);
    let b = norm(base);
    !b.is_empty() && c.len() >= b.len() && c[..b.len()] == b[..]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base() -> PathBuf {
        if cfg!(windows) {
            PathBuf::from("C:\\Users\\test\\AppData\\Local")
        } else {
            PathBuf::from("/home/test/AppData/Local")
        }
    }

    #[test]
    fn rejects_unsafe_paths() {
        let s = Safety::for_test(vec![base()], vec![]);
        assert!(s.check(Path::new("relative/path")).is_err());
        assert!(s.check(&base().join("..").join("x")).is_err());
        // The allowed base itself is not deletable.
        assert!(s.check(&base()).is_err());
        // Outside any allowed base.
        let outside = if cfg!(windows) {
            PathBuf::from("C:\\Other\\place\\file")
        } else {
            PathBuf::from("/other/place/file")
        };
        assert!(s.check(&outside).is_err());
        // Inside the base is fine.
        assert!(s.check(&base().join("pip").join("cache").join("x")).is_ok());
    }

    #[test]
    fn fail_closed_without_bases() {
        let s = Safety::for_test(vec![], vec![]);
        assert!(s.check(&base().join("pip").join("cache")).is_err());
    }

    #[test]
    fn case_insensitive_containment() {
        let s = Safety::for_test(vec![base()], vec![]);
        let mixed = base().join("PIP").join("Cache").join("wheel");
        assert!(s.check(&mixed).is_ok());
    }

    #[test]
    fn whitelist_glob_and_subtree() {
        let wl = vec![
            base().join("keepme").to_string_lossy().into_owned(),
            "*important*".to_string(),
        ];
        let s = Safety::for_test(vec![base()], wl);
        assert!(s.is_whitelisted(&base().join("keepme")));
        assert!(s.is_whitelisted(&base().join("keepme").join("sub").join("f")));
        assert!(s.is_whitelisted(&base().join("very-Important-data")));
        assert!(!s.is_whitelisted(&base().join("junk")));
    }
}
