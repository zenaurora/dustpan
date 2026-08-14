//! Context-menu manager (`dpan ctxmenu`): list, disable and re-enable the
//! right-click entries that vendors stuff into the registry.
//!
//! Disable is non-destructive and needs no admin rights:
//! - COM handlers (shellex\ContextMenuHandlers) are blocked through the
//!   official per-user kill switch `HKCU\...\Shell Extensions\Blocked`
//!   (the same mechanism ShellExView uses).
//! - Static verbs (shell\<verb>) get a `LegacyDisable` value written to
//!   the HKCU\Software\Classes shadow of the key, which overrides the
//!   HKLM original in the merged HKCR view.
//!
//! Vendor keys are never deleted; `on` reverses everything. dustpan's
//! registry writes stay strictly inside HKCU.

use std::process::ExitCode;

use crate::ui::Style;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Action {
    List,
    Off,
    On,
}

pub struct CtxOptions {
    pub action: Action,
    pub filter: Option<String>,
    pub json: bool,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[cfg_attr(not(windows), allow(dead_code))] // only constructed by the Windows collector
pub enum Kind {
    /// Static verb under `shell\<name>`.
    Verb,
    /// COM handler under `shellex\ContextMenuHandlers\<name>`.
    Handler,
}

impl Kind {
    fn label(self) -> &'static str {
        match self {
            Kind::Verb => "verb",
            Kind::Handler => "handler",
        }
    }
}

/// Where the entry shows up. `path` is the HKCR subtree it lives under.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Scope {
    Files,
    Directory,
    Background,
    Folder,
    Drive,
    AllObjects,
}

impl Scope {
    pub const ALL: [Scope; 6] = [
        Scope::Files,
        Scope::Directory,
        Scope::Background,
        Scope::Folder,
        Scope::Drive,
        Scope::AllObjects,
    ];

    #[cfg_attr(not(windows), allow(dead_code))] // used by the Windows collector
    pub fn reg_path(self) -> &'static str {
        match self {
            Scope::Files => "*",
            Scope::Directory => "Directory",
            Scope::Background => r"Directory\Background",
            Scope::Folder => "Folder",
            Scope::Drive => "Drive",
            Scope::AllObjects => "AllFilesystemObjects",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Scope::Files => "files",
            Scope::Directory => "directories",
            Scope::Background => "folder background",
            Scope::Folder => "folders",
            Scope::Drive => "drives",
            Scope::AllObjects => "all objects",
        }
    }
}

pub struct MenuEntry {
    /// Registry key name (the stable identifier used for matching).
    pub id: String,
    /// Human-readable menu text or CLSID friendly name.
    pub text: String,
    pub scope: Scope,
    pub kind: Kind,
    /// Handlers only: the CLSID being blocked/unblocked.
    pub clsid: String,
    /// Command line (verbs) or DLL path (handlers) — vendor hint.
    pub detail: String,
    pub disabled: bool,
}

impl MenuEntry {
    /// Windows' own entries: disabling those is usually a mistake.
    fn is_windows_builtin(&self) -> bool {
        let d = self.detail.to_lowercase();
        d.contains("\\windows\\system32")
            || d.contains("\\windows\\syswow64")
            || d.contains("%systemroot%\\system32")
            || d.contains("%systemroot%\\syswow64")
            || d.contains("%windir%\\system32")
            || d.contains("%windir%\\syswow64")
    }
}

/// `&Upload to Foo` -> `Upload to Foo`; resource refs (`@shell32.dll,-123`)
/// are unresolvable without LoadString, fall back to the key name.
#[cfg_attr(not(windows), allow(dead_code))] // used by the Windows collector
pub fn clean_menu_text(raw: &str, fallback: &str) -> String {
    let raw = raw.trim();
    if raw.is_empty() || raw.starts_with('@') {
        return fallback.to_string();
    }
    raw.replace('&', "")
}

