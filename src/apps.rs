//! Installed application inventory (`dpan apps`), modeled on how Geek
//! Uninstaller discovers software:
//!
//! 1. Registry uninstall keys (HKLM 64-bit, HKLM WOW6432Node, HKCU) — the
//!    same three hives every uninstaller UI reads, with the standard hide
//!    rules (SystemComponent, patch entries, nameless keys).
//! 2. Store/UWP packages via PowerShell `Get-AppxPackage` (opt-in, slow).
//! 3. A portable-app scanner for unzip-and-run software the registry does
//!    not know about: well-known directories plus user-configured roots.
//!
//! Read-only: this module never modifies the registry or the filesystem.

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::UNIX_EPOCH;

use crate::clean::iso_from_unix;
use crate::ui::{fmt_size, Style};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[cfg_attr(not(windows), allow(dead_code))] // registry variants unused off-Windows
pub enum Source {
    Machine,
    Machine32,
    User,
    Store,
    Portable,
}

impl Source {
    pub fn label(self) -> &'static str {
        match self {
            Source::Machine => "system",
            Source::Machine32 => "sys32",
            Source::User => "user",
            Source::Store => "store",
            Source::Portable => "portable",
        }
    }
}

#[derive(Debug)]
pub struct AppEntry {
    pub name: String,
    pub version: String,
    pub publisher: String,
    pub location: String,
    pub uninstall_string: String,
    pub size_bytes: Option<u64>,
    /// ISO date (YYYY-MM-DD) when known.
    pub install_date: Option<String>,
    pub source: Source,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum SortKey {
    Name,
    Size,
    Date,
}

impl SortKey {
    pub fn parse(s: &str) -> Option<SortKey> {
        match s.to_ascii_lowercase().as_str() {
            "name" => Some(SortKey::Name),
            "size" => Some(SortKey::Size),
            "date" => Some(SortKey::Date),
            _ => None,
        }
    }
}

pub struct AppsOptions {
    pub json: bool,
    pub store: bool,
    pub verbose: bool,
    pub no_color: bool,
    pub sort: SortKey,
    pub filter: Option<String>,
}

impl Default for AppsOptions {
    fn default() -> Self {
        AppsOptions {
            json: false,
            store: false,
            verbose: false,
            no_color: false,
            sort: SortKey::Name,
            filter: None,
        }
    }
}

pub fn run(opts: &AppsOptions) -> ExitCode {
    let mut apps = collect(opts.store);
    if let Some(filter) = &opts.filter {
        let needle = filter.to_lowercase();
        apps.retain(|a| a.name.to_lowercase().contains(&needle));
    }
    sort_apps(&mut apps, opts.sort);

    if opts.json {
        print_json(&apps);
        return ExitCode::SUCCESS;
    }
    let style = Style::auto(opts.no_color);
    print_table(&apps, &style, opts.verbose);
    if !cfg!(windows) {
        println!(
            "{}",
            style.dim("(registry and Store sources are Windows-only; only portable scan ran)")
        );
    }
    ExitCode::SUCCESS
}

fn collect(include_store: bool) -> Vec<AppEntry> {
    let mut apps = registry::collect();
    if include_store {
        apps.extend(store_apps());
    }
    // Dedupe portable candidates against everything found so far.
    let known_locations: HashSet<String> = apps
        .iter()
        .filter(|a| !a.location.is_empty())
        .map(|a| norm_path(&a.location))
        .collect();
    let known_names: HashSet<String> = apps.iter().map(|a| a.name.to_lowercase()).collect();
    apps.extend(
        portable_apps()
            .into_iter()
            .filter(|p| !known_locations.contains(&norm_path(&p.location)))
            .filter(|p| !known_names.contains(&p.name.to_lowercase())),
    );
    apps
}

fn norm_path(p: &str) -> String {
    p.to_lowercase()
        .replace('/', "\\")
        .trim_end_matches('\\')
        .to_string()
}

pub fn sort_apps(apps: &mut [AppEntry], key: SortKey) {
    match key {
        SortKey::Name => apps.sort_by_key(|a| a.name.to_lowercase()),
        SortKey::Size => apps.sort_by(|a, b| {
            b.size_bytes
                .unwrap_or(0)
                .cmp(&a.size_bytes.unwrap_or(0))
                .then(a.name.to_lowercase().cmp(&b.name.to_lowercase()))
        }),
        SortKey::Date => apps.sort_by(|a, b| {
            b.install_date
                .clone()
                .unwrap_or_default()
                .cmp(&a.install_date.clone().unwrap_or_default())
                .then(a.name.to_lowercase().cmp(&b.name.to_lowercase()))
        }),
    }
}

/// Geek-style hide rules for raw registry entries.
/// `system_component`: SystemComponent DWORD == 1.
/// `parent`: ParentKeyName / ParentDisplayName present (patch entries).
/// `release_type`: ReleaseType value.
#[cfg_attr(not(windows), allow(dead_code))] // only the registry reader calls this
pub fn should_hide(name: &str, system_component: bool, parent: bool, release_type: &str) -> bool {
    if name.trim().is_empty() || system_component || parent {
        return true;
    }
    matches!(
        release_type,
        "Security Update" | "Update Rollup" | "Hotfix" | "ServicePack"
    )
}

/// Registry InstallDate is "YYYYMMDD"; normalize to "YYYY-MM-DD".
#[cfg_attr(not(windows), allow(dead_code))] // only the registry reader calls this
pub fn fmt_install_date(raw: &str) -> Option<String> {
    let raw = raw.trim();
    if raw.len() != 8 || !raw.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    Some(format!("{}-{}-{}", &raw[..4], &raw[4..6], &raw[6..8]))
}

// ---------------------------------------------------------------------------
// Source 1: registry uninstall keys (Windows only, advapi32 FFI).
// ---------------------------------------------------------------------------

#[cfg(windows)]
mod registry {
    use super::{fmt_install_date, should_hide, AppEntry, Source};
    use std::ffi::c_void;
    use std::os::windows::ffi::OsStrExt;

