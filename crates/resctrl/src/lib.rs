use std::fmt;
use std::io;
use std::path::{Path, PathBuf};

pub use error::{Error, Result};

mod error;
mod provider;
pub use provider::{FsProvider, RealFs};

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
    use std::collections::{BTreeMap, BTreeSet, HashSet};

    #[derive(Clone, Default)]
    struct MockFs {
        state: std::rc::Rc<std::cell::RefCell<MockState>>,
    }

    #[derive(Default)]
    struct MockState {
        dirs: BTreeSet<PathBuf>,
        files: BTreeMap<PathBuf, String>,
        no_perm_files: HashSet<PathBuf>,
        no_perm_dirs: HashSet<PathBuf>,
        nospace_dirs: HashSet<PathBuf>,
        missing_pids: HashSet<i32>,
        mount_err: Option<i32>,
    }

    // Tests are single-threaded; declare Send/Sync to satisfy the trait bound.
    unsafe impl Send for MockFs {}
    unsafe impl Sync for MockFs {}

    impl MockFs {
        fn add_dir(&mut self, p: &Path) {
            let mut st = self.state.borrow_mut();
            st.dirs.insert(p.to_path_buf());
        }
        fn add_file(&mut self, p: &Path, content: &str) {
            let mut st = self.state.borrow_mut();
            st.files.insert(p.to_path_buf(), content.to_string());
        }
        fn set_no_perm_file(&mut self, p: &Path) {
            let mut st = self.state.borrow_mut();
            st.no_perm_files.insert(p.to_path_buf());
        }
        fn set_nospace_dir(&mut self, p: &Path) {
            let mut st = self.state.borrow_mut();
            st.nospace_dirs.insert(p.to_path_buf());
        }
        fn set_missing_pid(&mut self, pid: i32) {
            let mut st = self.state.borrow_mut();
            st.missing_pids.insert(pid);
        }

        // helper for exists in tests
        fn path_exists(&self, p: &Path) -> bool {
            let st = self.state.borrow();
            st.dirs.contains(p) || st.files.contains_key(p)
        }
    }

    impl FsProvider for MockFs {
        fn exists(&self, p: &Path) -> bool {
            let st = self.state.borrow();
            st.dirs.contains(p) || st.files.contains_key(p)
        }
        fn create_dir(&self, p: &Path) -> io::Result<()> {
            let mut st = self.state.borrow_mut();
            if st.no_perm_dirs.contains(p) {
                return Err(io::Error::from_raw_os_error(libc::EACCES));
            }
            if st.nospace_dirs.contains(p) {
                return Err(io::Error::from_raw_os_error(libc::ENOSPC));
            }
            if st.dirs.contains(p) {
                return Err(io::Error::new(io::ErrorKind::AlreadyExists, "exists"));
            }
            st.dirs.insert(p.to_path_buf());
            Ok(())
        }
        fn remove_dir(&self, p: &Path) -> io::Result<()> {
            let mut st = self.state.borrow_mut();
            if st.no_perm_dirs.contains(p) {
                return Err(io::Error::from_raw_os_error(libc::EACCES));
            }
            if !st.dirs.contains(p) {
                return Err(io::Error::from_raw_os_error(libc::ENOENT));
            }
            st.dirs.remove(p);
            Ok(())
        }
        fn write_str(&self, p: &Path, data: &str) -> io::Result<()> {
            let mut st = self.state.borrow_mut();
            if st.no_perm_files.contains(p) {
                return Err(io::Error::from_raw_os_error(libc::EACCES));
            }
            if !st.files.contains_key(p) {
                return Err(io::Error::from_raw_os_error(libc::ENOENT));
            }
            // If writing to tasks, simulate ESRCH for missing pid
            if p.ends_with("tasks") {
                if let Ok(pid) = data.trim().parse::<i32>() {
                    if st.missing_pids.contains(&pid) {
                        return Err(io::Error::from_raw_os_error(libc::ESRCH));
                    }
                }
                let entry = st.files.entry(p.to_path_buf()).or_default();
                if !entry.ends_with('\n') && !entry.is_empty() {
                    entry.push('\n');
                }
                entry.push_str(data);
                entry.push('\n');
            }
            Ok(())
        }
        fn read_to_string(&self, p: &Path) -> io::Result<String> {
            let st = self.state.borrow();
            if st.no_perm_files.contains(p) {
                return Err(io::Error::from_raw_os_error(libc::EACCES));
            }
            match st.files.get(p) {
                Some(s) => Ok(s.clone()),
                None => Err(io::Error::from_raw_os_error(libc::ENOENT)),
            }
        }
        fn check_can_open_for_write(&self, p: &Path) -> io::Result<()> {
            let st = self.state.borrow();
            if st.no_perm_files.contains(p) {
                return Err(io::Error::from_raw_os_error(libc::EACCES));
            }
            if st.files.contains_key(p) {
                Ok(())
            } else {
                Err(io::Error::from_raw_os_error(libc::ENOENT))
            }
        }
        fn mount_resctrl(&self, target: &Path) -> io::Result<()> {
            let mut st = self.state.borrow_mut();
            if let Some(code) = st.mount_err.take() {
                return Err(io::Error::from_raw_os_error(code));
            }
            // Simulate mount by ensuring target dir exists and appending to /proc/mounts
            st.dirs.insert(target.to_path_buf());
            let line = format!("resctrl {} resctrl rw,relatime 0 0\n", target.display());
            let pm = PathBuf::from("/proc/mounts");
            let entry = st.files.entry(pm).or_default();
            entry.push_str(&line);

            // Also simulate presence of tasks file under mountpoint
            let tasks = target.join("tasks");
            st.files.entry(tasks).or_default();
            Ok(())
        }
    }

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
        let mut fs = MockFs::default();
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
        let mut fs = MockFs::default();
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
        let mut fs = MockFs::default();
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
        let mut fs = MockFs::default();
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
        let mut fs = MockFs::default();
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
        let mut fs = MockFs::default();
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
        let mut fs = MockFs::default();
        fs.add_file(Path::new("/proc/mounts"), "");
        // cause mount to fail with EPERM
        {
            let mut st = fs.state.borrow_mut();
            st.mount_err = Some(libc::EPERM);
        }
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
        let mut fs = MockFs::default();
        fs.add_file(Path::new("/proc/mounts"), "");
        // cause mount to fail with ENODEV
        {
            let mut st = fs.state.borrow_mut();
            st.mount_err = Some(libc::ENODEV);
        }
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
        let mut fs = MockFs::default();
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
        let mut fs = MockFs::default();
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
        let mut fs = MockFs::default();
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
        let mut fs = MockFs::default();
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
        let mut fs = MockFs::default();
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
        let mut fs = MockFs::default();
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
        let mut fs = MockFs::default();
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
        let mut fs = MockFs::default();
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
        let mut fs = MockFs::default();
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
}
