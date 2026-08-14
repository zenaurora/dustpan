//! Application uninstaller (`dpan uninstall <filter>`), the write-side
//! counterpart of `dpan apps`. Approach follows open-source uninstallers
//! (Bulk Crap Uninstaller), not any proprietary tool:
//!
//! 1. Match the app by name against the same inventory `dpan apps` shows.
//! 2. Run the vendor's own uninstaller: msiexec for MSI GUIDs, the
//!    registry UninstallString for everything else; portable apps are
//!    simply deleted (through the safety gate, into nowhere but gone).
//! 3. Afterwards scan for leftovers — per-app directories under the
//!    AppData-style roots matched via name variants — and offer to delete
//!    them through the same safety gate + audit log the cleaner uses.
//!
//! Registry leftovers are reported but never deleted: dustpan writes to
//! the filesystem only, keeping the "read-only registry" promise.

use std::path::PathBuf;
use std::process::ExitCode;

use crate::apps::{self, AppEntry, Source};
use crate::clean::Cleaner;
use crate::safety::Safety;
use crate::scan::entry_size;
use crate::ui::{confirm, fmt_size, Style};

pub struct UninstallOptions {
    pub filter: String,
    pub dry_run: bool,
    pub yes: bool,
}

pub fn run(opts: &UninstallOptions) -> ExitCode {
    let style = Style::auto();
    let needle = opts.filter.to_lowercase();
    let mut apps: Vec<AppEntry> = apps::collect()
        .into_iter()
        .filter(|a| a.name.to_lowercase().contains(&needle))
        .collect();
    apps::sort_apps(&mut apps);

    let app = match apps.len() {
        0 => {
            eprintln!("no installed app matches '{}'", opts.filter);
            return ExitCode::FAILURE;
        }
        1 => &apps[0],
        _ => {
            // ambiguous: show candidates, make the user narrow it down
            println!(
                "{} matches for '{}', be more specific:",
                apps.len(),
                opts.filter
            );
            for a in apps.iter().take(10) {
                println!(
                    "  {:<42} {:<14} {}",
                    apps::truncate(&a.name, 42),
                    apps::truncate(&a.version, 14),
                    style.dim(a.source.label())
                );
            }
            return ExitCode::FAILURE;
        }
    };
    uninstall_app(app, opts, false)
}

/// Uninstall one already-resolved app: confirm -> vendor uninstaller (or
/// gate-checked delete for portable) -> leftover sweep. Also the entry
/// point for the interactive `dpan apps` picker; `pre_confirmed` skips
/// the per-app prompt when a batch confirmation was already given
/// (leftover-sweep prompts still apply).
pub fn uninstall_app(app: &AppEntry, opts: &UninstallOptions, pre_confirmed: bool) -> ExitCode {
    let style = Style::auto();

    println!(
        "{}  {} {}  {}",
        style.bold("Uninstall"),
        style.cyan(&app.name),
        app.version,
        style.dim(app.source.label())
    );
    if !app.location.is_empty() {
        println!("  {}", style.dim(&app.location));
    }
    if opts.dry_run {
        println!(
            "{}",
            style.yellow("(dry run: nothing will be executed or deleted)")
        );
    } else if !pre_confirmed && !opts.yes && !confirm(&format!("\nUninstall {}?", app.name)) {
        println!("Aborted.");
        return ExitCode::SUCCESS;
    }

    let ok = match app.source {
        Source::Portable => uninstall_portable(app, opts, &style),
        _ => run_vendor_uninstaller(app, opts, &style),
    };
    if !ok {
        return ExitCode::FAILURE;
    }

    if app.source == Source::Steam {
        // Steam uninstalls asynchronously in its own UI and owns the game
        // files (plus cloud saves); sweeping right now would race it and
        // could hit save-game directories. Hands off.
        println!(
            "{}",
            style.dim("Steam will finish the uninstall in its own window; no leftover sweep")
        );
        return ExitCode::SUCCESS;
    }

    // Leftover sweep: app data the vendor uninstaller typically leaves behind.
    let leftovers = find_leftovers(app);
    offer_leftover_cleanup(app, &leftovers, opts, &style);
    ExitCode::SUCCESS
}

// ---------------------------------------------------------------------------
// Step 1: run the vendor uninstaller (or delete the portable dir).
// ---------------------------------------------------------------------------