    type Hkey = *mut c_void;
    const HKEY_LOCAL_MACHINE: isize = 0x8000_0002u32 as i32 as isize;
    const HKEY_CURRENT_USER: isize = 0x8000_0001u32 as i32 as isize;
    const KEY_READ: u32 = 0x2_0019;
    const ERROR_SUCCESS: i32 = 0;
    const ERROR_NO_MORE_ITEMS: i32 = 259;
    const REG_SZ: u32 = 1;
    const REG_EXPAND_SZ: u32 = 2;
    const REG_DWORD: u32 = 4;

    #[link(name = "advapi32")]
    extern "system" {
        fn RegOpenKeyExW(
            key: Hkey,
            sub_key: *const u16,
            options: u32,
            desired: u32,
            result: *mut Hkey,
        ) -> i32;
        fn RegEnumKeyExW(
            key: Hkey,
            index: u32,
            name: *mut u16,
            name_len: *mut u32,
            reserved: *mut u32,
            class: *mut u16,
            class_len: *mut u32,
            last_write: *mut c_void,
        ) -> i32;
        fn RegQueryValueExW(
            key: Hkey,
            value_name: *const u16,
            reserved: *mut u32,
            value_type: *mut u32,
            data: *mut u8,
            data_len: *mut u32,
        ) -> i32;
        fn RegCloseKey(key: Hkey) -> i32;
    }

    fn wide(s: &str) -> Vec<u16> {
        std::ffi::OsStr::new(s)
            .encode_wide()
            .chain(Some(0))
            .collect()
    }

    /// RAII key handle.
    struct Key(Hkey);

    impl Key {
        fn open(root: isize, path: &str) -> Option<Key> {
            let mut out: Hkey = std::ptr::null_mut();
            let rc =
                unsafe { RegOpenKeyExW(root as Hkey, wide(path).as_ptr(), 0, KEY_READ, &mut out) };
            (rc == ERROR_SUCCESS).then(|| Key(out))
        }

        fn subkeys(&self) -> Vec<String> {
            let mut names = Vec::new();
            let mut index = 0u32;
            loop {
                let mut buf = [0u16; 256];
                let mut len = buf.len() as u32;
                let rc = unsafe {
                    RegEnumKeyExW(
                        self.0,
                        index,
                        buf.as_mut_ptr(),
                        &mut len,
                        std::ptr::null_mut(),
                        std::ptr::null_mut(),
                        std::ptr::null_mut(),
                        std::ptr::null_mut(),
                    )
                };
                if rc == ERROR_NO_MORE_ITEMS {
                    break;
                }
                if rc == ERROR_SUCCESS {
                    names.push(String::from_utf16_lossy(&buf[..len as usize]));
                }
                index += 1;
            }
            names
        }

