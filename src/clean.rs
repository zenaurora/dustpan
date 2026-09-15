//! Deletion engine: every removal funnels through here (mole's file_ops
//! pattern) — safety gate, whitelist, dry-run, then delete + audit log.

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::fsutil::is_link_or_reparse;
use crate::safety::Safety;
use crate::scan::entry_size;
use crate::targets::{self, Category, Mode, ResolvedTarget, Risk, TargetMetadata};

/// discover -> plan 阶段选中的目标；size 在 preview 阶段回填。
#[derive(Clone, Debug)]
pub struct PlannedTarget {
    pub target: ResolvedTarget,
    pub metadata: TargetMetadata,
    pub size: u64,
}

/// 菜单与命令行共用的清理计划，执行前不会再重新解析目标。
#[derive(Clone, Debug, Default)]
pub struct CleanPlan {
    /// 默认会执行的低风险目标。
    pub targets: Vec<PlannedTarget>,
    /// 已发现但需要用户明确选择的中风险或高成本目标。
    pub optional: Vec<PlannedTarget>,
}

impl CleanPlan {
    pub fn discover(only: Option<&[Category]>) -> CleanPlan {
        // Smart Clean 是固定默认策略，不再通过命令行开关启用。扫描仍会
        // 发现高成本缓存，只是先放进 optional，等用户逐项确认后再执行。
        let mut plan = CleanPlan::default();
        for target in targets::resolve_targets(only) {
            let metadata = target.metadata();
            let planned = PlannedTarget {
                target,
                metadata,
                size: 0,
            };
            if metadata.risk == Risk::Low && !metadata.expensive {
                plan.targets.push(planned);
            } else {
                plan.optional.push(planned);
            }
        }
        plan
    }

    pub fn preview(&mut self) {
        // 默认项和可选项合并扫描一次，避免用户选中高成本项后再次遍历磁盘。
        // sizes 的顺序与链式迭代器一致，先回填 targets，再回填 optional。
        let resolved: Vec<ResolvedTarget> = self
            .targets
            .iter()
            .chain(&self.optional)
            .map(|p| p.target.clone())
            .collect();
        let sizes = crate::scan::scan_sizes(&resolved);
        for (planned, size) in self.targets.iter_mut().chain(&mut self.optional).zip(sizes) {
            planned.size = size;
        }
    }

    /// 将用户勾选的可选项移动到执行计划。索引从 0 开始，并先排序去重，
    /// 逆序 remove 可避免前面的删除改变后续索引，这是这里的关键细节。
    pub fn include_optional(&mut self, indices: &[usize]) {
        let mut indices = indices.to_vec();
        indices.sort_unstable();
        indices.dedup();
        let mut selected = Vec::with_capacity(indices.len());
        for index in indices.into_iter().rev() {
            if index < self.optional.len() {
                selected.push(self.optional.remove(index));
            }
        }
        // 上面为保证索引稳定而逆序移除，这里再翻转一次，确保执行和报告
        // 顺序与用户看到的编号一致。
        selected.reverse();
        self.targets.extend(selected);
    }

    pub fn total(&self) -> u64 {
        self.targets.iter().map(|p| p.size).sum()
    }
    pub fn requires_admin(&self) -> bool {
        self.targets.iter().any(|p| p.metadata.requires_admin)
    }
    pub fn resolved(&self) -> Vec<ResolvedTarget> {
        self.targets.iter().map(|p| p.target.clone()).collect()
    }
    pub fn all_resolved(&self) -> Vec<ResolvedTarget> {
        self.targets
            .iter()
            .chain(&self.optional)
            .map(|p| p.target.clone())
            .collect()
    }
}