#[cfg_attr(not(windows), allow(dead_code))] // used by the Windows collector
pub fn looks_like_clsid(s: &str) -> bool {
    let s = s.trim();
    s.len() == 38
        && s.starts_with('{')
        && s.ends_with('}')
        && s[1..37].chars().all(|c| c.is_ascii_hexdigit() || c == '-')
}

pub fn find_matches<'a>(entries: &'a [MenuEntry], needle: &str) -> Vec<&'a MenuEntry> {
    let n = needle.to_lowercase();
    entries
        .iter()
        .filter(|e| e.id.to_lowercase().contains(&n) || e.text.to_lowercase().contains(&n))
        .collect()
}

pub fn run(opts: &CtxOptions) -> ExitCode {
    let style = Style::auto();
    if !cfg!(windows) && opts.action != Action::List {
        eprintln!("ctxmenu on/off only works on Windows");
        return ExitCode::FAILURE;
    }
    let entries = collect();
    match opts.action {
        Action::List => {
            if opts.json {
                print_json(&entries);
            } else {
                print_list(&entries, &style);
                if !cfg!(windows) {
                    println!("{}", style.dim("(context menu registry is Windows-only)"));
                }
            }
            ExitCode::SUCCESS
        }
        Action::Off | Action::On => {
            let filter = opts.filter.as_deref().unwrap_or_default();
            let matches = find_matches(&entries, filter);
            let target = match matches.len() {
                0 => {
                    eprintln!("no context-menu entry matches '{filter}'");
                    return ExitCode::FAILURE;
                }
                1 => matches[0],
                _ => {
                    println!(
                        "{} matches for '{filter}', be more specific:",
                        matches.len()
                    );
                    for e in matches.iter().take(10) {
                        println!(
                            "  {:<30} {:<10} {}",
                            e.text,
                            e.kind.label(),
                            style.dim(e.scope.label())
                        );
                    }
                    return ExitCode::FAILURE;
                }
            };
            if opts.action == Action::Off && target.is_windows_builtin() {
                eprintln!(
                    "'{}' looks like a Windows built-in entry, refusing to disable it",
                    target.text
                );
                return ExitCode::FAILURE;
            }
            let ok = match opts.action {
                Action::Off => disable(target),
                Action::On => enable(target),
                Action::List => unreachable!(),
            };
            if ok {
                let verb = if opts.action == Action::Off {
                    "disabled"
                } else {
                    "restored"
                };
                println!("{} {} '{}'", style.green("✓"), verb, target.text);
                println!(
                    "{}",
                    style.dim("takes effect for new Explorer windows; restart Explorer to apply everywhere")
                );
                ExitCode::SUCCESS
            } else {
                eprintln!("registry write failed (value may not exist, or access denied)");
                ExitCode::FAILURE
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Collection (Windows), output, and the HKCU-only disable/enable writes.
// ---------------------------------------------------------------------------

#[cfg_attr(not(windows), allow(dead_code))] // used by the Windows disable/enable
const BLOCKED_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Shell Extensions\Blocked";

#[cfg(windows)]
fn collect() -> Vec<MenuEntry> {
    use crate::reg::{Key, HKCR, HKCU, HKLM};

    let user_blocked = Key::open(HKCU, BLOCKED_KEY);
    let machine_blocked = Key::open(HKLM, BLOCKED_KEY);
    let is_blocked = |clsid: &str| {
        user_blocked.as_ref().is_some_and(|k| k.has_value(clsid))
            || machine_blocked.as_ref().is_some_and(|k| k.has_value(clsid))
    };

    let mut entries = Vec::new();
    for scope in Scope::ALL {
        // Static verbs: HKCR\<scope>\shell\<verb>
        let shell_path = format!(r"{}\shell", scope.reg_path());
        if let Some(shell) = Key::open(HKCR, &shell_path) {
            for verb in shell.subkeys() {
                let Some(key) = Key::open(HKCR, &format!(r"{shell_path}\{verb}")) else {
                    continue;
                };
                let mui = key.string_value("MUIVerb");
                let default = key.string_value("");
                let raw = if mui.is_empty() { default } else { mui };
                let command = Key::open(HKCR, &format!(r"{shell_path}\{verb}\command"))
                    .map(|c| c.string_value(""))
                    .unwrap_or_default();
                entries.push(MenuEntry {
                    text: clean_menu_text(&raw, &verb),
                    id: verb,
                    scope,
                    kind: Kind::Verb,
                    clsid: String::new(),
                    detail: command,
                    disabled: key.has_value("LegacyDisable"),
                });
            }
        }
        // COM handlers: HKCR\<scope>\shellex\ContextMenuHandlers\<name>
        let handlers_path = format!(r"{}\shellex\ContextMenuHandlers", scope.reg_path());
        if let Some(handlers) = Key::open(HKCR, &handlers_path) {
            for name in handlers.subkeys() {
                let clsid_raw = Key::open(HKCR, &format!(r"{handlers_path}\{name}"))
                    .map(|k| k.string_value(""))
                    .unwrap_or_default();
                let clsid = if looks_like_clsid(&clsid_raw) {
                    clsid_raw
                } else if looks_like_clsid(&name) {
                    name.clone()
                } else {
                    continue; // no CLSID, nothing we could block
                };
                let friendly = Key::open(HKCR, &format!(r"CLSID\{clsid}"))
                    .map(|k| k.string_value(""))
                    .unwrap_or_default();
                let dll = Key::open(HKCR, &format!(r"CLSID\{clsid}\InprocServer32"))
                    .map(|k| k.string_value(""))
                    .unwrap_or_default();
                entries.push(MenuEntry {
                    text: clean_menu_text(&friendly, &name),
                    id: name,
                    scope,
                    kind: Kind::Handler,
                    disabled: is_blocked(&clsid),
                    clsid,
                    detail: dll,
                });
            }
        }
    }
    entries
}

#[cfg(not(windows))]
fn collect() -> Vec<MenuEntry> {
    Vec::new()
}

#[cfg(windows)]
fn disable(entry: &MenuEntry) -> bool {
    use crate::reg::Key;
    match entry.kind {
        Kind::Handler => {
            Key::create_user(BLOCKED_KEY).is_some_and(|k| k.set_string(&entry.clsid, &entry.text))
        }
        Kind::Verb => {
            // shadow the verb in HKCU\Software\Classes; merged HKCR view
            // picks it up and LegacyDisable hides the menu item
            let path = format!(
                r"Software\Classes\{}\shell\{}",
                entry.scope.reg_path(),
                entry.id
            );
            Key::create_user(&path).is_some_and(|k| k.set_string("LegacyDisable", ""))
        }
    }
}

#[cfg(windows)]
fn enable(entry: &MenuEntry) -> bool {
    use crate::reg::Key;
    match entry.kind {
        Kind::Handler => {
            Key::open_user_rw(BLOCKED_KEY).is_some_and(|k| k.delete_value(&entry.clsid))
        }
        Kind::Verb => {
            let path = format!(
                r"Software\Classes\{}\shell\{}",
                entry.scope.reg_path(),
                entry.id
            );
            Key::open_user_rw(&path).is_some_and(|k| k.delete_value("LegacyDisable"))
        }
    }
}

#[cfg(not(windows))]
fn disable(_entry: &MenuEntry) -> bool {
    false
}

#[cfg(not(windows))]
fn enable(_entry: &MenuEntry) -> bool {
    false
}

fn print_list(entries: &[MenuEntry], style: &Style) {
    println!(
        "{}  {}",
        style.bold("Context menu"),
        style.dim(&format!("({} entries)", entries.len()))
    );
    for scope in Scope::ALL {
        let rows: Vec<&MenuEntry> = entries.iter().filter(|e| e.scope == scope).collect();
        if rows.is_empty() {
            continue;
        }
        println!("\n{}", style.bold(&style.cyan(scope.label())));
        for e in rows {
            let status = if e.disabled {
                style.yellow("off")
            } else {
                style.green(" on")
            };
            let tag = if e.is_windows_builtin() {
                style.dim(" [windows]")
            } else {
                String::new()
            };
            println!(
                "  {status}  {:<34} {:<8}{tag}",
                crate::apps::truncate(&e.text, 34),
                style.dim(e.kind.label()),
            );
            if !e.detail.is_empty() {
                println!(
                    "       {}",
                    style.dim(&crate::apps::truncate(&e.detail, 90))
                );
            }
        }
    }
    println!(
        "\n{}",
        style.dim(
            "dpan ctxmenu off <name> hides an entry · on <name> restores it · no admin needed"
        )
    );
}

fn print_json(entries: &[MenuEntry]) {
    use crate::analyze::json_escape;
    let mut out = String::from("[");
    for (i, e) in entries.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        out.push_str(&format!(
            "{{\"id\":\"{}\",\"text\":\"{}\",\"scope\":\"{}\",\"kind\":\"{}\",\"clsid\":\"{}\",\"detail\":\"{}\",\"disabled\":{}}}",
            json_escape(&e.id),
            json_escape(&e.text),
            e.scope.label(),
            e.kind.label(),
            json_escape(&e.clsid),
            json_escape(&e.detail),
            e.disabled
        ));
    }
    out.push(']');
    println!("{out}");
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(id: &str, text: &str, detail: &str) -> MenuEntry {
        MenuEntry {
            id: id.into(),
            text: text.into(),
            scope: Scope::Files,
            kind: Kind::Verb,
            clsid: String::new(),
            detail: detail.into(),
            disabled: false,
        }
    }

    #[test]
    fn menu_text_cleanup() {
        assert_eq!(clean_menu_text("&Upload to Pan", "x"), "Upload to Pan");
        assert_eq!(clean_menu_text("上传到&百度网盘", "x"), "上传到百度网盘");
        assert_eq!(clean_menu_text("@shell32.dll,-8506", "runas"), "runas");
        assert_eq!(clean_menu_text("  ", "fallback"), "fallback");
    }

    #[test]
    fn clsid_detection() {
        assert!(looks_like_clsid("{A1B2C3D4-1234-5678-9ABC-DEF012345678}"));
        assert!(!looks_like_clsid("7-Zip"));
        assert!(!looks_like_clsid("{short}"));
        assert!(!looks_like_clsid("{A1B2C3D4-1234-5678-9ABC-DEF01234567Z}"));
    }

    #[test]
    fn matching_by_id_and_text() {
        let entries = vec![
            entry("BaiduNetdisk", "上传到百度网盘", "baidu.exe"),
            entry("7-Zip", "7-Zip", "7z.dll"),
        ];
        assert_eq!(find_matches(&entries, "baidu").len(), 1);
        assert_eq!(find_matches(&entries, "百度").len(), 1);
        assert_eq!(find_matches(&entries, "zip").len(), 1);
        assert_eq!(find_matches(&entries, "i").len(), 2); // ambiguous
        assert!(find_matches(&entries, "nothing").is_empty());
    }

    #[test]
    fn builtin_detection_guards_windows_entries() {
        let win = entry(
            "runas",
            "Run as administrator",
            r"C:\Windows\System32\shell32.dll",
        );
        let vendor = entry("scan", "XX扫描", r"C:\Program Files\XX\scan.dll");
        let expandable = entry(
            "copyaspath",
            "Copy as path",
            r"%SystemRoot%\System32\shell32.dll",
        );
        assert!(win.is_windows_builtin());
        assert!(expandable.is_windows_builtin());
        assert!(!vendor.is_windows_builtin());
    }
}