        fn string_value(&self, name: &str) -> String {
            let mut ty = 0u32;
            let mut buf = vec![0u8; 8192];
            let mut len = buf.len() as u32;
            let rc = unsafe {
                RegQueryValueExW(
                    self.0,
                    wide(name).as_ptr(),
                    std::ptr::null_mut(),
                    &mut ty,
                    buf.as_mut_ptr(),
                    &mut len,
                )
            };
            if rc != ERROR_SUCCESS || (ty != REG_SZ && ty != REG_EXPAND_SZ) {
                return String::new();
            }
            let units: Vec<u16> = buf[..len as usize]
                .chunks_exact(2)
                .map(|c| u16::from_le_bytes([c[0], c[1]]))
                .collect();
            String::from_utf16_lossy(&units)
                .trim_end_matches('\0')
                .to_string()
        }

        fn dword_value(&self, name: &str) -> Option<u32> {
            let mut ty = 0u32;
            let mut buf = [0u8; 4];
            let mut len = buf.len() as u32;
            let rc = unsafe {
                RegQueryValueExW(
                    self.0,
                    wide(name).as_ptr(),
                    std::ptr::null_mut(),
                    &mut ty,
                    buf.as_mut_ptr(),
                    &mut len,
                )
            };
            (rc == ERROR_SUCCESS && ty == REG_DWORD).then(|| u32::from_le_bytes(buf))
        }
    }

    impl Drop for Key {
        fn drop(&mut self) {
            unsafe { RegCloseKey(self.0) };
        }
    }

    const UNINSTALL: &str = r"SOFTWARE\Microsoft\Windows\CurrentVersion\Uninstall";
    const UNINSTALL_32: &str = r"SOFTWARE\WOW6432Node\Microsoft\Windows\CurrentVersion\Uninstall";

    pub fn collect() -> Vec<AppEntry> {
        let mut apps = Vec::new();
        let hives = [
            (HKEY_LOCAL_MACHINE, UNINSTALL, Source::Machine),
            (HKEY_LOCAL_MACHINE, UNINSTALL_32, Source::Machine32),
            (HKEY_CURRENT_USER, UNINSTALL, Source::User),
        ];
        for (root, path, source) in hives {
            let Some(hive) = Key::open(root, path) else {
                continue;
            };
            for sub in hive.subkeys() {
                let Some(key) = Key::open(root, &format!("{path}\\{sub}")) else {
                    continue;
                };
                let name = key.string_value("DisplayName");
                let system_component = key.dword_value("SystemComponent") == Some(1);
                let parent = !key.string_value("ParentKeyName").is_empty()
                    || !key.string_value("ParentDisplayName").is_empty();
                let release_type = key.string_value("ReleaseType");
                if should_hide(&name, system_component, parent, &release_type) {
                    continue;
                }
                apps.push(AppEntry {
                    name,
                    version: key.string_value("DisplayVersion"),
                    publisher: key.string_value("Publisher"),
                    location: key.string_value("InstallLocation"),
                    uninstall_string: key.string_value("UninstallString"),
                    // EstimatedSize is stored in KB
                    size_bytes: key.dword_value("EstimatedSize").map(|kb| kb as u64 * 1024),
                    install_date: fmt_install_date(&key.string_value("InstallDate")),
                    source,
                });
            }
        }
        apps
    }
}

#[cfg(not(windows))]
mod registry {
    use super::AppEntry;

