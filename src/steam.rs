//! Steam game inventory: parses Steam's own library metadata instead of
//! the registry, because the `Steam App <id>` registry entries carry no
//! size. The ACF manifests have exact SizeOnDisk and the install dir.
//!
//! Root discovery: DPAN_STEAM_DIR env (tests/override) -> registry
//! HKCU\Software\Valve\Steam SteamPath -> %ProgramFiles(x86)%\Steam.
//! Libraries come from steamapps/libraryfolders.vdf, games from
//! steamapps/appmanifest_*.acf. Uninstall goes through Steam itself via
//! the steam://uninstall/<appid> protocol.

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

use crate::apps::{AppEntry, Source};
use crate::clean::iso_from_unix;

pub fn collect() -> Vec<AppEntry> {
    let mut libraries: Vec<PathBuf> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    if let Some(root) = steam_root() {
        add_library(&mut libraries, &mut seen, root.clone());
        // libraryfolders.vdf is authoritative: it lists every library the
        // user added in Steam, whatever drive it lives on.
        for lib in vdf_libraries(&root) {
            add_library(&mut libraries, &mut seen, lib);
        }
    }
    // Fallback for broken root discovery: users conventionally keep
    // libraries at <drive>:\SteamLibrary, so probe every drive letter.
    for lib in drive_scan_libraries() {
        add_library(&mut libraries, &mut seen, lib);
    }
    libraries.iter().flat_map(|lib| scan_library(lib)).collect()
}

/// Case/separator-insensitive dedupe: the registry stores the root as
/// `c:/program files (x86)/steam` while the vdf lists the same library as
/// `C:\Program Files (x86)\Steam`, and the vdf always includes the root.
fn norm_lib(p: &Path) -> String {
    p.to_string_lossy()
        .to_lowercase()
        .replace('/', "\\")
        .trim_end_matches('\\')
        .to_string()
}

fn add_library(libraries: &mut Vec<PathBuf>, seen: &mut HashSet<String>, p: PathBuf) {
    if p.is_dir() && seen.insert(norm_lib(&p)) {
        libraries.push(p);
    }
}

fn vdf_libraries(root: &Path) -> Vec<PathBuf> {
    let vdf = root.join("steamapps").join("libraryfolders.vdf");
    let Ok(content) = fs::read_to_string(&vdf) else {
        return Vec::new();
    };
    parse_library_paths(&content)
        .into_iter()
        .map(PathBuf::from)
        .collect()
}

/// Probe `<drive>:\SteamLibrary` on every drive letter (Windows only).
fn drive_scan_libraries() -> Vec<PathBuf> {
    if !cfg!(windows) {
        return Vec::new();
    }
    (b'A'..=b'Z')
        .map(|letter| PathBuf::from(format!("{}:\\SteamLibrary", letter as char)))
        .filter(|p| p.join("steamapps").is_dir())
        .collect()
}

fn steam_root() -> Option<PathBuf> {
    if let Ok(dir) = std::env::var("DPAN_STEAM_DIR") {
        let p = PathBuf::from(dir);
        if p.is_dir() {
            return Some(p);
        }
        // invalid override: fall through to normal discovery
    }
    #[cfg(windows)]
    {
        use crate::reg::{Key, HKCU};
        if let Some(key) = Key::open(HKCU, r"Software\Valve\Steam") {
            let path = key.string_value("SteamPath");
            if !path.is_empty() {
                let p = PathBuf::from(path);
                if p.is_dir() {
                    return Some(p);
                }
            }
        }
        if let Ok(pf) = std::env::var("ProgramFiles(x86)") {
            let p = PathBuf::from(pf).join("Steam");
            if p.is_dir() {
                return Some(p);
            }
        }
    }
    None
}

/// Root + its vdf-listed libraries (no drive scan); test-friendly entry.
#[cfg_attr(not(test), allow(dead_code))]
pub fn collect_from_root(root: &Path) -> Vec<AppEntry> {
    let mut libraries = Vec::new();
    let mut seen = HashSet::new();
    add_library(&mut libraries, &mut seen, root.to_path_buf());
    for lib in vdf_libraries(root) {
        add_library(&mut libraries, &mut seen, lib);
    }
    libraries.iter().flat_map(|lib| scan_library(lib)).collect()
}

