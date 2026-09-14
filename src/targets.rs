//! Cleaning target catalog: pure data + template expansion.
//!
//! Mirrors mole's data/logic separation: `TARGETS` is a static table of
//! `%VAR%`-style path templates with `*` wildcards; expansion resolves them
//! against the current environment and filesystem. A template whose env var
//! is missing simply yields no targets, which keeps the table portable.

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

use crate::fsutil::is_link_or_reparse;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Category {
    Temp,
    System,
    Browser,
    Dev,
    Apps,
}

impl Category {
    pub const ALL: [Category; 5] = [
        Category::Temp,
        Category::System,
        Category::Browser,
        Category::Dev,
        Category::Apps,
    ];

    pub fn parse(s: &str) -> Option<Category> {
        match s.to_ascii_lowercase().as_str() {
            "temp" => Some(Category::Temp),
            "system" => Some(Category::System),
            "browser" | "browsers" => Some(Category::Browser),
            "dev" => Some(Category::Dev),
            "apps" | "app" => Some(Category::Apps),
            _ => None,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Category::Temp => "Temp",
            Category::System => "System",
            Category::Browser => "Browser",
            Category::Dev => "Dev",
            Category::Apps => "Apps",
        }
    }

    pub fn key(self) -> &'static str {
        match self {
            Category::Temp => "temp",
            Category::System => "system",
            Category::Browser => "browser",
            Category::Dev => "dev",
            Category::Apps => "apps",
        }
    }
}

/// How a resolved path should be cleaned.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Mode {
    /// Delete the children of the directory, keep the directory itself.
    Contents,
    /// Delete the matched entry itself (used for wildcard file targets).
    Entry,
}

pub struct TargetSpec {
    pub category: Category,
    pub name: &'static str,
    pub template: &'static str,
    pub mode: Mode,
}

const fn t(
    category: Category,
    name: &'static str,
    template: &'static str,
    mode: Mode,
) -> TargetSpec {
    TargetSpec {
        category,
        name,
        template,
        mode,
    }
}

