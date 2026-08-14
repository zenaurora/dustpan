//! Disk usage explorer (`dpan analyze`), modeled on mole's analyze-go:
//! one concurrent walk builds a directory-size index, then a read-only
//! interactive browser navigates it. Nothing here ever deletes anything.

use std::collections::HashMap;
use std::fs;
use std::io::{self, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Mutex;
use std::thread;
use std::time::{Duration, Instant, UNIX_EPOCH};

use crate::clean::iso_from_unix;
use crate::term::{self, Key, RawMode};
use crate::ui::{fmt_count, fmt_size, Style};

pub struct AnalyzeOptions {
    pub path: Option<PathBuf>,
    pub json: bool,
    pub top: usize,
}

impl Default for AnalyzeOptions {
    fn default() -> Self {
        AnalyzeOptions {
            path: None,
            json: false,
            top: 40,
        }
    }
}

pub fn run(opts: &AnalyzeOptions) -> ExitCode {
    let root = match opts.path.clone().or_else(default_root) {
        Some(p) => p,
        None => {
            eprintln!("error: no path given and no home directory found");
            return ExitCode::FAILURE;
        }
    };
    let Ok(meta) = root.symlink_metadata() else {
        eprintln!("error: path does not exist: {}", root.display());
        return ExitCode::FAILURE;
    };
    if !meta.is_dir() {
        eprintln!("error: not a directory: {}", root.display());
        return ExitCode::FAILURE;
    }

    let style = Style::auto();
    let progress = !opts.json && io::stderr().is_terminal();
    let scan = scan_tree(&root, progress);

    if opts.json {
        print_json(&root, &scan);
        return ExitCode::SUCCESS;
    }
    explore(&root, &scan, &style, opts.top);
    ExitCode::SUCCESS
}

fn default_root() -> Option<PathBuf> {
    std::env::var("USERPROFILE")
        .or_else(|_| std::env::var("HOME"))
        .ok()
        .map(PathBuf::from)
}

// ---------------------------------------------------------------------------
// Scanner: one parallel walk, every visited directory lands in the index.
// ---------------------------------------------------------------------------

pub struct ScanResult {
    /// Total size of every directory visited (recursive, symlinks skipped).
    pub dir_sizes: HashMap<PathBuf, u64>,
    pub files: u64,
    pub dirs: u64,
    pub elapsed: Duration,
}

pub fn scan_tree(root: &Path, progress: bool) -> ScanResult {
    let start = Instant::now();
    let files = AtomicU64::new(0);
    let dirs = AtomicU64::new(0);
    let bytes = AtomicU64::new(0);

    // Immediate children: files counted here, dirs become parallel jobs.
    let mut top_dirs = Vec::new();
    let mut root_file_bytes = 0u64;
    if let Ok(rd) = fs::read_dir(root) {
        for e in rd.flatten() {
            let Ok(meta) = e.metadata() else { continue };
            if meta.is_symlink() {
                continue;
            }
            if meta.is_dir() {
                dirs.fetch_add(1, Ordering::Relaxed);
                top_dirs.push(e.path());
            } else {
                files.fetch_add(1, Ordering::Relaxed);
                bytes.fetch_add(meta.len(), Ordering::Relaxed);
                root_file_bytes += meta.len();
            }
        }
    }

    let queue = Mutex::new(top_dirs.clone());
    let map = Mutex::new(HashMap::new());
    let stop = AtomicBool::new(false);
    let workers = thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4)
        .min(8);

    thread::scope(|scope| {
        if progress {
            scope.spawn(|| {
                while !stop.load(Ordering::Relaxed) {
                    eprint!(
                        "\r\x1b[2Kscanning... {} files, {}",
                        fmt_count(files.load(Ordering::Relaxed)),
                        fmt_size(bytes.load(Ordering::Relaxed))
                    );
                    let _ = io::stderr().flush();
                    thread::sleep(Duration::from_millis(150));
                }
                eprint!("\r\x1b[2K");
                let _ = io::stderr().flush();
            });
        }
        let handles: Vec<_> = (0..workers)
            .map(|_| {
                scope.spawn(|| loop {
                    let job = queue.lock().unwrap().pop();
                    let Some(dir) = job else { break };
                    let mut local = Vec::new();
                    walk(&dir, &mut local, &files, &dirs, &bytes);
                    map.lock().unwrap().extend(local);
                })
            })
            .collect();
        for h in handles {
            let _ = h.join();
        }
        stop.store(true, Ordering::Relaxed);
    });

    let mut dir_sizes = map.into_inner().unwrap();
    let root_total = root_file_bytes
        + top_dirs
            .iter()
            .map(|d| dir_sizes.get(d).copied().unwrap_or(0))
            .sum::<u64>();
    dir_sizes.insert(root.to_path_buf(), root_total);

    ScanResult {
        dir_sizes,
        files: files.load(Ordering::Relaxed),
        dirs: dirs.load(Ordering::Relaxed),
        elapsed: start.elapsed(),
    }
}