/// Every ACF manifest inside one library's steamapps folder.
fn scan_library(lib: &Path) -> Vec<AppEntry> {
    let steamapps = lib.join("steamapps");
    let Ok(rd) = fs::read_dir(&steamapps) else {
        return Vec::new();
    };
    let mut games = Vec::new();
    for entry in rd.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if !name.starts_with("appmanifest_") || !name.ends_with(".acf") {
            continue;
        }
        let Ok(content) = fs::read_to_string(entry.path()) else {
            continue;
        };
        let Some(game) = parse_acf(&content) else {
            continue;
        };
        games.push(AppEntry {
            name: game.name,
            version: String::new(),
            publisher: "Steam".into(),
            location: steamapps
                .join("common")
                .join(&game.installdir)
                .display()
                .to_string(),
            uninstall_string: format!("steam://uninstall/{}", game.appid),
            reg_key: String::new(),
            size_bytes: (game.size_on_disk > 0).then_some(game.size_on_disk),
            install_date: (game.last_updated > 0)
                .then(|| iso_from_unix(game.last_updated)[..10].to_string()),
            source: Source::Steam,
        });
    }
    games
}

/// Extract the app ID used to match a thin registry entry with its richer ACF
/// replacement. A missing/corrupt manifest therefore affects only that game.
pub fn registry_app_id(name: &str, uninstall_string: &str) -> Option<String> {
    let lower = uninstall_string.to_ascii_lowercase();
    if let Some(start) = lower.find("steam://uninstall/") {
        let value = &uninstall_string[start + "steam://uninstall/".len()..];
        let app_id: String = value.chars().take_while(char::is_ascii_digit).collect();
        if !app_id.is_empty() {
            return Some(app_id);
        }
    }
    name.strip_prefix("Steam App ")
        .filter(|id| !id.is_empty() && id.bytes().all(|b| b.is_ascii_digit()))
        .map(String::from)
}

// ---------------------------------------------------------------------------
// Minimal VDF/ACF parsing: files are trees of `"key" "value"` lines; we
// only need flat key lookups, so a line scanner is enough.
// ---------------------------------------------------------------------------

/// One `"key"  "value"` line -> (key, value). Scans char by char so
/// escaped quotes (`\"`) and backslashes (`\\`) inside values survive.
fn kv_line(line: &str) -> Option<(String, String)> {
    let mut segments: Vec<String> = Vec::new();
    let mut current = String::new();
    let mut in_quotes = false;
    let mut chars = line.chars();
    while let Some(c) = chars.next() {
        if !in_quotes {
            if c == '"' {
                in_quotes = true;
                current.clear();
            }
            continue;
        }
        match c {
            '\\' => {
                // \" -> ", \\ -> \; anything else kept verbatim
                if let Some(next) = chars.next() {
                    current.push(next);
                }
            }
            '"' => {
                in_quotes = false;
                segments.push(std::mem::take(&mut current));
            }
            other => current.push(other),
        }
    }
    (segments.len() >= 2).then(|| (segments[0].clone(), segments[1].clone()))
}

/// libraryfolders.vdf: every "path" value is a library root.
pub fn parse_library_paths(vdf: &str) -> Vec<String> {
    vdf.lines()
        .filter_map(kv_line)
        .filter(|(k, _)| k == "path")
        .map(|(_, v)| v)
        .collect()
}

pub struct AcfGame {
    pub appid: String,
    pub name: String,
    pub installdir: String,
    pub size_on_disk: u64,
    pub last_updated: u64,
}