/// The Windows cleaning knowledge base. Templates use `\` separators and
/// `%VAR%` env references; `*` matches one path segment (or part of one).
///
/// Deliberately excluded (expensive to re-download, mole whitelists them too):
/// `.m2/repository`, `.gradle/caches`, `.nuget/packages`, pnpm store.
pub const TARGETS: &[TargetSpec] = &[
    // -- Temp -------------------------------------------------------------
    t(Category::Temp, "User temp", "%TEMP%", Mode::Contents),
    t(
        Category::Temp,
        "Windows temp",
        "%SystemRoot%\\Temp",
        Mode::Contents,
    ),
    // -- System -----------------------------------------------------------
    t(
        Category::System,
        "Thumbnail cache",
        "%LOCALAPPDATA%\\Microsoft\\Windows\\Explorer\\thumbcache_*.db",
        Mode::Entry,
    ),
    t(
        Category::System,
        "Icon cache",
        "%LOCALAPPDATA%\\Microsoft\\Windows\\Explorer\\iconcache_*.db",
        Mode::Entry,
    ),
    t(
        Category::System,
        "Internet cache (INetCache)",
        "%LOCALAPPDATA%\\Microsoft\\Windows\\INetCache",
        Mode::Contents,
    ),
    t(
        Category::System,
        "Crash dumps",
        "%LOCALAPPDATA%\\CrashDumps",
        Mode::Contents,
    ),
    t(
        Category::System,
        "Error reports (WER archive)",
        "%LOCALAPPDATA%\\Microsoft\\Windows\\WER\\ReportArchive",
        Mode::Contents,
    ),
    t(
        Category::System,
        "Error reports (WER queue)",
        "%LOCALAPPDATA%\\Microsoft\\Windows\\WER\\ReportQueue",
        Mode::Contents,
    ),
    t(
        Category::System,
        "DirectX shader cache",
        "%LOCALAPPDATA%\\D3DSCache",
        Mode::Contents,
    ),
    t(
        Category::System,
        "NVIDIA DX shader cache",
        "%LOCALAPPDATA%\\NVIDIA\\DXCache",
        Mode::Contents,
    ),
    t(
        Category::System,
        "NVIDIA GL shader cache",
        "%LOCALAPPDATA%\\NVIDIA\\GLCache",
        Mode::Contents,
    ),
    t(
        Category::System,
        "AMD DX shader cache",
        "%LOCALAPPDATA%\\AMD\\DxCache",
        Mode::Contents,
    ),
    // -- Browser ----------------------------------------------------------
    t(
        Category::Browser,
        "Chrome cache",
        "%LOCALAPPDATA%\\Google\\Chrome\\User Data\\*\\Cache",
        Mode::Contents,
    ),
    t(
        Category::Browser,
        "Chrome code cache",
        "%LOCALAPPDATA%\\Google\\Chrome\\User Data\\*\\Code Cache",
        Mode::Contents,
    ),
    t(
        Category::Browser,
        "Chrome GPU cache",
        "%LOCALAPPDATA%\\Google\\Chrome\\User Data\\*\\GPUCache",
        Mode::Contents,
    ),
    t(
        Category::Browser,
        "Edge cache",
        "%LOCALAPPDATA%\\Microsoft\\Edge\\User Data\\*\\Cache",
        Mode::Contents,
    ),
    t(
        Category::Browser,
        "Edge code cache",
        "%LOCALAPPDATA%\\Microsoft\\Edge\\User Data\\*\\Code Cache",
        Mode::Contents,
    ),
    t(
        Category::Browser,
        "Edge GPU cache",
        "%LOCALAPPDATA%\\Microsoft\\Edge\\User Data\\*\\GPUCache",
        Mode::Contents,
    ),
    t(
        Category::Browser,
        "Firefox cache",
        "%LOCALAPPDATA%\\Mozilla\\Firefox\\Profiles\\*\\cache2",
        Mode::Contents,
    ),
    // -- Dev --------------------------------------------------------------
    t(
        Category::Dev,
        "npm cache",
        "%LOCALAPPDATA%\\npm-cache",
        Mode::Contents,
    ),
    t(
        Category::Dev,
        "pnpm cache",
        "%LOCALAPPDATA%\\pnpm-cache",
        Mode::Contents,
    ),
    t(
        Category::Dev,
        "Yarn cache",
        "%LOCALAPPDATA%\\Yarn\\Cache",
        Mode::Contents,
    ),
    t(
        Category::Dev,
        "pip cache",
        "%LOCALAPPDATA%\\pip\\cache",
        Mode::Contents,
    ),
    t(
        Category::Dev,
        "uv cache",
        "%LOCALAPPDATA%\\uv\\cache",
        Mode::Contents,
    ),
    t(
        Category::Dev,
        "Cargo registry cache",
        "%USERPROFILE%\\.cargo\\registry\\cache",
        Mode::Contents,
    ),
    t(
        Category::Dev,
        "Go build cache",
        "%LOCALAPPDATA%\\go-build",
        Mode::Contents,
    ),
    t(
        Category::Dev,
        "NuGet HTTP cache",
        "%LOCALAPPDATA%\\NuGet\\v3-cache",
        Mode::Contents,
    ),
    t(
        Category::Dev,
        "VS Code cache",
        "%APPDATA%\\Code\\Cache",
        Mode::Contents,
    ),
    t(
        Category::Dev,
        "VS Code cached data",
        "%APPDATA%\\Code\\CachedData",
        Mode::Contents,
    ),
    t(
        Category::Dev,
        "VS Code code cache",
        "%APPDATA%\\Code\\Code Cache",
        Mode::Contents,
    ),
    t(
        Category::Dev,
        "VS Code GPU cache",
        "%APPDATA%\\Code\\GPUCache",
        Mode::Contents,
    ),
    t(
        Category::Dev,
        "JetBrains caches",
        "%LOCALAPPDATA%\\JetBrains\\*\\caches",
        Mode::Contents,
    ),
    // -- Apps -------------------------------------------------------------
    t(
        Category::Apps,
        "Discord cache",
        "%APPDATA%\\discord\\Cache",
        Mode::Contents,
    ),
    t(
        Category::Apps,
        "Discord code cache",
        "%APPDATA%\\discord\\Code Cache",
        Mode::Contents,
    ),
    t(
        Category::Apps,
        "Discord GPU cache",
        "%APPDATA%\\discord\\GPUCache",
        Mode::Contents,
    ),
    t(
        Category::Apps,
        "Slack cache",
        "%APPDATA%\\Slack\\Cache",
        Mode::Contents,
    ),
    t(
        Category::Apps,
        "Slack GPU cache",
        "%APPDATA%\\Slack\\GPUCache",
        Mode::Contents,
    ),
    t(
        Category::Apps,
        "Teams cache (classic)",
        "%APPDATA%\\Microsoft\\Teams\\Cache",
        Mode::Contents,
    ),
    t(
        Category::Apps,
        "Spotify browser storage",
        "%LOCALAPPDATA%\\Spotify\\Storage",
        Mode::Contents,
    ),
];