/// Turn an UninstallString into an argv. MSI entries get normalized to
/// `msiexec /x {GUID} /qb` regardless of how the vendor spelled them
/// (`/I{GUID}`, `/X{GUID}`, mixed quoting...), following BCUninstaller.
/// steam:// protocol URLs are handed to the shell so Steam picks them up.
pub fn uninstall_command(uninstall_string: &str) -> Option<Vec<String>> {
    let s = uninstall_string.trim();
    if s.is_empty() {
        return None;
    }
    if s.starts_with("steam://") {
        // `start` resolves the protocol handler; empty "" is the window title
        return Some(vec![
            "cmd".into(),
            "/C".into(),
            "start".into(),
            "".into(),
            s.into(),
        ]);
    }
    if let Some(guid) = extract_msi_guid(s) {
        return Some(vec![
            "msiexec".into(),
            "/x".into(),
            guid,
            "/qb".into(), // basic UI: progress bar, no questions
        ]);
    }
    split_command_line(s)
}

/// MSI uninstall entries always carry a `{GUID}` product code.
fn extract_msi_guid(s: &str) -> Option<String> {
    if !s.to_lowercase().contains("msiexec") {
        return None;
    }
    let start = s.find('{')?;
    let end = s[start..].find('}')? + start;
    let guid = &s[start..=end];
    // sanity: 38 chars incl. braces, hex + hyphens
    (guid.len() == 38
        && guid[1..guid.len() - 1]
            .chars()
            .all(|c| c.is_ascii_hexdigit() || c == '-'))
    .then(|| guid.to_string())
}

/// Minimal Windows command-line splitter: honors double quotes, and for
/// the common unquoted `C:\Program Files\...\uninstall.exe /S` case takes
/// everything up to the first `.exe` as the program path.
pub fn split_command_line(s: &str) -> Option<Vec<String>> {
    let s = s.trim();
    let (program, rest) = if let Some(stripped) = s.strip_prefix('"') {
        let close = stripped.find('"')?;
        (stripped[..close].to_string(), &stripped[close + 1..])
    } else if let Some(pos) = s.to_lowercase().find(".exe") {
        let end = pos + 4;
        (s[..end].to_string(), &s[end..])
    } else {
        // no .exe marker: fall back to whitespace split
        let mut parts = s.split_whitespace();
        (
            parts.next()?.to_string(),
            s.split_once(' ').map_or("", |x| x.1),
        )
    };
    let mut argv = vec![program];
    argv.extend(rest.split_whitespace().map(String::from));
    Some(argv)
}

fn run_vendor_uninstaller(app: &AppEntry, opts: &UninstallOptions, style: &Style) -> bool {
    let Some(argv) = uninstall_command(&app.uninstall_string) else {
        eprintln!("no usable UninstallString for {}", app.name);
        return false;
    };
    println!("{} {}", style.dim("running:"), argv.join(" "));
    if opts.dry_run {
        return true;
    }
    if !cfg!(windows) {
        println!("{}", style.yellow("not on Windows, skipping execution"));
        return true;
    }
    match std::process::Command::new(&argv[0])
        .args(&argv[1..])
        .status()
    {
        Ok(status) => {
            let code = status.code().unwrap_or(-1);
            if !uninstaller_allows_cleanup(code) {
                if code == 1602 {
                    println!("{} uninstall cancelled", style.dim("–"));
                } else {
                    eprintln!("uninstaller exited with code {code}");
                }
                return false;
            }
            if code == 3010 {
                println!("{} uninstalled (reboot required)", style.green("✓"));
            } else if app.source == Source::Steam {
                // `start` only dispatches the URL: Steam confirms and
                // uninstalls asynchronously, so don't claim completion.
                println!("{} handed off to Steam — confirm there", style.green("✓"));
            } else {
                println!("{} vendor uninstaller finished", style.green("✓"));
            }
            true
        }
        Err(e) => {
            eprintln!("failed to launch uninstaller: {e}");
            false
        }
    }
}

fn uninstaller_allows_cleanup(code: i32) -> bool {
    matches!(code, 0 | 3010)
}

fn uninstall_portable(app: &AppEntry, opts: &UninstallOptions, style: &Style) -> bool {
    // Portable apps have no uninstaller: removing the directory is the
    // uninstall. Route it through the standard gate for safety + audit.
    let safety = Safety::load().with_extra_allowed_bases(&apps::portable_roots());
    let path = PathBuf::from(&app.location);
    if let Err(reason) = safety.check(&path) {
        eprintln!("refusing to delete {}: {reason}", path.display());
        return false;
    }
    let size = entry_size(&path);
    if opts.dry_run {
        println!("would delete {} ({})", path.display(), fmt_size(size));
        return true;
    }
    let mut cleaner = Cleaner::new(&safety, false);
    if cleaner.remove_path(&path) {
        println!(
            "{} deleted {} ({})",
            style.green("✓"),
            path.display(),
            fmt_size(size)
        );
        true
    } else {
        eprintln!("could not delete {} (files in use?)", path.display());
        false
    }
}