pub fn parse_acf(acf: &str) -> Option<AcfGame> {
    let mut appid = String::new();
    let mut name = String::new();
    let mut installdir = String::new();
    let mut size_on_disk = 0u64;
    let mut last_updated = 0u64;
    for (key, value) in acf.lines().filter_map(kv_line) {
        match key.as_str() {
            "appid" if appid.is_empty() => appid = value,
            "name" if name.is_empty() => name = value,
            "installdir" if installdir.is_empty() => installdir = value,
            "SizeOnDisk" => size_on_disk = value.parse().unwrap_or(0),
            "LastUpdated" => last_updated = value.parse().unwrap_or(0),
            _ => {}
        }
    }
    // appid feeds a steam:// URL handed to the shell: digits only, which
    // also filters corrupt manifests
    let appid_ok = !appid.is_empty() && appid.bytes().all(|b| b.is_ascii_digit());
    (appid_ok && !name.is_empty() && !installdir.is_empty()).then_some(AcfGame {
        appid,
        name,
        installdir,
        size_on_disk,
        last_updated,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const ACF: &str = r#"
"AppState"
{
	"appid"		"570"
	"Universe"		"1"
	"name"		"Dota 2"
	"StateFlags"		"4"
	"installdir"		"dota 2 beta"
	"LastUpdated"		"1721718000"
	"SizeOnDisk"		"39125612544"
	"buildid"		"14923282"
}
"#;

    const VDF: &str = r#"
"libraryfolders"
{
	"0"
	{
		"path"		"C:\\Program Files (x86)\\Steam"
		"label"		""
	}
	"1"
	{
		"path"		"D:\\SteamLibrary"
		"totalsize"		"2000396288000"
	}
}
"#;

    #[test]
    fn acf_parsing() {
        let game = parse_acf(ACF).unwrap();
        assert_eq!(game.appid, "570");
        assert_eq!(game.name, "Dota 2");
        assert_eq!(game.installdir, "dota 2 beta");
        assert_eq!(game.size_on_disk, 39_125_612_544);
        assert_eq!(game.last_updated, 1_721_718_000);
        assert!(parse_acf("\"AppState\"\n{\n}").is_none());
    }

    #[test]
    fn vdf_library_paths() {
        let paths = parse_library_paths(VDF);
        assert_eq!(
            paths,
            vec![r"C:\Program Files (x86)\Steam", r"D:\SteamLibrary"]
        );
    }

    #[test]
    fn registry_steam_ids_are_extracted_for_per_game_deduplication() {
        assert_eq!(registry_app_id("Steam App 570", ""), Some("570".into()));
        assert_eq!(
            registry_app_id("Dota 2", r#""steam.exe" steam://uninstall/570"#),
            Some("570".into())
        );
        assert_eq!(
            registry_app_id("Dota 2", "STEAM://UNINSTALL/570"),
            Some("570".into())
        );
        assert_eq!(registry_app_id("Steam", r#""C:\...\uninstall.exe""#), None);
    }

    #[test]
    fn kv_line_handles_escaped_quotes() {
        let (k, v) = kv_line(r#"	"name"		"The \"Sequel\"""#).unwrap();
        assert_eq!(k, "name");
        assert_eq!(v, r#"The "Sequel""#);
        let (k, v) = kv_line(r#""path"  "D:\\SteamLibrary""#).unwrap();
        assert_eq!(k, "path");
        assert_eq!(v, r"D:\SteamLibrary");
        assert!(kv_line(r#""OnlyKey""#).is_none());
    }

    #[test]
    fn acf_rejects_non_numeric_appid() {
        let bad = ACF.replace(r#""appid"		"570""#, r#""appid"		"570\" & calc""#);
        assert!(parse_acf(&bad).is_none());
    }

    #[test]
    fn vdf_root_listed_in_different_case_not_scanned_twice() {
        let root = std::env::temp_dir().join(format!("dpan-steam-dupe-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let steamapps = root.join("steamapps");
        std::fs::create_dir_all(steamapps.join("common")).unwrap();
        std::fs::write(steamapps.join("appmanifest_570.acf"), ACF).unwrap();
        // vdf lists the root itself with different casing (real Steam does
        // this: registry path lowercase, vdf path original case)
        let upper = root.to_str().unwrap().to_uppercase();
        // uppercase path only exists on case-insensitive filesystems (macOS
        // default, Windows) — which is exactly the affected environment
        if std::path::Path::new(&upper).is_dir() {
            let vdf = format!(
                "\"libraryfolders\"\n{{\n\t\"0\"\n\t{{\n\t\t\"path\"\t\t\"{}\"\n\t}}\n}}\n",
                upper.replace('\\', "\\\\")
            );
            std::fs::write(steamapps.join("libraryfolders.vdf"), vdf).unwrap();
            let games = collect_from_root(&root);
            assert_eq!(games.len(), 1, "same library scanned twice");
        }
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn collect_from_fixture_root() {
        let root = std::env::temp_dir().join(format!("dpan-steam-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let steamapps = root.join("steamapps");
        std::fs::create_dir_all(steamapps.join("common").join("dota 2 beta")).unwrap();
        std::fs::write(steamapps.join("appmanifest_570.acf"), ACF).unwrap();
        std::fs::write(steamapps.join("appmanifest_broken.acf"), "junk").unwrap();

        let games = collect_from_root(&root);
        assert_eq!(games.len(), 1);
        let g = &games[0];
        assert_eq!(g.name, "Dota 2");
        assert_eq!(g.size_bytes, Some(39_125_612_544)); // ~36.4 GB
        assert_eq!(g.uninstall_string, "steam://uninstall/570");
        assert!(g.location.ends_with(&format!(
            "steamapps{}common{}dota 2 beta",
            std::path::MAIN_SEPARATOR,
            std::path::MAIN_SEPARATOR
        )));
        assert_eq!(g.install_date.as_deref(), Some("2024-07-23"));
        std::fs::remove_dir_all(&root).unwrap();
    }
}
