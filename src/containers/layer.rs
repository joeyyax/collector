use super::{Container, Registry};
use crate::util::{self, log, shutdown};
use serde::Serialize;
use std::collections::{HashMap, HashSet};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, RwLock};
use std::time::Duration;

#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct Size {
    pub bytes: u64,
    /// Unix seconds.
    pub measured_at: f64,
}

/// Writable-layer sizes by container id, measured one container at a time so a full pass takes `interval`.
#[derive(Clone, Default)]
pub struct Sizes(Arc<RwLock<HashMap<String, Size>>>);

impl Sizes {
    pub fn get(&self, id: &str) -> Option<Size> {
        self.0.read().unwrap_or_else(std::sync::PoisonError::into_inner).get(id).copied()
    }

    #[cfg(test)]
    pub fn set(&self, id: &str, size: Size) {
        self.0.write().unwrap_or_else(std::sync::PoisonError::into_inner).insert(id.to_string(), size);
    }

    pub fn start(&self, registry: Registry, rootfs: PathBuf, interval: Duration) {
        let me = self.clone();
        std::thread::Builder::new().name("layer-size".into()).spawn(move || me.run(&registry, &rootfs, interval)).expect("spawn layer-size");
    }

    fn run(&self, registry: &Registry, rootfs: &Path, interval: Duration) {
        let warned = AtomicBool::new(false);
        loop {
            let ctrs = registry.all();
            let now = util::now_secs();
            let next = {
                let mut sizes = self.0.write().unwrap_or_else(std::sync::PoisonError::into_inner);
                let ids: HashSet<&str> = ctrs.iter().map(|c| c.id.as_str()).collect();
                sizes.retain(|id, _| ids.contains(id.as_str()));
                next_due(&ctrs, &sizes, now, interval.as_secs_f64())
            };
            if let Some(c) = next {
                let upper = c.upper_dir.as_deref().unwrap_or_default();
                match measure(&rootfs.join(upper.trim_start_matches('/'))) {
                    Some((bytes, errors)) => {
                        if errors > 0 && !warned.swap(true, Ordering::Relaxed) {
                            log::warn!("writable layer: {} has {errors} unreadable directories, so its size is low; give the collector CAP_DAC_READ_SEARCH", c.name);
                        }
                        self.0.write().unwrap_or_else(std::sync::PoisonError::into_inner).insert(c.id.clone(), Size { bytes, measured_at: util::now_secs() });
                    }
                    None => log::debug!("writable layer: {} not found under {}", upper, rootfs.display()),
                }
            }
            let pace = interval.div_f64(ctrs.len().max(1) as f64).max(Duration::from_secs(1));
            if shutdown::sleep(pace) {
                return;
            }
        }
    }
}

/// The next container to measure: unmeasured ones first, then the oldest measurement older than `interval`.
pub fn next_due(ctrs: &[Arc<Container>], sizes: &HashMap<String, Size>, now: f64, interval: f64) -> Option<Arc<Container>> {
    let mut candidates: Vec<(f64, &Arc<Container>)> = ctrs
        .iter()
        .filter(|c| c.upper_dir.is_some())
        .filter_map(|c| match sizes.get(&c.id) {
            None => Some((f64::NEG_INFINITY, c)),
            Some(s) if now - s.measured_at >= interval => Some((s.measured_at, c)),
            Some(_) => None,
        })
        .collect();
    candidates.sort_by(|a, b| a.0.total_cmp(&b.0).then_with(|| a.1.name.cmp(&b.1.name)));
    candidates.first().map(|(_, c)| (*c).clone())
}

/// Apparent size of every non-directory under `root`, hard links counted once, not crossing filesystems.
/// Returns the total and how many directories couldn't be read; None if `root` is missing.
pub fn measure(root: &Path) -> Option<(u64, u64)> {
    let dev = std::fs::symlink_metadata(root).ok()?.dev();
    let (mut total, mut errors) = (0u64, 0u64);
    let mut seen = HashSet::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            errors += 1;
            continue;
        };
        for e in entries.flatten() {
            let Ok(md) = e.metadata() else { continue };
            if md.is_dir() {
                if md.dev() == dev {
                    stack.push(e.path());
                }
            } else if md.nlink() <= 1 || seen.insert((md.dev(), md.ino())) {
                total += md.len();
            }
        }
    }
    Some((total, errors))
}