/// 查询常见的缓存占用进程。策略是保守的：空结果只代表没有识别到已知
/// 进程，并不代表系统中绝对没有其它程序持有文件句柄。
pub fn running_cache_apps(plan: &CleanPlan) -> Vec<String> {
    let names: Vec<&str> = plan
        .targets
        .iter()
        .filter_map(|p| p.metadata.associated_app)
        .collect();
    if names.is_empty() || !cfg!(windows) {
        return Vec::new();
    }
    let output = Command::new("tasklist")
        .args(["/FO", "CSV", "/NH"])
        .output();
    let Ok(output) = output else {
        return Vec::new();
    };
    let text = String::from_utf8_lossy(&output.stdout).to_ascii_lowercase();
    let mut running = Vec::new();
    for name in names {
        // 同一应用通常有 Cache、Code Cache、GPUCache 多个目标，提示时
        // 只显示一次进程名，避免出现 chrome.exe 重复三行。
        if text.contains(&name.to_ascii_lowercase())
            && !running
                .iter()
                .any(|seen: &String| seen.eq_ignore_ascii_case(name))
        {
            running.push(name.to_string());
        }
    }
    running
}

/// 判断当前进程是否拥有管理员令牌。非 Windows 构建固定返回 false，方便
/// 在 macOS/Linux 上运行单元测试而不引入平台依赖。
pub fn is_elevated() -> bool {
    #[cfg(windows)]
    {
        return std::process::Command::new("net")
            .args(["session"])
            .output()
            .is_ok_and(|o| o.status.success());
    }
    #[cfg(not(windows))]
    {
        false
    }
}

/// 返回提权提示文案。真正的 UAC 只在删除 Windows 系统缓存且遇到拒绝时
/// 触发，当前计划不会因为提权而重新生成。
pub fn explain_elevation() -> &'static str {
    "Windows system caches may need administrator permission; dpan will request UAC only if deletion is denied."
}

#[derive(Default)]
pub struct CleanStats {
    pub freed: u64,
    pub deleted: usize,
    /// Locked / permission-denied entries (common on Windows while apps run).
    pub failed: usize,
    /// Whitelisted or safety-rejected entries.
    pub skipped: usize,
    pub failed_paths: Vec<PathBuf>,
    pub failure_reasons: Vec<String>,
}

impl CleanStats {
    pub fn merge(&mut self, other: &CleanStats) {
        self.freed += other.freed;
        self.deleted += other.deleted;
        self.failed += other.failed;
        self.skipped += other.skipped;
        self.failed_paths.extend(other.failed_paths.iter().cloned());
        self.failure_reasons
            .extend(other.failure_reasons.iter().cloned());
    }
}

pub struct Cleaner<'a> {
    safety: &'a Safety,
    dry_run: bool,
    allow_elevation: bool,
    log: AuditLog,
    pub verbose_lines: Vec<String>,
}