    pub fn collect() -> Vec<AppEntry> {
        Vec::new() // registry only exists on Windows
    }
}

// ---------------------------------------------------------------------------
// Source 2: Store/UWP packages via PowerShell (opt-in with --store).
// ---------------------------------------------------------------------------

fn store_apps() -> Vec<AppEntry> {
    if !cfg!(windows) {
        return Vec::new();
    }
    let output = std::process::Command::new("powershell")
        .args([
            "-NoProfile",
            "-Command",
            "Get-AppxPackage | Where-Object { -not $_.IsFramework } | \
             ForEach-Object { \"$($_.Name)`t$($_.Version)`t$($_.Publisher)`t$($_.InstallLocation)\" }",
        ])
        .output();
    let Ok(out) = output else {
        return Vec::new();
    };
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|line| {
            let mut parts = line.split('\t');
            let name = parts.next()?.trim().to_string();
            if name.is_empty() {
                return None;
            }
            Some(AppEntry {
                name,
                version: parts.next().unwrap_or("").to_string(),
                publisher: parts.next().unwrap_or("").to_string(),
                location: parts.next().unwrap_or("").to_string(),
                uninstall_string: String::new(),
                size_bytes: None,
                install_date: None,
                source: Source::Store,
            })
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Source 3: portable-app scanner (pure filesystem, works everywhere).
// ---------------------------------------------------------------------------

/// Directories where unzip-and-run apps typically live. Extra roots can be
/// added in `%APPDATA%\dustpan\portable_dirs.txt` (one per line).
fn portable_roots() -> Vec<PathBuf> {
    let mut roots = Vec::new();
    let var = |v: &str| std::env::var(v).ok().map(PathBuf::from);
    if let Some(local) = var("LOCALAPPDATA") {
        roots.push(local.join("Programs"));
    }
    if let Some(profile) = var("USERPROFILE") {
        roots.push(profile.join("scoop").join("apps"));
        roots.push(profile.join("PortableApps"));
    }
    if cfg!(windows) {
        roots.push(PathBuf::from(r"C:\PortableApps"));
    }
    if let Some(appdata) = var("APPDATA") {
        let config = appdata.join("dustpan").join("portable_dirs.txt");
        if let Ok(content) = fs::read_to_string(&config) {
            roots.extend(
                content
                    .lines()
                    .map(str::trim)
                    .filter(|l| !l.is_empty() && !l.starts_with('#'))
                    .map(PathBuf::from),
            );
        }
    }
    roots
}

fn portable_apps() -> Vec<AppEntry> {
    let mut apps = Vec::new();
    for root in portable_roots() {
        let Ok(rd) = fs::read_dir(&root) else {
            continue;
        };
        let is_scoop = root.ends_with("scoop/apps") || root.ends_with(r"scoop\apps");
        for entry in rd.flatten() {
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }
            if !contains_exe(&path, 2) {
                continue;
            }
            let name = entry.file_name().to_string_lossy().into_owned();
            if is_scoop && name == "scoop" {
                continue; // scoop's own runtime
            }
            let version = if is_scoop {
                scoop_version(&path).unwrap_or_default()
            } else {
                String::new()
            };
            let install_date = path
                .metadata()
                .and_then(|m| m.modified())
                .ok()
                .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                .map(|d| iso_from_unix(d.as_secs())[..10].to_string());
            apps.push(AppEntry {
                name,
                version,
                publisher: String::new(),
                location: path.display().to_string(),
                uninstall_string: String::new(),
                size_bytes: Some(crate::scan::entry_size(&path)),
                install_date,
                source: Source::Portable,
            });
        }
    }
    apps
}

/// A directory counts as a portable app if it holds an .exe within `depth`
/// levels (scoop layout is apps/<name>/<version>/*.exe, hence depth 2).
fn contains_exe(dir: &Path, depth: u32) -> bool {
    let Ok(rd) = fs::read_dir(dir) else {
        return false;
    };
    let mut subdirs = Vec::new();
    for entry in rd.flatten() {
        let path = entry.path();
        if path.is_file() {
            if path
                .extension()
                .is_some_and(|e| e.eq_ignore_ascii_case("exe"))
            {
                return true;
            }
        } else if depth > 0 && path.is_dir() {
            subdirs.push(path);
        }
    }
    subdirs.iter().any(|d| contains_exe(d, depth - 1))
}

/// scoop keeps apps/<name>/<version>/ plus a `current` junction; the highest
/// version-named directory is the installed version.
fn scoop_version(app_dir: &Path) -> Option<String> {
    let mut versions: Vec<String> = fs::read_dir(app_dir)
        .ok()?
        .flatten()
        .filter(|e| e.path().is_dir())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n != "current")
        .collect();
    versions.sort();
    versions.pop()
}

// ---------------------------------------------------------------------------
// Output.
// ---------------------------------------------------------------------------

fn print_table(apps: &[AppEntry], style: &Style, verbose: bool) {
    println!(
        "{}  {}",
        style.bold("Apps"),
        style.dim(&format!("({} found)", apps.len()))
    );
    println!();
    for app in apps {
        let size = app
            .size_bytes
            .map(fmt_size)
            .unwrap_or_else(|| "-".to_string());
        let date = app.install_date.as_deref().unwrap_or("-");
        println!(
            "  {:<42} {:<16} {:>10}  {:<10} {}",
            truncate(&app.name, 42),
            truncate(&app.version, 16),
            size,
            date,
            style.dim(app.source.label())
        );
        if verbose {
            if !app.publisher.is_empty() {
                println!("      {}", style.dim(&app.publisher));
            }
            if !app.location.is_empty() {
                println!("      {}", style.dim(&app.location));
            }
        }
    }
    // per-source tally, e.g. "system 12 · user 30 · portable 5"
    let mut counts: Vec<(Source, usize)> = Vec::new();
    for app in apps {
        match counts.iter_mut().find(|(s, _)| *s == app.source) {
            Some((_, n)) => *n += 1,
            None => counts.push((app.source, 1)),
        }
    }
    let summary: Vec<String> = counts
        .iter()
        .map(|(s, n)| format!("{} {}", s.label(), n))
        .collect();
    println!("\n{}", style.dim(&summary.join(" · ")));
}

pub fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let cut: String = s.chars().take(max.saturating_sub(1)).collect();
    format!("{cut}…")
}

