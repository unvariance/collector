use std::fmt;
use std::io;
use std::path::{Path, PathBuf};

pub use error::{Error, Result};

mod error;
mod provider;
pub use provider::{FsProvider, RealFs};

#[cfg(test)]
mod test_utils;

const DEFAULT_ROOT: &str = "/sys/fs/resctrl";
const DEFAULT_PREFIX: &str = "pod_";
const MAX_UID_LEN: usize = 63; // limit UID segment (<64)

#[derive(Clone, Debug)]
pub struct AssignmentResult {
    pub assigned: usize,
    pub missing: usize,
}

impl AssignmentResult {
    pub fn new(assigned: usize, missing: usize) -> Self {
        Self { assigned, missing }
    }
}

#[derive(Clone, Debug)]
pub struct Config {
    pub root: PathBuf,
    pub group_prefix: String,
    pub auto_mount: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            root: PathBuf::from(DEFAULT_ROOT),
            group_prefix: DEFAULT_PREFIX.to_string(),
            auto_mount: false,
        }
    }
}

#[derive(Clone)]
pub struct Resctrl<P: FsProvider = RealFs> {
    fs: P,
    cfg: Config,
}

impl Default for Resctrl<RealFs> {
    fn default() -> Self {
        Self::new(Config::default())
    }
}

impl Resctrl<RealFs> {
    pub fn new(cfg: Config) -> Self {
        Self { fs: RealFs, cfg }
    }
}

impl<P: FsProvider> fmt::Debug for Resctrl<P> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Resctrl")
            .field("root", &self.cfg.root)
            .field("group_prefix", &self.cfg.group_prefix)
            .finish()
    }
}

impl<P: FsProvider> Resctrl<P> {
    pub fn with_provider(fs: P, cfg: Config) -> Self {
        Self { fs, cfg }
    }

    // Public API

    /// Describe support status of resctrl on this system.
    /// - mounted: whether resctrl is mounted
    /// - mount_point: where it is mounted if present
    /// - writable: whether current process can write to root tasks file
    pub fn detect_support(&self) -> Result<SupportInfo> {
        // Determine mount point by reading /proc/mounts
        let mounts = match self.fs.read_to_string(Path::new("/proc/mounts")) {
            Ok(s) => s,
            Err(e) => {
                return Err(Error::Io {
                    path: PathBuf::from("/proc/mounts"),
                    source: e,
                })
            }
        };

        let mut mount_point: Option<PathBuf> = None;
        for line in mounts.lines() {
            // /proc/mounts format: <src> <target> <fstype> <opts> ...
            let parts: Vec<&str> = line.split_whitespace().collect();
            if parts.len() >= 3 && parts[2] == "resctrl" {
                mount_point = Some(PathBuf::from(parts[1]));
                break;
            }
        }

        let mounted = mount_point.is_some();
        let writable = if let Some(ref mp) = mount_point {
            let tasks = mp.join("tasks");
            // Try to open for write without writing anything
            self.fs.check_can_open_for_write(&tasks).is_ok()
        } else {
            false
        };

        Ok(SupportInfo {
            mounted,
            mount_point,
            writable,
        })
    }

    /// Ensure resctrl is mounted according to configuration.
    /// - If already mounted, returns Ok(())
    /// - If not mounted and auto_mount is false, returns Error::NotMounted
    /// - If not mounted and auto_mount is true, attempts to mount and returns
    ///   NoPermission/Unsupported/Io on failure.
    pub fn ensure_mounted(&self) -> Result<()> {
        let info = self.detect_support()?;
        if info.mounted {
            return Ok(());
        }
        if !self.cfg.auto_mount {
            return Err(Error::NotMounted {
                root: self.cfg.root.clone(),
            });
        }

        // Try to mount at configured root
        match self.fs.mount_resctrl(&self.cfg.root) {
            Ok(()) => {
                // Verify mounted after mount attempt
                let info2 = self.detect_support()?;
                if !info2.mounted {
                    return Err(Error::Io {
                        path: self.cfg.root.clone(),
                        source: io::Error::other("mount did not take effect"),
                    });
                }
                Ok(())
            }
            Err(e) => {
                if let Some(code) = e.raw_os_error() {
                    match code {
                        libc::EACCES | libc::EPERM => {
                            return Err(Error::NoPermission {
                                path: self.cfg.root.clone(),
                                source: e,
                            })
                        }
                        libc::ENODEV | libc::EINVAL | libc::ENOTSUP | libc::ENOSYS => {
                            return Err(Error::Unsupported { source: e });
                        }
                        _ => {}
                    }
                }
                Err(Error::Io {
                    path: self.cfg.root.clone(),
                    source: e,
                })
            }
        }
    }