// ---------------------------------------------------------------------------
// Step 2: leftover sweep.
// ---------------------------------------------------------------------------

pub struct Leftover {
    pub path: PathBuf,
    pub size: u64,
}

/// Name variants an installer might have used for its data directories:
/// "My App" -> ["my app", "myapp", "my-app", "my_app"], plus the publisher
/// subdirectory form `Publisher\App` handled at scan time.
pub fn name_variants(name: &str) -> Vec<String> {
    let base = name.trim().to_lowercase();
    // strip trailing version noise like "7-zip 24.08 (x64)": cut at the
    // first version-looking word (digits and dots only, e.g. "24.08")
    let is_version_word =
        |w: &str| !w.is_empty() && w.chars().all(|c| c.is_ascii_digit() || c == '.');
    let cleaned: String = base
        .split(&[' ', '(', '['][..])
        .filter(|w| !w.is_empty())
        .take_while(|w| !is_version_word(w))
        .collect::<Vec<_>>()
        .join(" ")
        .trim()
        .to_string();
    let base = if cleaned.is_empty() { base } else { cleaned };
    let mut variants = vec![base.clone()];
    for (from, to) in [(" ", ""), (" ", "-"), (" ", "_")] {
        let v = base.replace(from, to);
        if !variants.contains(&v) {
            variants.push(v);
        }
    }
    variants
}

/// Roots where per-app data typically survives an uninstall.
fn leftover_roots() -> Vec<PathBuf> {
    let mut roots = Vec::new();
    let var = |v: &str| std::env::var(v).ok().map(PathBuf::from);
    for v in ["APPDATA", "LOCALAPPDATA", "PROGRAMDATA"] {
        if let Some(p) = var(v) {
            roots.push(p);
        }
    }
    if let Some(local) = var("LOCALAPPDATA") {
        roots.push(local.join("Programs"));
    }
    roots
}

pub fn find_leftovers(app: &AppEntry) -> Vec<Leftover> {
    find_leftovers_in_roots(app, &leftover_roots())
}

fn find_leftovers_in_roots(app: &AppEntry, roots: &[PathBuf]) -> Vec<Leftover> {
    let variants = name_variants(&app.name);
    let publisher = app.publisher.trim().to_lowercase();
    let mut found = Vec::new();
    for root in roots {
        let Ok(rd) = std::fs::read_dir(root) else {
            continue;
        };
        for entry in rd.flatten() {
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }
            let dir_name = entry.file_name().to_string_lossy().to_lowercase();
            if variants.contains(&dir_name) {
                found.push(Leftover {
                    size: entry_size(&path),
                    path,
                });
                continue;
            }
            if publisher.is_empty() || dir_name != publisher {
                continue;
            }
            // Publisher\App layout: collect only matching child directories.
            // The publisher directory may contain unrelated sibling products.
            let Ok(children) = std::fs::read_dir(&path) else {
                continue;
            };
            for child in children.flatten() {
                let child_path = child.path();
                let child_name = child.file_name().to_string_lossy().to_lowercase();
                if child_path.is_dir() && variants.contains(&child_name) {
                    found.push(Leftover {
                        size: entry_size(&child_path),
                        path: child_path,
                    });
                }
            }
        }
    }
    // The install location itself often survives (empty or with logs).
    if !app.location.is_empty() {
        let loc = PathBuf::from(&app.location);
        if loc.is_dir() && app.source != Source::Portable {
            found.push(Leftover {
                size: entry_size(&loc),
                path: loc,
            });
        }
    }
    found
}