fn walk(
    dir: &Path,
    out: &mut Vec<(PathBuf, u64)>,
    files: &AtomicU64,
    dirs: &AtomicU64,
    bytes: &AtomicU64,
) -> u64 {
    let mut total = 0u64;
    if let Ok(rd) = fs::read_dir(dir) {
        for e in rd.flatten() {
            let Ok(meta) = e.metadata() else { continue };
            if meta.is_symlink() {
                continue;
            }
            if meta.is_dir() {
                dirs.fetch_add(1, Ordering::Relaxed);
                total += walk(&e.path(), out, files, dirs, bytes);
            } else {
                files.fetch_add(1, Ordering::Relaxed);
                bytes.fetch_add(meta.len(), Ordering::Relaxed);
                total += meta.len();
            }
        }
    }
    out.push((dir.to_path_buf(), total));
    total
}

// ---------------------------------------------------------------------------
// Explorer: line-based navigation, sizes come from the prebuilt index.
// ---------------------------------------------------------------------------

struct Entry {
    name: String,
    path: PathBuf,
    size: u64,
    is_dir: bool,
}

fn list_entries(dir: &Path, scan: &ScanResult) -> Vec<Entry> {
    let mut entries = Vec::new();
    if let Ok(rd) = fs::read_dir(dir) {
        for e in rd.flatten() {
            let Ok(meta) = e.metadata() else { continue };
            if meta.is_symlink() {
                continue;
            }
            let path = e.path();
            let size = if meta.is_dir() {
                // Dirs created after the scan fall back to a live walk.
                scan.dir_sizes
                    .get(&path)
                    .copied()
                    .unwrap_or_else(|| crate::scan::entry_size(&path))
            } else {
                meta.len()
            };
            entries.push(Entry {
                name: e.file_name().to_string_lossy().into_owned(),
                path,
                size,
                is_dir: meta.is_dir(),
            });
        }
    }
    entries.sort_by(|a, b| b.size.cmp(&a.size).then(a.name.cmp(&b.name)));
    entries
}

fn explore(root: &Path, scan: &ScanResult, style: &Style, top: usize) {
    let interactive = io::stdin().is_terminal() && io::stdout().is_terminal();
    if !interactive {
        // piped: print the root listing once and exit
        let entries = list_entries(root, scan);
        print_flat(root, &entries, scan, style, top);
        return;
    }
    match RawMode::enter() {
        Some(raw) => browse(root, scan, style, top, raw),
        None => {
            // raw mode unavailable (odd terminal): static listing beats nothing
            let entries = list_entries(root, scan);
            print_flat(root, &entries, scan, style, top);
        }
    }
}

