//! Installed application inventory (`dpan apps`): registry uninstall
//! keys, Steam library manifests, and a portable-app scanner, shown as a
//! full-screen picker in a terminal (Enter hands the selected app to the
//! uninstaller — with confirmation) or as a flat table when piped.
//! Collection itself never modifies the registry or filesystem.

use std::collections::HashSet;
use std::fs;
use std::io::{IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::UNIX_EPOCH;

use crate::clean::iso_from_unix;
use crate::term::{self, Key, RawMode};
use crate::ui::{fmt_size, pad_to_width, truncate_width, Style};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[cfg_attr(not(windows), allow(dead_code))] // registry variants unused off-Windows
pub enum Source {
    Machine,
    Machine32,
    User,
    Steam,
    Portable,
}

impl Source {
    pub fn label(self) -> &'static str {
        match self {
            Source::Machine => "system",
            Source::Machine32 => "sys32",
            Source::User => "user",
            Source::Steam => "steam",
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
    /// Registry key holding this entry, e.g. `HKLM\...\Uninstall\7-Zip`
    /// (empty for portable apps). Used to report leftovers after uninstall.
    pub reg_key: String,
    pub size_bytes: Option<u64>,
    /// ISO date (YYYY-MM-DD) when known.
    pub install_date: Option<String>,
    pub source: Source,
}

#[derive(Default)]
pub struct AppsOptions {
    pub json: bool,
    pub filter: Option<String>,
}

pub fn run(opts: &AppsOptions) -> ExitCode {
    let mut apps = collect();
    if let Some(filter) = &opts.filter {
        let needle = filter.to_lowercase();
        apps.retain(|a| a.name.to_lowercase().contains(&needle));
    }
    sort_apps(&mut apps);

    if opts.json {
        print_json(&apps);
        return ExitCode::SUCCESS;
    }
    let style = Style::auto();
    let interactive = std::io::stdin().is_terminal() && std::io::stdout().is_terminal();
    if interactive && !apps.is_empty() {
        return browse(apps, &style);
    }
    print_table(&apps, &style);
    if !cfg!(windows) {
        println!(
            "{}",
            style.dim("(registry source is Windows-only; only portable/steam scans ran)")
        );
    }
    ExitCode::SUCCESS
}

pub fn collect() -> Vec<AppEntry> {
    let mut apps = registry::collect();
    // Steam games come from ACF manifests (exact sizes). Only drop the
    // thin registry duplicates when the ACF scan actually delivered
    // replacements — otherwise games would silently vanish whenever
    // library discovery fails.
    let steam_games = crate::steam::collect();
    merge_steam_games(&mut apps, steam_games);
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

fn merge_steam_games(apps: &mut Vec<AppEntry>, steam_games: Vec<AppEntry>) {
    let steam_ids: HashSet<String> = steam_games
        .iter()
        .filter_map(|game| crate::steam::registry_app_id(&game.name, &game.uninstall_string))
        .collect();
    apps.retain(
        |app| match crate::steam::registry_app_id(&app.name, &app.uninstall_string) {
            Some(id) => !steam_ids.contains(&id),
            None => true,
        },
    );
    apps.extend(steam_games);
}

fn norm_path(p: &str) -> String {
    p.to_lowercase()
        .replace('/', "\\")
        .trim_end_matches('\\')
        .to_string()
}

/// Biggest first — the question "what is eating my disk" answers itself;
/// unknown sizes sink to the bottom, ties break by name.
pub fn sort_apps(apps: &mut [AppEntry]) {
    apps.sort_by(|a, b| {
        b.size_bytes
            .unwrap_or(0)
            .cmp(&a.size_bytes.unwrap_or(0))
            .then(a.name.to_lowercase().cmp(&b.name.to_lowercase()))
    });
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
    use crate::reg::{Key, HKCU, HKLM};

    const UNINSTALL: &str = r"SOFTWARE\Microsoft\Windows\CurrentVersion\Uninstall";
    const UNINSTALL_32: &str = r"SOFTWARE\WOW6432Node\Microsoft\Windows\CurrentVersion\Uninstall";

    pub fn collect() -> Vec<AppEntry> {
        let mut apps = Vec::new();
        let hives = [
            (HKLM, UNINSTALL, Source::Machine),
            (HKLM, UNINSTALL_32, Source::Machine32),
            (HKCU, UNINSTALL, Source::User),
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
                    // prefer the vendor's silent command when present
                    uninstall_string: {
                        let quiet = key.string_value("QuietUninstallString");
                        if quiet.is_empty() {
                            key.string_value("UninstallString")
                        } else {
                            quiet
                        }
                    },
                    reg_key: format!(
                        "{}\\{path}\\{sub}",
                        if root == HKLM { "HKLM" } else { "HKCU" }
                    ),
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
// Source 2: portable-app scanner (pure filesystem, works everywhere).
// ---------------------------------------------------------------------------

/// Directories where unzip-and-run apps typically live. Extra roots can be
/// added in `%APPDATA%\dustpan\portable_dirs.txt` (one per line).
pub(crate) fn portable_roots() -> Vec<PathBuf> {
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
                reg_key: String::new(),
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
// Interactive browser: j/k move, Space multi-select, Enter uninstall,
// q quit.
// ---------------------------------------------------------------------------

fn browse(mut apps: Vec<AppEntry>, style: &Style) -> ExitCode {
    let picked = {
        let Some(_raw) = RawMode::enter() else {
            print_table(&apps, style);
            return ExitCode::SUCCESS;
        };
        let _screen = term::AltScreen::enter();
        pick_apps(&mut apps, style)
        // raw mode + alt screen drop here, terminal is back to normal
    };
    if picked.is_empty() {
        return ExitCode::SUCCESS; // quit without choosing
    }
    let un_opts = crate::uninstall::UninstallOptions {
        filter: String::new(),
        dry_run: false,
        yes: false,
    };
    if picked.len() == 1 {
        // single app: uninstall_app runs its own confirmation
        return crate::uninstall::uninstall_app(&apps[picked[0]], &un_opts, false);
    }
    // batch: one summary + one confirmation, then run each uninstaller
    let total: u64 = picked.iter().filter_map(|&i| apps[i].size_bytes).sum();
    println!(
        "{}  {} apps · {}",
        style.bold("Uninstall"),
        picked.len(),
        style.cyan(&fmt_size(total))
    );
    for &i in &picked {
        let size = apps[i]
            .size_bytes
            .map(fmt_size)
            .unwrap_or_else(|| "-".to_string());
        println!(
            "  {} {:>10}  {}",
            pad_to_width(&truncate(&apps[i].name, 44), 44),
            size,
            style.dim(apps[i].source.label())
        );
    }
    if !crate::ui::confirm(&format!("\nUninstall these {} apps?", picked.len())) {
        println!("Aborted, nothing was uninstalled.");
        return ExitCode::SUCCESS;
    }
    println!();
    let mut failed = 0usize;
    for &i in &picked {
        // batch confirmation already given: skip the per-app prompt but
        // keep the leftover-sweep prompts (opts.yes stays false)
        if crate::uninstall::uninstall_app(&apps[i], &un_opts, true) != ExitCode::SUCCESS {
            failed += 1;
        }
        println!();
    }
    if failed > 0 {
        eprintln!("{failed} of {} uninstalls reported problems", picked.len());
        return ExitCode::FAILURE;
    }
    ExitCode::SUCCESS
}

/// Selection key that survives re-sorting.
fn entry_key(app: &AppEntry) -> String {
    format!("{}|{}", app.name, app.location)
}

/// Cursor loop inside the alternate screen. Space toggles selection with
/// instant feedback (LazyVim-style hint in the footer) and advances the
/// cursor; Enter returns the selection — or just the highlighted row when
/// nothing is marked. Empty result = quit. Sizes missing from the
/// registry are computed when a row is first highlighted: the frame
/// renders first (with a "computing" hint), then the walk runs, the list
/// re-sorts and the cursor follows the entry.
fn pick_apps(apps: &mut [AppEntry], style: &Style) -> Vec<usize> {
    if apps.is_empty() {
        return Vec::new();
    }
    let mut stdin = std::io::stdin().lock();
    let mut cursor = 0usize;
    let mut offset = 0usize;
    let mut selected: HashSet<String> = HashSet::new();
    let mut message = String::new();
    // keyed by location: survives re-sorting, prevents endless re-walks
    let mut size_attempted: HashSet<String> = HashSet::new();
    loop {
        let viewport = picker_viewport(term::term_rows());
        cursor = cursor.min(apps.len() - 1);
        if cursor < offset {
            offset = cursor;
        }
        if cursor >= offset + viewport {
            offset = cursor + 1 - viewport;
        }
        let sel = &apps[cursor];
        let needs_size = sel.size_bytes.is_none()
            && !sel.location.is_empty()
            && location_computable(&sel.location)
            && !size_attempted.contains(&sel.location);
        draw_picker(
            apps, style, cursor, offset, viewport, needs_size, &selected, &message,
        );
        if needs_size {
            // frame already shows the hint; now do the blocking walk
            let location = apps[cursor].location.clone();
            size_attempted.insert(location.clone());
            let path = Path::new(&location);
            if path.symlink_metadata().is_ok() {
                apps[cursor].size_bytes = Some(crate::scan::entry_size(path));
                // keep the "biggest first" contract honest: re-sort and
                // let the cursor follow the entry it was on
                sort_apps(apps);
                cursor = apps
                    .iter()
                    .position(|a| a.location == location)
                    .unwrap_or(cursor);
            }
            continue; // redraw with the real size before reading keys
        }
        message.clear();
        match term::read_key(&mut stdin) {
            Key::Quit => return Vec::new(),
            Key::Down => cursor = (cursor + 1).min(apps.len() - 1),
            Key::Up => cursor = cursor.saturating_sub(1),
            Key::PageDown => cursor = (cursor + viewport).min(apps.len() - 1),
            Key::PageUp => cursor = cursor.saturating_sub(viewport),
            Key::Top => cursor = 0,
            Key::Bottom => cursor = apps.len() - 1,
            Key::Space => {
                // toggle + instant feedback, then advance (fzf-style)
                let key = entry_key(&apps[cursor]);
                if selected.remove(&key) {
                    message = format!("○ unselected {}", apps[cursor].name);
                } else {
                    selected.insert(key);
                    message = format!("● selected {}", apps[cursor].name);
                }
                if !selected.is_empty() {
                    let total: u64 = apps
                        .iter()
                        .filter(|a| selected.contains(&entry_key(a)))
                        .filter_map(|a| a.size_bytes)
                        .sum();
                    message.push_str(&format!(
                        " — {} marked, {}",
                        selected.len(),
                        fmt_size(total)
                    ));
                }
                cursor = (cursor + 1).min(apps.len() - 1);
            }
            Key::Enter | Key::Right => {
                if selected.is_empty() {
                    return vec![cursor];
                }
                return apps
                    .iter()
                    .enumerate()
                    .filter(|(_, a)| selected.contains(&entry_key(a)))
                    .map(|(i, _)| i)
                    .collect();
            }
            Key::Number(_) | Key::Left | Key::Other => {}
        }
    }
}

/// The frame can contain nine non-list rows: three header rows, the
/// continuation marker, a footer spacer, and four detail/status rows.
/// Reserving all of them prevents a full frame from scrolling the console
/// and pushing its title off the top edge.
fn picker_viewport(terminal_rows: usize) -> usize {
    terminal_rows.saturating_sub(9).clamp(1, 500)
}

/// Guard against vendors writing overly broad InstallLocation values
/// (e.g. `C:\Program Files` itself): require at least two path segments
/// below the root before we agree to walk it.
fn location_computable(location: &str) -> bool {
    Path::new(location)
        .components()
        .filter(|c| matches!(c, std::path::Component::Normal(_)))
        .count()
        >= 2
}

#[allow(clippy::too_many_arguments)]
fn draw_picker(
    apps: &[AppEntry],
    style: &Style,
    cursor: usize,
    offset: usize,
    viewport: usize,
    computing: bool,
    selected: &HashSet<String>,
    message: &str,
) {
    let out = render_picker(
        apps, style, cursor, offset, viewport, computing, selected, message,
    );
    print!("{out}");
    let _ = std::io::stdout().flush();
}

#[allow(clippy::too_many_arguments)]
fn render_picker(
    apps: &[AppEntry],
    style: &Style,
    cursor: usize,
    offset: usize,
    viewport: usize,
    computing: bool,
    selected: &HashSet<String>,
    message: &str,
) -> String {
    let mut out = String::from("\x1b[H\x1b[2J");
    let total: u64 = apps.iter().filter_map(|a| a.size_bytes).sum();
    let hint = if selected.is_empty() {
        "j/k move · Space select · Enter uninstall · g/G jump · q quit".to_string()
    } else {
        format!(
            "{} marked · Enter uninstalls them · Space toggle · q quit",
            selected.len()
        )
    };
    out.push_str(&format!(
        "{}  {}\r\n{}\r\n\r\n",
        style.bold("Apps"),
        style.dim(&format!("{} apps · {}", apps.len(), fmt_size(total))),
        style.dim(&hint)
    ));
    for (i, app) in apps.iter().enumerate().skip(offset).take(viewport) {
        let size = if i == cursor && computing {
            "…".to_string()
        } else {
            app.size_bytes
                .map(fmt_size)
                .unwrap_or_else(|| "-".to_string())
        };
        let is_marked = selected.contains(&entry_key(app));
        let mark = if is_marked { "●" } else { " " };
        let line = format!(
            "{mark} {} {} {:>10}  {}",
            pad_to_width(&truncate(&app.name, 44), 44),
            pad_to_width(&truncate(&app.version, 14), 14),
            size,
            app.source.label()
        );
        if i == cursor {
            out.push_str(&style.invert(&format!(" ❯ {line}")));
            out.push_str("\r\n");
        } else if is_marked {
            out.push_str(&format!("   {}\r\n", style.cyan(&line)));
        } else {
            out.push_str(&format!("   {line}\r\n"));
        }
    }
    if offset + viewport < apps.len() {
        out.push_str(&style.dim(&format!("   … {}/{} shown", offset + viewport, apps.len())));
        out.push_str("\r\n");
    }
    // footer: everything we know about the highlighted app
    let sel = &apps[cursor];
    out.push_str("\r\n");
    if !sel.location.is_empty() {
        out.push_str(&format!(
            "{}\r\n",
            style.dim(&format!("location  {}", truncate(&sel.location, 100)))
        ));
    }
    if computing {
        out.push_str(&format!("{}\r\n", style.dim("computing size…")));
    }
    if !message.is_empty() {
        out.push_str(&format!("{}\r\n", style.yellow(message)));
    }
    let mut meta = Vec::new();
    if !sel.publisher.is_empty() {
        meta.push(sel.publisher.clone());
    }
    if let Some(date) = &sel.install_date {
        meta.push(format!("installed {date}"));
    }
    if !meta.is_empty() {
        out.push_str(&format!("{}\r\n", style.dim(&meta.join(" · "))));
    }
    // A CRLF emitted on the terminal's last row scrolls the entire console
    // by one line. Leave the cursor on the final rendered row instead.
    if out.ends_with("\r\n") {
        out.truncate(out.len() - 2);
    }
    out
}

fn print_table(apps: &[AppEntry], style: &Style) {
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
            "  {} {} {:>10}  {:<10} {}",
            pad_to_width(&truncate(&app.name, 42), 42),
            pad_to_width(&truncate(&app.version, 16), 16),
            size,
            date,
            style.dim(app.source.label())
        );
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

/// Width-aware truncation (CJK chars count as 2 cells); shared with the
/// uninstall candidate list and ctxmenu output.
pub fn truncate(s: &str, max: usize) -> String {
    truncate_width(s, max)
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
    use crate::ui::display_width;

    fn entry(name: &str, size: Option<u64>, date: Option<&str>) -> AppEntry {
        AppEntry {
            name: name.into(),
            version: String::new(),
            publisher: String::new(),
            location: String::new(),
            uninstall_string: String::new(),
            reg_key: String::new(),
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
    fn sorting_biggest_first_unknown_last() {
        let mut apps = vec![
            entry("beta", Some(100), Some("2024-01-01")),
            entry("Alpha", Some(300), None),
            entry("gamma", None, Some("2025-06-01")),
        ];
        sort_apps(&mut apps);
        assert_eq!(apps[0].name, "Alpha"); // 300 bytes first
        assert_eq!(apps[1].name, "beta");
        assert_eq!(apps[2].name, "gamma"); // unknown size -> last
    }

    #[test]
    fn steam_merge_replaces_only_games_with_matching_manifests() {
        let mut registry = vec![
            entry("Steam App 570", None, None),
            entry("Steam App 730", None, None),
        ];
        registry[0].uninstall_string = "steam://uninstall/570".into();
        registry[1].uninstall_string = "steam://uninstall/730".into();
        let mut replacement = entry("Dota 2", Some(100), None);
        replacement.uninstall_string = "steam://uninstall/570".into();
        replacement.source = Source::Steam;

        merge_steam_games(&mut registry, vec![replacement]);
        let names: Vec<&str> = registry.iter().map(|app| app.name.as_str()).collect();

        assert_eq!(names, vec!["Steam App 730", "Dota 2"]);
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
        // CJK: 10 cells max -> 4 wide chars (8) + ellipsis (1) = 9 cells
        assert_eq!(truncate("上传到百度网盘助手", 10), "上传到百…");
        assert!(display_width(&truncate("上传到百度网盘助手", 10)) <= 10);
    }

    #[test]
    fn picker_frame_never_exceeds_terminal_height_while_scrolling() {
        let mut apps: Vec<AppEntry> = (0..30)
            .map(|i| entry(&format!("App {i}"), Some(1024), Some("2026-08-14")))
            .collect();
        for app in &mut apps {
            app.location = r"C:\Program Files\Example".into();
            app.publisher = "Example Publisher".into();
        }
        let rows = 24usize;
        let viewport = picker_viewport(rows);
        let mut selected = HashSet::new();
        selected.insert(entry_key(&apps[20]));
        let frame = render_picker(
            &apps,
            &Style::auto(),
            20,
            10,
            viewport,
            true,
            &selected,
            "selected App 20",
        );
        let rendered_lines = frame.split_terminator("\r\n").count();

        assert!(frame.starts_with("\x1b[H\x1b[2JApps"));
        assert!(!frame.ends_with("\r\n"));
        assert!(
            rendered_lines <= rows,
            "{rendered_lines} rendered lines overflow {rows} terminal rows"
        );
    }
}