fn print_json(apps: &[AppEntry]) {
    use crate::analyze::json_escape;
    let mut out = String::from("[");
    for (i, a) in apps.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        out.push_str(&format!(
            "{{\"name\":\"{}\",\"version\":\"{}\",\"publisher\":\"{}\",\"location\":\"{}\",\"uninstall\":\"{}\",\"size\":{},\"installed\":{},\"source\":\"{}\"}}",
            json_escape(&a.name),
            json_escape(&a.version),
            json_escape(&a.publisher),
            json_escape(&a.location),
            json_escape(&a.uninstall_string),
            a.size_bytes.map(|s| s.to_string()).unwrap_or_else(|| "null".into()),
            a.install_date
                .as_ref()
                .map(|d| format!("\"{d}\""))
                .unwrap_or_else(|| "null".into()),
            a.source.label()
        ));
    }
    out.push(']');
    println!("{out}");
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(name: &str, size: Option<u64>, date: Option<&str>) -> AppEntry {
        AppEntry {
            name: name.into(),
            version: String::new(),
            publisher: String::new(),
            location: String::new(),
            uninstall_string: String::new(),
            size_bytes: size,
            install_date: date.map(String::from),
            source: Source::Machine,
        }
    }

    #[test]
    fn hide_rules_match_geek_behavior() {
        assert!(should_hide("", false, false, ""));
        assert!(should_hide("   ", false, false, ""));
        assert!(should_hide("Runtime", true, false, ""));
        assert!(should_hide("KB5001234", false, true, ""));
        assert!(should_hide("Update", false, false, "Security Update"));
        assert!(should_hide("Update", false, false, "Hotfix"));
        assert!(!should_hide("7-Zip", false, false, ""));
    }

    #[test]
    fn install_date_formatting() {
        assert_eq!(fmt_install_date("20240315"), Some("2024-03-15".into()));
        assert_eq!(fmt_install_date(""), None);
        assert_eq!(fmt_install_date("2024"), None);
        assert_eq!(fmt_install_date("2024031X"), None);
    }

    #[test]
    fn sorting_by_each_key() {
        let mut apps = vec![
            entry("beta", Some(100), Some("2024-01-01")),
            entry("Alpha", Some(300), None),
            entry("gamma", None, Some("2025-06-01")),
        ];
        sort_apps(&mut apps, SortKey::Name);
        assert_eq!(apps[0].name, "Alpha");
        sort_apps(&mut apps, SortKey::Size);
        assert_eq!(apps[0].name, "Alpha"); // 300 bytes first
        assert_eq!(apps[2].name, "gamma"); // None -> last
        sort_apps(&mut apps, SortKey::Date);
        assert_eq!(apps[0].name, "gamma"); // newest first
    }

    #[test]
    fn portable_scan_detects_exe_dirs() {
        let root = std::env::temp_dir().join(format!("dpan-apps-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        // LOCALAPPDATA\Programs layout: one real app, one data-only dir
        let programs = root.join("AppData").join("Local").join("Programs");
        fs::create_dir_all(programs.join("MyTool")).unwrap();
        fs::write(programs.join("MyTool").join("mytool.exe"), b"x").unwrap();
        fs::create_dir_all(programs.join("JustData")).unwrap();
        fs::write(programs.join("JustData").join("readme.txt"), b"x").unwrap();
        // scoop layout: apps/<name>/<version>/app.exe
        let scoop = root.join("scoop").join("apps").join("ripgrep");
        fs::create_dir_all(scoop.join("14.1.0")).unwrap();
        fs::write(scoop.join("14.1.0").join("rg.exe"), b"x").unwrap();

        assert!(contains_exe(&programs.join("MyTool"), 2));
        assert!(!contains_exe(&programs.join("JustData"), 2));
        assert!(contains_exe(&scoop, 2));
        assert_eq!(scoop_version(&scoop), Some("14.1.0".into()));
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn truncate_handles_wide_names() {
        assert_eq!(truncate("short", 10), "short");
        assert_eq!(truncate("exactly-10", 10), "exactly-10");
        assert_eq!(truncate("this is far too long", 10), "this is f…");
    }
}
