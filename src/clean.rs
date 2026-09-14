//! Deletion engine: every removal funnels through here (mole's file_ops
//! pattern) — safety gate, whitelist, dry-run, then delete + audit log.

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::fsutil::is_link_or_reparse;
use crate::safety::Safety;
use crate::scan::entry_size;
use crate::targets::{Mode, ResolvedTarget};

#[derive(Default)]
pub struct CleanStats {
    pub freed: u64,
    pub deleted: usize,
    /// Locked / permission-denied entries (common on Windows while apps run).
    pub failed: usize,
    /// Whitelisted or safety-rejected entries.
    pub skipped: usize,
}

impl CleanStats {
    pub fn merge(&mut self, other: &CleanStats) {
        self.freed += other.freed;
        self.deleted += other.deleted;
        self.failed += other.failed;
        self.skipped += other.skipped;
    }
}

pub struct Cleaner<'a> {
    safety: &'a Safety,
    dry_run: bool,
    log: AuditLog,
    pub verbose_lines: Vec<String>,
}

impl<'a> Cleaner<'a> {
    pub fn new(safety: &'a Safety, dry_run: bool) -> Cleaner<'a> {
        Cleaner {
            safety,
            dry_run,
            log: AuditLog::new(dry_run),
            verbose_lines: Vec::new(),
        }
    }

    pub fn clean_target(&mut self, target: &ResolvedTarget) -> CleanStats {
        let mut stats = CleanStats::default();
        match target.mode {
            Mode::Entry => {
                let s = self.remove_one(&target.path);
                stats.merge(&s);
            }
            Mode::Contents => {
                if target
                    .path
                    .symlink_metadata()
                    .is_ok_and(|meta| is_link_or_reparse(&meta))
                {
                    stats.skipped += 1;
                    self.verbose_lines.push(format!(
                        "skip (target is a reparse point): {}",
                        target.path.display()
                    ));
                    return stats;
                }
                let Ok(entries) = fs::read_dir(&target.path) else {
                    stats.failed += 1;
                    return stats;
                };
                for entry in entries {
                    match entry {
                        Ok(entry) => {
                            let s = self.remove_one(&entry.path());
                            stats.merge(&s);
                        }
                        Err(_) => stats.failed += 1,
                    }
                }
            }
        }
        stats
    }

    /// Single-entry gate: whitelist -> safety -> dry-run -> delete -> log.
    fn remove_one(&mut self, path: &Path) -> CleanStats {
        let mut stats = CleanStats::default();
        if self.safety.is_whitelisted(path) {
            stats.skipped += 1;
            self.verbose_lines
                .push(format!("skip (whitelisted): {}", path.display()));
            return stats;
        }
        if let Err(reason) = self.safety.check(path) {
            stats.skipped += 1;
            self.verbose_lines
                .push(format!("skip ({reason}): {}", path.display()));
            return stats;
        }
        let size = entry_size(path);
        if self.dry_run {
            stats.freed += size;
            stats.deleted += 1;
            self.verbose_lines
                .push(format!("would remove: {}", path.display()));
            return stats;
        }
        match remove_entry(path) {
            Ok(()) => {
                stats.freed += size;
                stats.deleted += 1;
                self.log.write("REMOVED", size, path);
            }
            Err(_) => {
                stats.failed += 1;
                self.verbose_lines
                    .push(format!("failed (locked?): {}", path.display()));
                self.log.write("FAILED", 0, path);
            }
        }
        stats
    }

    /// Public single-path removal for the uninstaller: same gate, same
    /// audit log. Returns true when the path was actually removed.
    pub fn remove_path(&mut self, path: &Path) -> bool {
        let stats = self.remove_one(path);
        stats.deleted > 0 && !self.dry_run
    }
}

fn remove_entry(path: &Path) -> std::io::Result<()> {
    let meta = path.symlink_metadata()?;
    if is_link_or_reparse(&meta) {
        // Never recurse through a symlink, junction, or other reparse point.
        // Directory links are removed as links, not as their targets.
        fs::remove_file(path).or_else(|_| fs::remove_dir(path))
    } else if meta.is_dir() {
        fs::remove_dir_all(path)
    } else {
        fs::remove_file(path)
    }
}

/// Append-only audit log, mirroring mole's operations.log.
/// `%LOCALAPPDATA%\dustpan\operations.log` (or `~/.local/share/dustpan/`).
struct AuditLog {
    file: Option<std::fs::File>,
}

impl AuditLog {
    fn new(dry_run: bool) -> AuditLog {
        if dry_run {
            return AuditLog { file: None };
        }
        let file = log_path().and_then(|p| {
            fs::create_dir_all(p.parent()?).ok()?;
            OpenOptions::new().create(true).append(true).open(&p).ok()
        });
        AuditLog { file }
    }