    pub fn create_group(&self, pod_uid: &str) -> Result<String> {
        // Ensure root exists
        if !self.fs.exists(&self.cfg.root) {
            return Err(Error::NotMounted {
                root: self.cfg.root.clone(),
            });
        }

        let group_name = group_name(&self.cfg.group_prefix, pod_uid);
        let path = self.cfg.root.join(&group_name);

        match self.fs.create_dir(&path) {
            Ok(()) => Ok(path.to_string_lossy().into_owned()),
            Err(e) => match map_basic_fs_error(&path, &e) {
                // Treat AlreadyExists as success (idempotent)
                Error::Io { source, .. } if source.kind() == io::ErrorKind::AlreadyExists => {
                    Ok(path.to_string_lossy().into_owned())
                }
                other => Err(other),
            },
        }
    }

    pub fn delete_group(&self, group_path: &str) -> Result<()> {
        let p = PathBuf::from(group_path);
        match self.fs.remove_dir(&p) {
            Ok(()) => Ok(()),
            Err(e) => {
                if let Some(code) = e.raw_os_error() {
                    if code == libc::ENOENT {
                        // Idempotent delete: missing group is fine
                        return Ok(());
                    }
                }
                Err(map_basic_fs_error(&p, &e))
            }
        }
    }

    pub fn assign_tasks(&self, group_path: &str, pids: &[i32]) -> Result<AssignmentResult> {
        let tasks_path = PathBuf::from(group_path).join("tasks");
        let mut assigned = 0usize;
        let mut missing = 0usize;

        for pid in pids {
            let s = pid.to_string();
            match self.fs.write_str(&tasks_path, &s) {
                Ok(()) => assigned += 1,
                Err(e) => {
                    // Classify errors
                    if let Some(code) = e.raw_os_error() {
                        match code {
                            // ESRCH: task does not exist anymore → count as missing
                            libc::ESRCH => {
                                missing += 1;
                                continue;
                            }
                            libc::EACCES | libc::EPERM => {
                                return Err(Error::NoPermission {
                                    path: tasks_path.clone(),
                                    source: e,
                                });
                            }
                            libc::ENOSPC => {
                                return Err(Error::Capacity { source: e });
                            }
                            libc::ENOENT => {
                                // Group/tasks file missing
                                return Err(Error::Io {
                                    path: tasks_path.clone(),
                                    source: e,
                                });
                            }
                            _ => {}
                        }
                    }
                    // Default: bubble as Io for tasks path
                    return Err(Error::Io {
                        path: tasks_path.clone(),
                        source: e,
                    });
                }
            }
        }

        Ok(AssignmentResult { assigned, missing })
    }

    pub fn list_group_tasks(&self, group_path: &str) -> Result<Vec<i32>> {
        let tasks_path = PathBuf::from(group_path).join("tasks");
        let s = self
            .fs
            .read_to_string(&tasks_path)
            .map_err(|e| map_basic_fs_error(&tasks_path, &e))?;

        let mut pids = Vec::new();
        for (idx, line) in s.lines().enumerate() {
            let t = line.trim();
            if t.is_empty() {
                continue;
            }
            match t.parse::<i32>() {
                Ok(pid) => pids.push(pid),
                Err(e) => {
                    return Err(Error::Io {
                        path: tasks_path.clone(),
                        source: io::Error::new(
                            io::ErrorKind::InvalidData,
                            format!("invalid pid at line {}: '{}': {}", idx + 1, t, e),
                        ),
                    });
                }
            }
        }
        Ok(pids)
    }

    /// Return a reference to the underlying filesystem provider.
    pub fn fs_provider(&self) -> &P {
        &self.fs
    }