/// Full-screen cursor browser: j/k move, l/Enter open, h up, g/G jump,
/// q/ESC quit. Every keypress redraws immediately — no Enter needed.
fn browse(root: &Path, scan: &ScanResult, style: &Style, top: usize, _raw: RawMode) {
    let _screen = term::AltScreen::enter();
    let mut stdin = io::stdin().lock();
    let mut cur = root.to_path_buf();
    let mut entries = list_entries(&cur, scan);
    let mut cursor = 0usize;
    let mut offset = 0usize;
    let mut message = String::new();
    loop {
        let viewport = term::term_rows().saturating_sub(5).clamp(3, top.max(3));
        // keep the cursor inside the visible window
        cursor = cursor.min(entries.len().saturating_sub(1));
        if cursor < offset {
            offset = cursor;
        }
        if cursor >= offset + viewport {
            offset = cursor + 1 - viewport;
        }
        draw(
            &cur, &entries, scan, style, cursor, offset, viewport, &message,
        );
        message.clear();
        match term::read_key(&mut stdin) {
            Key::Quit => return,
            Key::Down => cursor = (cursor + 1).min(entries.len().saturating_sub(1)),
            Key::Up => cursor = cursor.saturating_sub(1),
            Key::PageDown => cursor = (cursor + viewport).min(entries.len().saturating_sub(1)),
            Key::PageUp => cursor = cursor.saturating_sub(viewport),
            Key::Top => cursor = 0,
            Key::Bottom => cursor = entries.len().saturating_sub(1),
            Key::Right | Key::Enter => {
                if let Some(picked) = entries.get(cursor) {
                    if picked.is_dir {
                        cur = picked.path.clone();
                        entries = list_entries(&cur, scan);
                        cursor = 0;
                        offset = 0;
                    } else {
                        message = file_info(picked);
                    }
                }
            }
            Key::Left => {
                if cur != root {
                    let leaving = cur.clone();
                    if let Some(parent) = cur.parent() {
                        cur = parent.to_path_buf();
                    }
                    entries = list_entries(&cur, scan);
                    // land the cursor on the directory we just left
                    cursor = entries.iter().position(|e| e.path == leaving).unwrap_or(0);
                    offset = 0;
                } else {
                    message = "already at the scanned root".into();
                }
            }
            Key::Space | Key::Other => {}
        }
    }
}

fn file_info(entry: &Entry) -> String {
    let modified = entry
        .path
        .metadata()
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| iso_from_unix(d.as_secs()))
        .unwrap_or_else(|| "?".into());
    format!(
        "{} — {}, modified {}",
        entry.name,
        fmt_size(entry.size),
        modified
    )
}

/// One interactive frame. Raw mode disables output post-processing, so
/// every line must end in explicit `\r\n`.
#[allow(clippy::too_many_arguments)]
fn draw(
    cur: &Path,
    entries: &[Entry],
    scan: &ScanResult,
    style: &Style,
    cursor: usize,
    offset: usize,
    viewport: usize,
    message: &str,
) {
    let mut out = String::from("\x1b[H\x1b[2J"); // home + clear
    let total = scan.dir_sizes.get(cur).copied().unwrap_or(0);
    out.push_str(&format!(
        "{}  {}  {}\r\n",
        style.bold("Analyze"),
        style.cyan(&cur.display().to_string()),
        style.dim(&fmt_size(total))
    ));
    out.push_str(&format!(
        "{}\r\n\r\n",
        style.dim(&format!(
            "{} files · {} dirs · scanned in {:.1}s · j/k move · l open · h up · g/G · q quit",
            fmt_count(scan.files),
            fmt_count(scan.dirs),
            scan.elapsed.as_secs_f32()
        ))
    ));
    let max = entries.first().map(|e| e.size).unwrap_or(0).max(1);
    for (i, e) in entries.iter().enumerate().skip(offset).take(viewport) {
        let bar_len = ((e.size as f64 / max as f64) * 14.0).round() as usize;
        let marker = if e.is_dir { "▸" } else { " " };
        let name = if e.is_dir {
            style.cyan(&e.name)
        } else {
            e.name.clone()
        };
        let pointer = if i == cursor {
            style.bold("❯")
        } else {
            " ".into()
        };
        let line = format!(
            "{pointer} {marker} {:<40} {:>10}  {}",
            name,
            fmt_size(e.size),
            style.dim(&"█".repeat(bar_len))
        );
        if i == cursor {
            out.push_str(&format!("\x1b[7m{line}\x1b[0m\r\n"));
        } else {
            out.push_str(&format!("{line}\r\n"));
        }
    }
    if entries.is_empty() {
        out.push_str(&style.dim("  (empty directory)"));
        out.push_str("\r\n");
    } else if offset + viewport < entries.len() {
        out.push_str(&style.dim(&format!(
            "  … {}/{} shown",
            offset + viewport,
            entries.len()
        )));
        out.push_str("\r\n");
    }
    if !message.is_empty() {
        out.push_str(&format!("\r\n{}\r\n", style.yellow(message)));
    }
    print!("{out}");
    let _ = io::stdout().flush();
}