impl<'a> Cleaner<'a> {
    pub fn new(safety: &'a Safety, dry_run: bool) -> Cleaner<'a> {
        Cleaner {
            safety,
            dry_run,
            allow_elevation: false,
            log: AuditLog::new(dry_run),
            verbose_lines: Vec::new(),
        }
    }

    pub fn set_elevation_for_target(&mut self, allowed: bool) {
        self.allow_elevation = allowed;
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
                    stats
                        .failure_reasons
                        .push(format!("{}: cannot read directory", target.path.display()));
                    return stats;
                };
                for entry in entries {
                    match entry {
                        Ok(entry) => {
                            let s = self.remove_one(&entry.path());
                            stats.merge(&s);
                        }
                        Err(error) => {
                            stats.failed += 1;
                            stats.failure_reasons.push(format!(
                                "{}: {}",
                                target.path.display(),
                                error
                            ));
                        }
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
            Err(error) => {
                #[cfg(windows)]
                if self.allow_elevation && error.kind() == std::io::ErrorKind::PermissionDenied {
                    if elevated_remove_entry(path).is_ok() {
                        stats.freed += size;
                        stats.deleted += 1;
                        self.log.write("REMOVED", size, path);
                        return stats;
                    }
                }
                stats.failed += 1;
                stats.failed_paths.push(path.to_path_buf());
                let reason = match error.kind() {
                    std::io::ErrorKind::PermissionDenied => "permission denied".to_string(),
                    std::io::ErrorKind::NotFound => "file disappeared".to_string(),
                    std::io::ErrorKind::IsADirectory => "directory is in use".to_string(),
                    _ => error.to_string(),
                };
                stats
                    .failure_reasons
                    .push(format!("{}: {reason}", path.display()));
                self.verbose_lines
                    .push(format!("failed ({reason}): {}", path.display()));
                self.log.write("FAILED", 0, path);
            }
        }
        stats
    }

    /// 重试首轮失败的条目（通常是用户关闭 Chrome/Edge/Discord/IDE 后）。
    /// 重试仍经过同一安全闸门，成功删除的结果单独返回供报告合并。
    pub fn retry_failed(&mut self, paths: &[PathBuf]) -> CleanStats {
        let mut stats = CleanStats::default();
        for path in paths {
            stats.merge(&self.remove_one(path));
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

#[cfg(windows)]
fn elevated_remove_entry(path: &Path) -> std::io::Result<()> {
    // PowerShell 的 -EncodedCommand 约定是 UTF-16LE Base64；这样路径中的
    // 引号、中文或 PowerShell 特殊字符都不会改变待执行的命令。
    let escaped = path.to_string_lossy().replace('\'', "''");
    let script = format!("Remove-Item -LiteralPath '{escaped}' -Recurse -Force -ErrorAction Stop");
    let mut bytes = Vec::with_capacity(script.len() * 2);
    for unit in script.encode_utf16() {
        bytes.extend_from_slice(&unit.to_le_bytes());
    }
    // 这里使用无依赖的小型 Base64 编码器；每 3 个字节编码为 4 个字符，
    // 末尾不足 3 字节时用 '=' 补齐，这是协议要求的 magic 规则。
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut encoded = String::new();
    for chunk in bytes.chunks(3) {
        let a = chunk[0] as u32;
        let b = chunk.get(1).copied().unwrap_or(0) as u32;
        let c = chunk.get(2).copied().unwrap_or(0) as u32;
        let n = (a << 16) | (b << 8) | c;
        encoded.push(TABLE[((n >> 18) & 63) as usize] as char);
        encoded.push(TABLE[((n >> 12) & 63) as usize] as char);
        encoded.push(if chunk.len() > 1 {
            TABLE[((n >> 6) & 63) as usize] as char
        } else {
            '='
        });
        encoded.push(if chunk.len() > 2 {
            TABLE[(n & 63) as usize] as char
        } else {
            '='
        });
    }
    let launcher = format!("$p=Start-Process powershell -Verb RunAs -Wait -PassThru -ArgumentList '-NoProfile','-EncodedCommand','{encoded}'; exit $p.ExitCode");
    let status = Command::new("powershell")
        .args(["-NoProfile", "-Command", &launcher])
        .status()?;
    if status.success() {
        Ok(())
    } else {
        Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "elevated removal was cancelled or failed",
        ))
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

    #[test]
    fn smart_plan_excludes_expensive_and_system_targets() {
        let plan = CleanPlan::discover(None);
        assert!(plan
            .targets
            .iter()
            .all(|target| target.metadata.risk == Risk::Low));
        assert!(plan.targets.iter().all(|target| !target.metadata.expensive));
    }

    #[test]
    fn metadata_identifies_process_and_permission_requirements() {
        let target = ResolvedTarget {
            category: Category::Browser,
            name: "Chrome cache",
            path: PathBuf::from("/tmp/chrome"),
            mode: Mode::Contents,
        };
        assert_eq!(target.metadata().associated_app, Some("chrome.exe"));
        assert!(!target.metadata().requires_admin);
    }

    #[test]
    fn optional_selection_preserves_user_order() {
        let make = |name| PlannedTarget {
            target: ResolvedTarget {
                category: Category::Dev,
                name,
                path: PathBuf::from(format!("/tmp/{name}")),
                mode: Mode::Entry,
            },
            metadata: TargetMetadata {
                risk: Risk::Medium,
                redownload_cost: "high",
                associated_app: None,
                requires_admin: false,
                expensive: true,
            },
            size: 1,
        };
        let mut plan = CleanPlan {
            targets: Vec::new(),
            optional: vec![make("a"), make("b"), make("c")],
        };
        plan.include_optional(&[2, 0]);
        assert_eq!(plan.targets[0].target.name, "a");
        assert_eq!(plan.targets[1].target.name, "c");
        assert_eq!(plan.optional.len(), 1);
    }
}