    /// Reconcile tasks in a resctrl group with the desired PIDs produced by `pid_source`.
    ///
    /// The function repeatedly compares the current tasks in `group_path` with the
    /// PIDs returned by `pid_source`, assigning only the missing ones. The loop runs
    /// up to `max_passes` times or until convergence (no missing tasks) is reached.
    ///
    /// Returns `AssignmentResult { assigned, missing }` where
    /// - `assigned` is the total number of successful task assignments across passes
    /// - `missing` is the number of desired PIDs still not present in the group after
    ///   the final pass (0 indicates convergence)
    pub fn reconcile_group(
        &self,
        group_path: &str,
        mut pid_source: impl FnMut() -> Result<Vec<i32>>,
        max_passes: usize,
    ) -> Result<AssignmentResult> {
        use std::collections::HashSet;

        let mut total_assigned = 0usize;
        let mut last_desired: HashSet<i32> = HashSet::new();

        for _ in 0..max_passes {
            // Desired tasks for this pass
            let desired_vec = pid_source()?;
            last_desired = desired_vec.into_iter().collect();

            // Current tasks in the group
            let current_vec = self.list_group_tasks(group_path)?;
            let current: HashSet<i32> = current_vec.into_iter().collect();

            // Compute missing PIDs (desired but not yet in the group)
            let missing: Vec<i32> = last_desired.difference(&current).copied().collect();

            if missing.is_empty() {
                return Ok(AssignmentResult::new(total_assigned, 0));
            }

            // Try to assign missing tasks
            let res = self.assign_tasks(group_path, &missing)?;
            total_assigned += res.assigned;
            // Do not treat res.missing as terminal – recompute in next pass
        }

        // After exhausting passes, calculate how many are still missing
        let current_vec = self.list_group_tasks(group_path)?;
        let current: std::collections::HashSet<i32> = current_vec.into_iter().collect();
        let still_missing = last_desired.difference(&current).count();

        Ok(AssignmentResult::new(total_assigned, still_missing))
    }
}

fn sanitize_uid(uid: &str) -> String {
    let filtered: String = uid
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
        .collect();
    let trimmed = if filtered.len() > MAX_UID_LEN {
        filtered[..MAX_UID_LEN].to_string()
    } else {
        filtered
    };
    if trimmed.is_empty() {
        "unknown".to_string()
    } else {
        trimmed
    }
}

fn group_name(prefix: &str, pod_uid: &str) -> String {
    format!("{}{}", prefix, sanitize_uid(pod_uid))
}