/// Non-interactive fallback: the old one-shot listing (piped output etc.).
fn print_flat(cur: &Path, entries: &[Entry], scan: &ScanResult, style: &Style, top: usize) {
    let total = scan.dir_sizes.get(cur).copied().unwrap_or(0);
    println!(
        "{}  {}",
        style.bold("Analyze"),
        style.cyan(&cur.display().to_string())
    );
    println!(
        "{}",
        style.dim(&format!(
            "total {} · {} files · {} dirs · scanned in {:.1}s",
            fmt_size(total),
            fmt_count(scan.files),
            fmt_count(scan.dirs),
            scan.elapsed.as_secs_f32()
        ))
    );
    println!();
    let max = entries.first().map(|e| e.size).unwrap_or(0).max(1);
    for (i, e) in entries.iter().take(top).enumerate() {
        let bar_len = ((e.size as f64 / max as f64) * 14.0).round() as usize;
        let marker = if e.is_dir { "▸" } else { " " };
        let name = if e.is_dir {
            style.cyan(&e.name)
        } else {
            e.name.clone()
        };
        println!(
            "{:>4}  {} {:<40} {:>10}  {}",
            i + 1,
            marker,
            name,
            fmt_size(e.size),
            style.dim(&"█".repeat(bar_len))
        );
    }
    if entries.len() > top {
        println!(
            "{}",
            style.dim(&format!(
                "  ... {} more entries hidden (--top {})",
                entries.len() - top,
                top
            ))
        );
    }
    if entries.is_empty() {
        println!("{}", style.dim("  (empty directory)"));
    }
}

// ---------------------------------------------------------------------------
// JSON output (`--json`), mirroring mole's scripting support.
// ---------------------------------------------------------------------------

fn print_json(root: &Path, scan: &ScanResult) {
    let entries = list_entries(root, scan);
    let total = scan.dir_sizes.get(root).copied().unwrap_or(0);
    let mut out = String::new();
    out.push_str(&format!(
        "{{\"path\":\"{}\",\"total\":{},\"files\":{},\"dirs\":{},\"entries\":[",
        json_escape(&root.display().to_string()),
        total,
        scan.files,
        scan.dirs
    ));
    for (i, e) in entries.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        out.push_str(&format!(
            "{{\"name\":\"{}\",\"type\":\"{}\",\"size\":{}}}",
            json_escape(&e.name),
            if e.is_dir { "dir" } else { "file" },
            e.size
        ));
    }
    out.push_str("]}");
    println!("{out}");
}

pub fn json_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn setup(tag: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!("dpan-analyze-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("big").join("nested")).unwrap();
        fs::create_dir_all(root.join("small")).unwrap();
        fs::write(root.join("big").join("a.bin"), vec![0u8; 300]).unwrap();
        fs::write(
            root.join("big").join("nested").join("b.bin"),
            vec![0u8; 200],
        )
        .unwrap();
        fs::write(root.join("small").join("c.bin"), vec![0u8; 50]).unwrap();
        fs::write(root.join("top.txt"), vec![0u8; 10]).unwrap();
        root
    }

    #[test]
    fn scan_indexes_every_directory() {
        let root = setup("scan");
        let scan = scan_tree(&root, false);
        assert_eq!(scan.dir_sizes.get(&root), Some(&560));
        assert_eq!(scan.dir_sizes.get(&root.join("big")), Some(&500));
        assert_eq!(
            scan.dir_sizes.get(&root.join("big").join("nested")),
            Some(&200)
        );
        assert_eq!(scan.dir_sizes.get(&root.join("small")), Some(&50));
        assert_eq!(scan.files, 4);
        assert_eq!(scan.dirs, 3);
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn entries_sorted_by_size_desc() {
        let root = setup("sort");
        let scan = scan_tree(&root, false);
        let entries = list_entries(&root, &scan);
        let names: Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, vec!["big", "small", "top.txt"]);
        assert!(entries[0].is_dir && !entries[2].is_dir);
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn json_escaping() {
        assert_eq!(json_escape("a\"b\\c"), "a\\\"b\\\\c");
        assert_eq!(json_escape("tab\there"), "tab\\u0009here");
    }
}
