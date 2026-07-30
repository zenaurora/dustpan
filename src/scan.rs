//! Size scanning: recursive directory sizes, parallel across targets.

use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;
use std::thread;

use crate::targets::ResolvedTarget;

/// Size of a single entry: file length, or recursive size for a directory.
/// Symlinks are never followed. Unreadable entries count as 0.
pub fn entry_size(path: &Path) -> u64 {
    let Ok(meta) = path.symlink_metadata() else {
        return 0;
    };
    if meta.is_dir() {
        dir_size(path)
    } else {
        meta.len()
    }
}

fn dir_size(path: &Path) -> u64 {
    let Ok(entries) = std::fs::read_dir(path) else {
        return 0;
    };
    let mut total = 0u64;
    for entry in entries.flatten() {
        let Ok(meta) = entry.metadata() else {
            continue;
        };
        if meta.is_symlink() {
            continue;
        }
        if meta.is_dir() {
            total += dir_size(&entry.path());
        } else {
            total += meta.len();
        }
    }
    total
}

/// Compute target sizes in parallel; result index matches input index.
pub fn scan_sizes(targets: &[ResolvedTarget]) -> Vec<u64> {
    let sizes = Mutex::new(vec![0u64; targets.len()]);
    let next = AtomicUsize::new(0);
    let workers = thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4)
        .min(8)
        .min(targets.len().max(1));

    thread::scope(|scope| {
        for _ in 0..workers {
            scope.spawn(|| loop {
                let i = next.fetch_add(1, Ordering::Relaxed);
                if i >= targets.len() {
                    break;
                }
                let size = entry_size(&targets[i].path);
                sizes.lock().unwrap()[i] = size;
            });
        }
    });
    sizes.into_inner().unwrap()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn sizes_are_recursive_and_skip_symlinks() {
        let root = std::env::temp_dir().join(format!("dpan-scan-{}", std::process::id()));
        fs::create_dir_all(root.join("sub")).unwrap();
        fs::write(root.join("a.bin"), vec![0u8; 100]).unwrap();
        fs::write(root.join("sub").join("b.bin"), vec![0u8; 50]).unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(root.join("a.bin"), root.join("link")).unwrap();
        assert_eq!(entry_size(&root), 150);
        assert_eq!(entry_size(&root.join("a.bin")), 100);
        assert_eq!(entry_size(&root.join("missing")), 0);
        fs::remove_dir_all(&root).unwrap();
    }
}