/// A template resolved to a concrete, existing path.
pub struct ResolvedTarget {
    pub category: Category,
    pub name: &'static str,
    pub path: PathBuf,
    pub mode: Mode,
}

pub fn resolve_targets(only: Option<&[Category]>) -> Vec<ResolvedTarget> {
    let mut out = Vec::new();
    let mut seen = HashSet::new();
    for spec in TARGETS {
        if let Some(filter) = only {
            if !filter.contains(&spec.category) {
                continue;
            }
        }
        for path in expand_template(spec.template, &|v| std::env::var(v).ok()) {
            let key = path.to_string_lossy().replace('/', "\\").to_lowercase();
            if path.symlink_metadata().is_ok() && seen.insert(key) {
                out.push(ResolvedTarget {
                    category: spec.category,
                    name: spec.name,
                    path,
                    mode: spec.mode,
                });
            }
        }
    }
    out
}

/// Expand `%VAR%` references via `lookup`. Returns `None` if any var is unset.
pub fn expand_vars(template: &str, lookup: &dyn Fn(&str) -> Option<String>) -> Option<String> {
    let mut out = String::new();
    let mut rest = template;
    while let Some(start) = rest.find('%') {
        out.push_str(&rest[..start]);
        let after = &rest[start + 1..];
        let end = after.find('%')?;
        out.push_str(&lookup(&after[..end])?);
        rest = &after[end + 1..];
    }
    out.push_str(rest);
    Some(out)
}

/// Expand a template into concrete paths: env vars first, then `*` wildcards
/// against the filesystem. Non-wildcard templates pass through unchecked
/// (existence is verified by the caller).
pub fn expand_template(template: &str, lookup: &dyn Fn(&str) -> Option<String>) -> Vec<PathBuf> {
    let Some(expanded) = expand_vars(template, lookup) else {
        return Vec::new();
    };
    // Templates use `\`; convert for the host OS so tests run anywhere.
    let expanded = if cfg!(windows) {
        expanded
    } else {
        expanded.replace('\\', "/")
    };
    let sep = if cfg!(windows) { '\\' } else { '/' };
    if !expanded.contains('*') {
        return vec![PathBuf::from(expanded)];
    }
    let segs: Vec<&str> = expanded.split(sep).collect();
    let star = segs.iter().position(|s| s.contains('*')).unwrap();
    let base = PathBuf::from(segs[..star].join(&sep.to_string()));
    let mut results = Vec::new();
    walk_wildcards(&base, &segs[star..], &mut results);
    results
}