fn map_basic_fs_error(path: &Path, e: &io::Error) -> Error {
    if let Some(code) = e.raw_os_error() {
        match code {
            libc::EACCES | libc::EPERM => Error::NoPermission {
                path: path.to_path_buf(),
                source: io::Error::from_raw_os_error(code),
            },
            libc::ENOSPC => Error::Capacity {
                source: io::Error::from_raw_os_error(code),
            },
            _ => Error::Io {
                path: path.to_path_buf(),
                source: io::Error::from_raw_os_error(code),
            },
        }
    } else {
        Error::Io {
            path: path.to_path_buf(),
            source: io::Error::new(e.kind(), format!("{}", e)),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SupportInfo {
    pub mounted: bool,
    pub mount_point: Option<PathBuf>,
    pub writable: bool,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_utils::mock_fs::MockFs;

    #[test]
    fn test_group_sanitization() {
        let s = sanitize_uid("abcDEF-123_+=!:@#$%^&*()");
        assert_eq!(s, "abcDEF-123_");

        let long = "x".repeat(100);
        let s2 = sanitize_uid(&long);
        assert_eq!(s2.len(), MAX_UID_LEN);
    }

    #[test]
    fn test_detect_support_not_mounted() {
        let fs = MockFs::default();
        // Provide empty /proc/mounts
        fs.add_file(Path::new("/proc/mounts"), "");
        let rc = Resctrl::with_provider(fs, Config::default());
        let info = rc.detect_support().expect("detect ok");
        assert!(!info.mounted);
        assert_eq!(info.mount_point, None);
        assert!(!info.writable);
    }

    #[test]
    fn test_detect_support_proc_mounts_missing() {
        let fs = MockFs::default();
        let rc = Resctrl::with_provider(fs, Config::default());
        let err = rc.detect_support().unwrap_err();
        match err {
            Error::Io { path, source } => {
                assert!(path.ends_with("/proc/mounts"));
                assert_eq!(source.raw_os_error(), Some(libc::ENOENT));
            }
            other => panic!("unexpected error: {:?}", other),
        }
    }

    #[test]
    fn test_detect_support_proc_mounts_permission_denied() {
        let fs = MockFs::default();
        fs.add_file(Path::new("/proc/mounts"), "");
        fs.set_no_perm_file(Path::new("/proc/mounts"));
        let rc = Resctrl::with_provider(fs, Config::default());
        let err = rc.detect_support().unwrap_err();
        match err {
            Error::Io { path, source } => {
                assert!(path.ends_with("/proc/mounts"));
                assert_eq!(source.raw_os_error(), Some(libc::EACCES));
            }
            other => panic!("unexpected error: {:?}", other),
        }
    }

    #[test]
    fn test_detect_support_mounted_and_writable() {
        let fs = MockFs::default();
        // Simulate mounted under default root
        fs.add_file(
            Path::new("/proc/mounts"),
            "resctrl /sys/fs/resctrl resctrl rw 0 0\n",
        );
        fs.add_dir(Path::new("/sys"));
        fs.add_dir(Path::new("/sys/fs"));
        fs.add_dir(Path::new("/sys/fs/resctrl"));
        fs.add_file(Path::new("/sys/fs/resctrl/tasks"), "");
        let rc = Resctrl::with_provider(fs, Config::default());
        let info = rc.detect_support().expect("detect ok");
        assert!(info.mounted);
        assert_eq!(info.mount_point, Some(PathBuf::from("/sys/fs/resctrl")));
        assert!(info.writable);
    }

    #[test]
    fn test_detect_support_mounted_but_no_permission() {
        let fs = MockFs::default();
        fs.add_file(
            Path::new("/proc/mounts"),
            "resctrl /sys/fs/resctrl resctrl rw 0 0\n",
        );
        fs.add_dir(Path::new("/sys/fs/resctrl"));
        let tasks = Path::new("/sys/fs/resctrl/tasks");
        fs.add_file(tasks, "");
        fs.set_no_perm_file(tasks);
        let rc = Resctrl::with_provider(fs, Config::default());
        let info = rc.detect_support().expect("detect ok");
        assert!(info.mounted);
        assert!(!info.writable);
    }

    #[test]
    fn test_ensure_mounted_respects_auto_mount_flag() {
        let fs = MockFs::default();
        fs.add_file(Path::new("/proc/mounts"), "");
        let rc = Resctrl::with_provider(
            fs,
            Config {
                root: PathBuf::from("/sys/fs/resctrl"),
                group_prefix: "pod_".into(),
                auto_mount: false,
            },
        );
        let err = rc.ensure_mounted().unwrap_err();
        match err {
            Error::NotMounted { .. } => {}
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn test_ensure_mounted_performs_mount() {
        let fs = MockFs::default();
        fs.add_file(Path::new("/proc/mounts"), "");
        // also ensure /sys and /sys/fs exist in mock
        fs.add_dir(Path::new("/sys"));
        fs.add_dir(Path::new("/sys/fs"));
        let rc = Resctrl::with_provider(
            fs.clone(),
            Config {
                root: PathBuf::from("/sys/fs/resctrl"),
                group_prefix: "pod_".into(),
                auto_mount: true,
            },
        );
        rc.ensure_mounted().expect("mounted");
        // After mount, detect reports mounted
        let info = rc.detect_support().expect("detect ok");
        assert!(info.mounted);
        assert_eq!(
            info.mount_point.unwrap().to_string_lossy(),
            "/sys/fs/resctrl"
        );
    }

    #[test]
    fn test_ensure_mounted_permission_failure() {
        let fs = MockFs::default();
        fs.add_file(Path::new("/proc/mounts"), "");
        // cause mount to fail with EPERM
        fs.set_mount_err(libc::EPERM);
        let rc = Resctrl::with_provider(
            fs,
            Config {
                root: PathBuf::from("/sys/fs/resctrl"),
                group_prefix: "pod_".into(),
                auto_mount: true,
            },
        );
        let err = rc.ensure_mounted().unwrap_err();
        match err {
            Error::NoPermission { .. } => {}
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn test_ensure_mounted_unsupported_kernel() {
        let fs = MockFs::default();
        fs.add_file(Path::new("/proc/mounts"), "");
        // cause mount to fail with ENODEV
        fs.set_mount_err(libc::ENODEV);
        let rc = Resctrl::with_provider(
            fs,
            Config {
                root: PathBuf::from("/sys/fs/resctrl"),
                group_prefix: "pod_".into(),
                auto_mount: true,
            },
        );
        let err = rc.ensure_mounted().unwrap_err();
        match err {
            Error::Unsupported { .. } => {}
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn test_create_group_success() {
        let fs = MockFs::default();
        let root = PathBuf::from("/sys/fs/resctrl");
        fs.add_dir(&root);
        let cfg = Config {
            root: root.clone(),
            group_prefix: "pod_".into(),
            auto_mount: false,
        };
        let rc = Resctrl::with_provider(fs.clone(), cfg);
        let group = rc.create_group("my-pod:UID").expect("create ok");
        assert!(group.contains("/sys/fs/resctrl/pod_my-podUID"));
        // also verify the fs contains the directory
        let p = PathBuf::from(&group);
        assert!(fs.path_exists(&p));
    }

    #[test]
    fn test_create_group_not_mounted() {
        let fs = MockFs::default();
        let rc = Resctrl::with_provider(fs, Config::default());
        let err = rc.create_group("uid").unwrap_err();
        match err {
            Error::NotMounted { .. } => {}
            other => panic!("unexpected error: {:?}", other),
        }
    }

    #[test]
    fn test_create_group_enospc_maps_capacity() {
        let fs = MockFs::default();
        let root = PathBuf::from("/sys/fs/resctrl");
        fs.add_dir(&root);
        let cfg = Config {
            root: root.clone(),
            group_prefix: "pod_".into(),
            auto_mount: false,
        };
        let group_path = root.join("pod_abc");
        fs.set_nospace_dir(&group_path);

        let rc = Resctrl::with_provider(fs, cfg);
        let err = rc.create_group("abc").unwrap_err();
        matches_capacity(err);
    }

    #[test]
    fn test_delete_group_success() {
        let fs = MockFs::default();
        let root = PathBuf::from("/sys/fs/resctrl");
        fs.add_dir(&root);
        let group_path = root.join("pod_abc");
        fs.add_dir(&group_path);

        let rc = Resctrl::with_provider(
            fs.clone(),
            Config {
                root,
                group_prefix: "pod_".into(),
                auto_mount: false,
            },
        );
        rc.delete_group(group_path.to_str().unwrap())
            .expect("delete ok");
        // also verify directory removed in fs
        assert!(!fs.path_exists(&group_path));
    }

    #[test]
    fn test_assign_tasks_success_and_missing() {
        let fs = MockFs::default();
        let root = PathBuf::from("/sys/fs/resctrl");
        fs.add_dir(&root);
        let group_path = root.join("pod_abc");
        fs.add_dir(&group_path);
        let tasks = group_path.join("tasks");
        fs.add_file(&tasks, "");
        fs.set_missing_pid(42);

        let rc = Resctrl::with_provider(
            fs,
            Config {
                root,
                group_prefix: "pod_".into(),
                auto_mount: false,
            },
        );
        let res = rc
            .assign_tasks(group_path.to_str().unwrap(), &[1, 42, 2])
            .expect("assign ok");
        assert_eq!(res.assigned, 2);
        assert_eq!(res.missing, 1);
    }

    #[test]
    fn test_assign_tasks_no_permission() {
        let fs = MockFs::default();
        let root = PathBuf::from("/sys/fs/resctrl");
        fs.add_dir(&root);
        let group_path = root.join("pod_abc");
        fs.add_dir(&group_path);
        let tasks = group_path.join("tasks");
        fs.add_file(&tasks, "");
        fs.set_no_perm_file(&tasks);

        let rc = Resctrl::with_provider(
            fs,
            Config {
                root,
                group_prefix: "pod_".into(),
                auto_mount: false,
            },
        );
        let err = rc
            .assign_tasks(group_path.to_str().unwrap(), &[1])
            .unwrap_err();
        match err {
            Error::NoPermission { .. } => {}
            other => panic!("unexpected error: {:?}", other),
        }
    }

    #[test]
    fn test_assign_tasks_enoent_group_missing() {
        let fs = MockFs::default();
        let root = PathBuf::from("/sys/fs/resctrl");
        fs.add_dir(&root);
        let group_path = root.join("pod_abc");
        fs.add_dir(&group_path);
        // Do NOT create tasks file
        let rc = Resctrl::with_provider(
            fs,
            Config {
                root,
                group_prefix: "pod_".into(),
                auto_mount: false,
            },
        );
        let err = rc
            .assign_tasks(group_path.to_str().unwrap(), &[1])
            .unwrap_err();
        match err {
            Error::Io { path, source } => {
                assert!(path.ends_with("tasks"));
                assert_eq!(source.raw_os_error(), Some(libc::ENOENT));
            }
            other => panic!("unexpected error: {:?}", other),
        }
    }

    #[test]
    fn test_list_group_tasks_success() {
        let fs = MockFs::default();
        let root = PathBuf::from("/sys/fs/resctrl");
        fs.add_dir(&root);
        let group_path = root.join("pod_abc");
        fs.add_dir(&group_path);
        let tasks = group_path.join("tasks");
        fs.add_file(&tasks, "1\n2\n3\n");

        let rc = Resctrl::with_provider(
            fs,
            Config {
                root,
                group_prefix: "pod_".into(),
                auto_mount: false,
            },
        );
        let pids = rc
            .list_group_tasks(group_path.to_str().unwrap())
            .expect("list ok");
        assert_eq!(pids, vec![1, 2, 3]);
    }

    #[test]
    fn test_list_group_tasks_invalid_content() {
        let fs = MockFs::default();
        let root = PathBuf::from("/sys/fs/resctrl");
        fs.add_dir(&root);
        let group_path = root.join("pod_abc");
        fs.add_dir(&group_path);
        let tasks = group_path.join("tasks");
        fs.add_file(&tasks, "1\n2\na bc\n3\n");

        let rc = Resctrl::with_provider(
            fs,
            Config {
                root,
                group_prefix: "pod_".into(),
                auto_mount: false,
            },
        );
        let err = rc
            .list_group_tasks(group_path.to_str().unwrap())
            .unwrap_err();
        match err {
            Error::Io { path, source } => {
                assert!(path.ends_with("tasks"));
                assert_eq!(source.kind(), io::ErrorKind::InvalidData);
            }
            other => panic!("unexpected error: {:?}", other),
        }
    }

    #[test]
    fn test_list_group_tasks_no_permission() {
        let fs = MockFs::default();
        let root = PathBuf::from("/sys/fs/resctrl");
        fs.add_dir(&root);
        let group_path = root.join("pod_abc");
        fs.add_dir(&group_path);
        let tasks = group_path.join("tasks");
        fs.add_file(&tasks, "");
        fs.set_no_perm_file(&tasks);

        let rc = Resctrl::with_provider(
            fs,
            Config {
                root,
                group_prefix: "pod_".into(),
                auto_mount: false,
            },
        );
        let err = rc
            .list_group_tasks(group_path.to_str().unwrap())
            .unwrap_err();
        match err {
            Error::NoPermission { .. } => {}
            other => panic!("unexpected error: {:?}", other),
        }
    }

    fn matches_capacity(err: Error) {
        match err {
            Error::Capacity { .. } => {}
            other => panic!("expected capacity, got {:?}", other),
        }
    }

    #[test]
    fn test_reconcile_group_converges() {
        let fs = MockFs::default();
        // Simulate mounted under default root
        fs.add_file(
            Path::new("/proc/mounts"),
            "resctrl /sys/fs/resctrl resctrl rw 0 0\n",
        );
        let root = PathBuf::from("/sys/fs/resctrl");
        fs.add_dir(&root);

        // Create group and its tasks file
        let group_path = root.join("pod_abc");
        fs.add_dir(&group_path);
        let tasks = group_path.join("tasks");
        fs.add_file(&tasks, "");

        let rc = Resctrl::with_provider(
            fs,
            Config {
                root: root.clone(),
                group_prefix: "pod_".into(),
                auto_mount: false,
            },
        );

        // Desired PIDs remain stable; should converge in <= 2 passes
        let desired = vec![101, 202];
        use std::cell::RefCell;
        let calls = RefCell::new(0usize);
        let pid_source = || -> Result<Vec<i32>> {
            *calls.borrow_mut() += 1;
            Ok(desired.clone())
        };
        let res = rc
            .reconcile_group(group_path.to_str().unwrap(), pid_source, 10)
            .expect("reconcile ok");

        assert_eq!(res.missing, 0);
        assert_eq!(res.assigned, desired.len());
        // Should converge in 2 passes (first to assign, second to verify)
        assert!(
            *calls.borrow() <= 2,
            "Expected <= 2 iterations, got {}",
            *calls.borrow()
        );

        // Verify tasks file contains the assigned PIDs
        let listed = rc
            .list_group_tasks(group_path.to_str().unwrap())
            .expect("list ok");
        assert_eq!(listed.len(), desired.len());
        for p in desired {
            assert!(listed.contains(&p));
        }
    }

    #[test]
    fn test_reconcile_group_partial_when_pids_missing() {
        let fs = MockFs::default();
        // Simulate mounted under default root
        fs.add_file(
            Path::new("/proc/mounts"),
            "resctrl /sys/fs/resctrl resctrl rw 0 0\n",
        );
        let root = PathBuf::from("/sys/fs/resctrl");
        fs.add_dir(&root);

        // Create group and its tasks file
        let group_path = root.join("pod_def");
        fs.add_dir(&group_path);
        let tasks = group_path.join("tasks");
        fs.add_file(&tasks, "");

        // Mark a PID as always missing (ESRCH) when writing
        fs.set_missing_pid(303);

        let rc = Resctrl::with_provider(
            fs,
            Config {
                root: root.clone(),
                group_prefix: "pod_".into(),
                auto_mount: false,
            },
        );

        use std::cell::RefCell;
        let calls = RefCell::new(0usize);
        let pid_source = || -> Result<Vec<i32>> {
            *calls.borrow_mut() += 1;
            Ok(vec![303])
        };

        let max_passes = 3;
        let res = rc
            .reconcile_group(group_path.to_str().unwrap(), pid_source, max_passes)
            .expect("reconcile ok");

        assert_eq!(res.assigned, 0);
        assert_eq!(res.missing, 1);
        assert_eq!(*calls.borrow(), max_passes);
    }

    #[test]
    fn test_reconcile_group_converges_after_changes() {
        let fs = MockFs::default();
        fs.add_file(
            Path::new("/proc/mounts"),
            "resctrl /sys/fs/resctrl resctrl rw 0 0\n",
        );
        let root = PathBuf::from("/sys/fs/resctrl");
        fs.add_dir(&root);

        let group_path = root.join("pod_dyn");
        fs.add_dir(&group_path);
        let tasks = group_path.join("tasks");
        fs.add_file(&tasks, "");

        let rc = Resctrl::with_provider(
            fs,
            Config {
                root: root.clone(),
                group_prefix: "pod_".into(),
                auto_mount: false,
            },
        );

        let mut pass = 0usize;
        let pid_source = move || -> Result<Vec<i32>> {
            let out = match pass {
                0 => vec![1],
                1 => vec![1, 2],
                _ => vec![1, 2],
            };
            pass += 1;
            Ok(out)
        };
        let res = rc
            .reconcile_group(group_path.to_str().unwrap(), pid_source, 10)
            .expect("reconcile ok");
        assert_eq!(res.missing, 0);
        assert_eq!(res.assigned, 2); // 1 then 2
                                     // should have required at least 3 passes (implicitly via closure sequence)
    }

    #[test]
    fn test_reconcile_group_noop_when_desired_already_present() {
        let fs = MockFs::default();
        fs.add_file(
            Path::new("/proc/mounts"),
            "resctrl /sys/fs/resctrl resctrl rw 0 0\n",
        );
        let root = PathBuf::from("/sys/fs/resctrl");
        fs.add_dir(&root);

        let group_path = root.join("pod_preloaded");
        fs.add_dir(&group_path);
        let tasks = group_path.join("tasks");
        fs.add_file(&tasks, "10\n11\n");

        let rc = Resctrl::with_provider(
            fs,
            Config {
                root: root.clone(),
                group_prefix: "pod_".into(),
                auto_mount: false,
            },
        );

        use std::cell::RefCell;
        let calls = RefCell::new(0usize);
        let pid_source = || -> Result<Vec<i32>> {
            *calls.borrow_mut() += 1;
            Ok(vec![10, 11])
        };
        let res = rc
            .reconcile_group(group_path.to_str().unwrap(), pid_source, 5)
            .expect("reconcile ok");
        assert_eq!(res.missing, 0);
        assert_eq!(res.assigned, 0);
        // Should only need 1 pass since PIDs are already there
        assert_eq!(
            *calls.borrow(),
            1,
            "Expected 1 iteration for already-present PIDs"
        );
    }

    #[test]
    fn test_reconcile_group_no_convergence_with_continuous_changes() {
        let fs = MockFs::default();
        fs.add_file(
            Path::new("/proc/mounts"),
            "resctrl /sys/fs/resctrl resctrl rw 0 0\n",
        );
        let root = PathBuf::from("/sys/fs/resctrl");
        fs.add_dir(&root);

        let group_path = root.join("pod_chaos");
        fs.add_dir(&group_path);
        let tasks = group_path.join("tasks");
        fs.add_file(&tasks, "");

        // Mark all PIDs from earlier iterations as missing to simulate process churn
        for i in 0..10 {
            fs.set_missing_pid(100 + i);
            fs.set_missing_pid(200 + i);
        }

        let rc = Resctrl::with_provider(
            fs,
            Config {
                root: root.clone(),
                group_prefix: "pod_".into(),
                auto_mount: false,
            },
        );

        use std::cell::RefCell;
        let calls = RefCell::new(0usize);
        let pid_source = || -> Result<Vec<i32>> {
            let iteration = *calls.borrow();
            *calls.borrow_mut() += 1;
            // PIDs keep changing every iteration - simulate high churn
            Ok(vec![100 + iteration as i32, 200 + iteration as i32])
        };

        let max_passes = 5;
        let res = rc
            .reconcile_group(group_path.to_str().unwrap(), pid_source, max_passes)
            .expect("reconcile ok");

        // Should not converge as all PIDs fail with ESRCH (missing)
        assert_eq!(
            res.assigned, 0,
            "Should not have assigned any PIDs due to ESRCH"
        );
        assert_eq!(
            res.missing, 2,
            "Should have 2 missing PIDs from last iteration"
        );
        assert_eq!(
            *calls.borrow(),
            max_passes,
            "Should have tried all {} passes",
            max_passes
        );
    }

    #[test]
    fn test_reconcile_group_handles_forking_processes() {
        let fs = MockFs::default();
        fs.add_file(
            Path::new("/proc/mounts"),
            "resctrl /sys/fs/resctrl resctrl rw 0 0\n",
        );
        let root = PathBuf::from("/sys/fs/resctrl");
        fs.add_dir(&root);

        let group_path = root.join("pod_fork");
        fs.add_dir(&group_path);
        let tasks = group_path.join("tasks");
        // Simulate that PID 100 is already in resctrl group (parent already assigned)
        fs.add_file(&tasks, "100\n");

        // Keep a handle to the mock FS to mutate the tasks file from the PID source
        let fs_mut = fs.clone();

        let rc = Resctrl::with_provider(
            fs,
            Config {
                root: root.clone(),
                group_prefix: "pod_".into(),
                auto_mount: false,
            },
        );

        use std::cell::RefCell;
        let calls = RefCell::new(0usize);
        let tasks_path = tasks.clone();
        // On every call, add a new PID to the resctrl tasks file and also include
        // it in the returned desired set (simulating children forking into the cgroup
        // and already present in resctrl via inheritance).
        let pid_source = || -> Result<Vec<i32>> {
            *calls.borrow_mut() += 1;
            let dyn_pid = 200 + *calls.borrow() as i32;
            // Append the dynamic PID to the tasks file before we list current tasks
            fs_mut
                .write_str(&tasks_path, &format!("{}", dyn_pid))
                .expect("write tasks");
            Ok(vec![100, 101, dyn_pid])
        };

        let res = rc
            .reconcile_group(group_path.to_str().unwrap(), pid_source, 10)
            .expect("reconcile ok");

        // We expect immediate convergence because the dynamically added PID is already
        // present in the resctrl tasks file when desired is evaluated.
        assert_eq!(res.missing, 0);
        assert_eq!(res.assigned, 1);
        assert!(
            *calls.borrow() <= 2,
            "Should converge quickly for fork scenario"
        );

        // Verify that both the original and dynamically added PID are in the group
        let listed = rc
            .list_group_tasks(group_path.to_str().unwrap())
            .expect("list ok");
        assert!(listed.contains(&100));
        // At least the first dynamic PID should be present
        assert!(listed.iter().any(|p| *p >= 201));
    }
}