/// The overlay `upperdir` of a process's root mount from `/proc/<pid>/mountinfo`, when it's an absolute path.
pub fn upper_dir_from_mountinfo(text: &str) -> Option<String> {
    text.lines().find_map(|l| {
        let (pre, post) = l.split_once(" - ")?;
        if pre.split(' ').nth(4)? != "/" {
            return None;
        }
        let mut f = post.split(' ');
        if f.next()? != "overlay" {
            return None;
        }
        let opts = f.nth(1)?;
        opts.split(',').find_map(|o| o.strip_prefix("upperdir=")).filter(|p| p.starts_with('/')).map(str::to_string)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn measures_files_once_and_skips_directories() {
        let root = std::env::temp_dir().join(format!("collector-layer-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("var/lib/app/cache")).unwrap();
        std::fs::write(root.join("var/lib/app/data.db"), vec![0u8; 3000]).unwrap();
        std::fs::write(root.join("var/lib/app/cache/a"), vec![0u8; 500]).unwrap();
        std::fs::hard_link(root.join("var/lib/app/cache/a"), root.join("var/lib/app/cache/b")).unwrap();
        std::os::unix::fs::symlink("/etc/passwd", root.join("var/lib/app/link")).unwrap();
        let (bytes, errors) = measure(&root).unwrap();
        let link = std::fs::symlink_metadata(root.join("var/lib/app/link")).unwrap().len();
        let _ = std::fs::remove_dir_all(&root);
        assert_eq!(bytes, 3000 + 500 + link);
        assert_eq!(errors, 0);
        assert_eq!(measure(Path::new("/nonexistent/collector")), None);
    }

    #[test]
    fn measures_new_containers_first_then_the_stalest() {
        let c = |id: &str, upper: bool| Arc::new(Container { id: id.into(), name: id.into(), upper_dir: upper.then(|| format!("/var/lib/docker/overlay2/{id}/diff")), ..Default::default() });
        let ctrs = vec![c("a", true), c("b", true), c("c", true), c("d", false)];
        let at = |t| Size { bytes: 1, measured_at: t };
        let mut sizes = HashMap::from([("a".to_string(), at(100.0)), ("b".to_string(), at(50.0))]);
        assert_eq!(next_due(&ctrs, &sizes, 200.0, 3600.0).unwrap().id, "c");
        sizes.insert("c".into(), at(190.0));
        assert!(next_due(&ctrs, &sizes, 200.0, 3600.0).is_none());
        assert_eq!(next_due(&ctrs, &sizes, 3700.0, 3600.0).unwrap().id, "b");
        assert_eq!(next_due(&ctrs, &sizes, 3690.0, 3600.0).unwrap().id, "b");
    }

    #[test]
    fn upper_dir_from_root_mount() {
        let text = "1043 943 0:120 / / rw,relatime master:296 - overlay overlay rw,lowerdir=/var/lib/docker/overlay2/l/ABC:/var/lib/docker/overlay2/l/DEF,upperdir=/var/lib/docker/overlay2/9f3c/diff,workdir=/var/lib/docker/overlay2/9f3c/work,nouserxattr\n\
                    1044 1043 0:123 / /proc rw,nosuid,nodev,noexec,relatime - proc proc rw\n";
        assert_eq!(upper_dir_from_mountinfo(text).as_deref(), Some("/var/lib/docker/overlay2/9f3c/diff"));
        let relative = "1 0 0:1 / / rw - overlay overlay rw,lowerdir=l/A,upperdir=9f3c/diff,workdir=9f3c/work\n";
        assert_eq!(upper_dir_from_mountinfo(relative), None);
        assert_eq!(upper_dir_from_mountinfo("1 0 8:1 / / rw - ext4 /dev/sda1 rw\n"), None);
    }
}