fn walk_wildcards(dir: &Path, segs: &[&str], results: &mut Vec<PathBuf>) {
    let Some((first, rest)) = segs.split_first() else {
        results.push(dir.to_path_buf());
        return;
    };
    if !first.contains('*') {
        walk_wildcards(&dir.join(first), rest, results);
        return;
    }
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if wildcard_match(first, &name) {
            let path = dir.join(&name);
            // A wildcard can otherwise walk through a junction into an
            // unrelated tree before the deletion safety gate sees it.
            if let Ok(meta) = path.symlink_metadata() {
                if !rest.is_empty() && is_link_or_reparse(&meta) {
                    continue;
                }
            }
            walk_wildcards(&path, rest, results);
        }
    }
}

/// Case-insensitive `*` glob match (star matches any run of characters).
pub fn wildcard_match(pattern: &str, text: &str) -> bool {
    let p: Vec<char> = pattern.to_lowercase().chars().collect();
    let t: Vec<char> = text.to_lowercase().chars().collect();
    let (mut pi, mut ti) = (0usize, 0usize);
    let mut star: Option<(usize, usize)> = None;
    while ti < t.len() {
        if pi < p.len() && (p[pi] == t[ti]) {
            pi += 1;
            ti += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star = Some((pi, ti));
            pi += 1;
        } else if let Some((sp, st)) = star {
            pi = sp + 1;
            ti = st + 1;
            star = Some((sp, st + 1));
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == '*' {
        pi += 1;
    }
    pi == p.len()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    #[test]
    fn wildcard_basics() {
        assert!(wildcard_match("thumbcache_*.db", "thumbcache_1024.db"));
        assert!(wildcard_match("thumbcache_*.db", "THUMBCACHE_idx.DB"));
        assert!(!wildcard_match("thumbcache_*.db", "iconcache_1024.db"));
        assert!(wildcard_match("*", "anything"));
        assert!(wildcard_match("a*b*c", "aXXbYYc"));
        assert!(!wildcard_match("a*b", "a"));
        assert!(wildcard_match("*cache*", "MyCacheDir"));
    }

    #[test]
    fn env_expansion() {
        let vars: HashMap<&str, &str> = [("LOCALAPPDATA", "/home/u/AppData/Local")].into();
        let lookup = |v: &str| vars.get(v).map(|s| s.to_string());
        assert_eq!(
            expand_vars("%LOCALAPPDATA%\\pip\\cache", &lookup),
            Some("/home/u/AppData/Local\\pip\\cache".to_string())
        );
        assert_eq!(expand_vars("%MISSING%\\x", &lookup), None);
    }

    #[test]
    fn template_wildcard_expansion() {
        let root = std::env::temp_dir().join(format!("dpan-test-{}", std::process::id()));
        let profiles = root.join("User Data");
        std::fs::create_dir_all(profiles.join("Default").join("Cache")).unwrap();
        std::fs::create_dir_all(profiles.join("Profile 1").join("Cache")).unwrap();
        std::fs::create_dir_all(profiles.join("Profile 2")).unwrap(); // no Cache

        let root_str = root.to_str().unwrap().to_string();
        let lookup = move |v: &str| (v == "ROOT").then(|| root_str.clone());
        let mut got = expand_template("%ROOT%\\User Data\\*\\Cache", &lookup);
        got.sort();
        // Non-wildcard tails are joined without existence checks; the caller
        // filters, so "Profile 2\Cache" may appear here only if it exists.
        let got_names: Vec<String> = got
            .iter()
            .filter(|p| p.exists())
            .map(|p| {
                p.parent()
                    .unwrap()
                    .file_name()
                    .unwrap()
                    .to_string_lossy()
                    .into_owned()
            })
            .collect();
        assert_eq!(got_names, vec!["Default", "Profile 1"]);
        std::fs::remove_dir_all(&root).unwrap();
    }
}