    fn write(&mut self, action: &str, size: u64, path: &Path) {
        if let Some(f) = &mut self.file {
            let _ = writeln!(f, "{}\t{}\t{}\t{}", iso_now(), action, size, path.display());
        }
    }
}

pub fn log_path() -> Option<PathBuf> {
    if let Ok(local) = std::env::var("LOCALAPPDATA") {
        return Some(PathBuf::from(local).join("dustpan").join("operations.log"));
    }
    std::env::var("HOME").ok().map(|h| {
        PathBuf::from(h)
            .join(".local")
            .join("share")
            .join("dustpan")
            .join("operations.log")
    })
}

/// UTC timestamp without external crates (Howard Hinnant's civil algorithm).
fn iso_now() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    iso_from_unix(secs)
}

pub fn iso_from_unix(secs: u64) -> String {
    let (y, m, d) = civil_from_days((secs / 86400) as i64);
    let rem = secs % 86400;
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z",
        y,
        m,
        d,
        rem / 3600,
        rem % 3600 / 60,
        rem % 60
    )
}

fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = (z - era * 146097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::targets::Category;
    use std::fs;

    fn make_target(root: &Path) -> ResolvedTarget {
        ResolvedTarget {
            category: Category::Dev,
            name: "test cache",
            path: root.to_path_buf(),
            mode: Mode::Contents,
        }
    }

    fn setup(tag: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!("dpan-clean-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("sub")).unwrap();
        fs::write(root.join("a.bin"), vec![0u8; 100]).unwrap();
        fs::write(root.join("sub").join("b.bin"), vec![0u8; 50]).unwrap();
        fs::write(root.join("keep.txt"), vec![0u8; 10]).unwrap();
        root
    }

    #[test]
    fn dry_run_deletes_nothing() {
        let root = setup("dry");
        let safety = Safety::for_test(vec![root.clone()], vec![]);
        let mut cleaner = Cleaner::new(&safety, true);
        let stats = cleaner.clean_target(&make_target(&root));
        assert_eq!(stats.deleted, 3);
        assert_eq!(stats.freed, 160);
        assert!(root.join("a.bin").exists());
        assert!(root.join("sub").join("b.bin").exists());
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn real_run_removes_children_keeps_dir_and_whitelist() {
        let root = setup("real");
        let wl = vec![root.join("keep.txt").to_string_lossy().into_owned()];
        let safety = Safety::for_test(vec![root.clone()], wl);
        let mut cleaner = Cleaner::new(&safety, false);
        let stats = cleaner.clean_target(&make_target(&root));
        assert_eq!(stats.deleted, 2); // a.bin + sub/
        assert_eq!(stats.skipped, 1); // keep.txt
        assert_eq!(stats.freed, 150);
        assert!(root.exists());
        assert!(root.join("keep.txt").exists());
        assert!(!root.join("a.bin").exists());
        assert!(!root.join("sub").exists());
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn timestamp_is_sane() {
        let ts = iso_now();
        assert!(ts.starts_with("20"), "unexpected timestamp: {ts}");
        assert_eq!(ts.len(), 20);
    }
}