fn offer_leftover_cleanup(
    app: &AppEntry,
    leftovers: &[Leftover],
    opts: &UninstallOptions,
    style: &Style,
) {
    // Registry side: report only, we never write the registry.
    if !app.reg_key.is_empty() && !opts.dry_run {
        println!(
            "{}",
            style.dim(&format!(
                "registry entry (removed by the uninstaller, or remove manually): {}",
                app.reg_key
            ))
        );
    }
    if leftovers.is_empty() {
        println!("{}", style.dim("no filesystem leftovers found"));
        return;
    }
    let total: u64 = leftovers.iter().map(|l| l.size).sum();
    println!("\n{} ({}):", style.bold("Leftovers found"), fmt_size(total));
    for l in leftovers {
        println!("  {:>10}  {}", fmt_size(l.size), l.path.display());
    }
    if opts.dry_run {
        println!("{}", style.yellow("dry run: leftovers not deleted"));
        return;
    }
    if !opts.yes && !confirm("Delete these leftovers?") {
        println!("Leftovers kept.");
        return;
    }
    let safety = Safety::load();
    let mut cleaner = Cleaner::new(&safety, false);
    let mut freed = 0u64;
    let mut kept = 0usize;
    for l in leftovers {
        if safety.check(&l.path).is_ok() && cleaner.remove_path(&l.path) {
            freed += l.size;
        } else {
            kept += 1;
        }
    }
    println!("{} freed {}", style.green("✓"), fmt_size(freed));
    if kept > 0 {
        println!(
            "{}",
            style.yellow(&format!("  {kept} paths kept (protected or in use)"))
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn msi_guid_normalization() {
        let cmd =
            uninstall_command("MsiExec.exe /I{A1B2C3D4-1234-5678-9ABC-DEF012345678}").unwrap();
        assert_eq!(cmd[0], "msiexec");
        assert_eq!(cmd[1], "/x");
        assert_eq!(cmd[2], "{A1B2C3D4-1234-5678-9ABC-DEF012345678}");
        assert_eq!(cmd[3], "/qb");
        // /X spelling and extra args normalize the same way
        let cmd2 =
            uninstall_command("msiexec /X {A1B2C3D4-1234-5678-9ABC-DEF012345678} /quiet").unwrap();
        assert_eq!(cmd2[2], "{A1B2C3D4-1234-5678-9ABC-DEF012345678}");
    }

    #[test]
    fn steam_urls_launch_via_protocol_handler() {
        let cmd = uninstall_command("steam://uninstall/570").unwrap();
        assert_eq!(cmd, vec!["cmd", "/C", "start", "", "steam://uninstall/570"]);
    }

    #[test]
    fn exe_command_splitting() {
        let cmd = uninstall_command(r#""C:\Program Files\7-Zip\Uninstall.exe" /S"#).unwrap();
        assert_eq!(cmd[0], r"C:\Program Files\7-Zip\Uninstall.exe");
        assert_eq!(cmd[1], "/S");
        // unquoted path with spaces, .exe heuristic
        let cmd2 = uninstall_command(r"C:\Program Files\Foo Bar\unins000.exe /SILENT").unwrap();
        assert_eq!(cmd2[0], r"C:\Program Files\Foo Bar\unins000.exe");
        assert_eq!(cmd2[1], "/SILENT");
        assert!(uninstall_command("   ").is_none());
    }

    #[test]
    fn bogus_msi_guid_falls_through_to_exe_split() {
        // msiexec mention but broken guid -> treated as a plain command line
        let cmd = uninstall_command("msiexec.exe /x {not-a-guid}").unwrap();
        assert_eq!(cmd[0], "msiexec.exe");
    }

    #[test]
    fn only_success_exit_codes_allow_leftover_cleanup() {
        assert!(uninstaller_allows_cleanup(0));
        assert!(uninstaller_allows_cleanup(3010));
        assert!(!uninstaller_allows_cleanup(1602));
        assert!(!uninstaller_allows_cleanup(1603));
        assert!(!uninstaller_allows_cleanup(-1));
    }

    #[test]
    fn publisher_layout_returns_only_the_matching_app_directory() {
        let root =
            std::env::temp_dir().join(format!("dpan-leftovers-publisher-{}", std::process::id()));
        let publisher = root.join("Acme");
        let wanted = publisher.join("My App");
        let sibling = publisher.join("Other App");
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&wanted).unwrap();
        std::fs::create_dir_all(&sibling).unwrap();
        std::fs::write(wanted.join("cache.bin"), b"wanted").unwrap();
        std::fs::write(sibling.join("keep.bin"), b"keep").unwrap();

        let app = AppEntry {
            name: "My App".into(),
            version: String::new(),
            publisher: "Acme".into(),
            location: String::new(),
            uninstall_string: String::new(),
            reg_key: String::new(),
            size_bytes: None,
            install_date: None,
            source: Source::User,
        };
        let leftovers = find_leftovers_in_roots(&app, std::slice::from_ref(&root));
        let paths: Vec<&std::path::Path> = leftovers.iter().map(|l| l.path.as_path()).collect();

        assert_eq!(paths, vec![wanted.as_path()]);
        assert!(!paths.contains(&publisher.as_path()));
        assert!(!paths.contains(&sibling.as_path()));
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn name_variant_generation() {
        assert_eq!(
            name_variants("Visual Studio Code"),
            vec![
                "visual studio code",
                "visualstudiocode",
                "visual-studio-code",
                "visual_studio_code"
            ]
        );
        // version/arch noise stripped
        assert!(name_variants("7-Zip 24.08 (x64)").contains(&"7-zip".to_string()));
        // single word: no duplicate variants
        assert_eq!(name_variants("Discord"), vec!["discord"]);
    }
}
