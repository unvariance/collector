#[cfg(test)]
pub mod mock_fs {
    #![allow(dead_code)]
    use crate::FsProvider;
    use std::collections::{HashMap, HashSet};
    use std::io;
    use std::path::{Path, PathBuf};
    use std::sync::{Arc, Mutex};

    #[derive(Clone, Default)]
    pub struct MockFsState {
        pub files: HashMap<PathBuf, String>,
        pub dirs: HashSet<PathBuf>,
        pub no_perm_files: HashSet<PathBuf>,
        pub no_perm_dirs: HashSet<PathBuf>,
        pub nospace_dirs: HashSet<PathBuf>,
        pub missing_pids: HashSet<i32>,
        pub mount_err: Option<i32>,
        // Optional overrides for directory listing. If present, returned as-is.
        pub child_dir_overrides: HashMap<PathBuf, Vec<String>>,
        // Simulate permission denied on remove_dir for these paths
        pub no_perm_remove_dirs: HashSet<PathBuf>,
    }

    #[derive(Clone, Default)]
    pub struct MockFs {
        state: Arc<Mutex<MockFsState>>,
    }

    impl MockFs {
        pub fn new() -> Self {
            Self::default()
        }

        pub fn add_file(&self, p: &Path, content: &str) {
            let mut st = self.state.lock().unwrap();
            st.files.insert(p.to_path_buf(), content.to_string());
        }

        pub fn add_dir(&self, p: &Path) {
            let mut st = self.state.lock().unwrap();
            st.dirs.insert(p.to_path_buf());
        }

        pub fn set_missing_pid(&self, pid: i32) {
            let mut st = self.state.lock().unwrap();
            st.missing_pids.insert(pid);
        }

        pub fn set_no_perm_file(&self, p: &Path) {
            let mut st = self.state.lock().unwrap();
            st.no_perm_files.insert(p.to_path_buf());
        }

        pub fn set_no_perm_dir(&self, p: &Path) {
            let mut st = self.state.lock().unwrap();
            st.no_perm_dirs.insert(p.to_path_buf());
        }

        pub fn set_no_perm_remove_dir(&self, p: &Path) {
            let mut st = self.state.lock().unwrap();
            st.no_perm_remove_dirs.insert(p.to_path_buf());
        }

        pub fn set_nospace_dir(&self, p: &Path) {
            let mut st = self.state.lock().unwrap();
            st.nospace_dirs.insert(p.to_path_buf());
        }

        pub fn set_mount_err(&self, err: i32) {
            let mut st = self.state.lock().unwrap();
            st.mount_err = Some(err);
        }

        pub fn dir_exists(&self, p: &Path) -> bool {
            let st = self.state.lock().unwrap();
            st.dirs.contains(p)
        }

        pub fn path_exists(&self, p: &Path) -> bool {
            let st = self.state.lock().unwrap();
            st.dirs.contains(p) || st.files.contains_key(p)
        }

        pub fn file_contents(&self, p: &Path) -> Option<String> {
            let st = self.state.lock().unwrap();
            st.files.get(p).cloned()
        }

        /// Override child directory listing for a given parent path (used to simulate
        /// races where entries disappear between list and remove).
        pub fn set_child_dirs_override(&self, parent: &Path, names: Vec<String>) {
            let mut st = self.state.lock().unwrap();
            st.child_dir_overrides.insert(parent.to_path_buf(), names);
        }
    }

    impl FsProvider for MockFs {
        fn exists(&self, p: &Path) -> bool {
            let st = self.state.lock().unwrap();
            st.dirs.contains(p) || st.files.contains_key(p)
        }

        fn create_dir(&self, p: &Path) -> io::Result<()> {
            let mut st = self.state.lock().unwrap();
            if st.no_perm_dirs.contains(p) {
                return Err(io::Error::from_raw_os_error(libc::EACCES));
            }
            if st.nospace_dirs.contains(p) {
                return Err(io::Error::from_raw_os_error(libc::ENOSPC));
            }
            if st.dirs.contains(p) {
                return Err(io::Error::from_raw_os_error(libc::EEXIST));
            }
            st.dirs.insert(p.to_path_buf());
            Ok(())
        }

        fn remove_dir(&self, p: &Path) -> io::Result<()> {
            let mut st = self.state.lock().unwrap();
            if st.no_perm_remove_dirs.contains(p) {
                return Err(io::Error::from_raw_os_error(libc::EACCES));
            }
            if !st.dirs.remove(p) {
                return Err(io::Error::from_raw_os_error(libc::ENOENT));
            }
            // Also remove any files within this directory
            let prefix = p.to_path_buf();
            st.files.retain(|k, _| !k.starts_with(&prefix));
            Ok(())
        }

        fn write_str(&self, p: &Path, data: &str) -> io::Result<()> {
            let mut st = self.state.lock().unwrap();
            if st.no_perm_files.contains(p) {
                return Err(io::Error::from_raw_os_error(libc::EACCES));
            }
            // Check file exists for writing
            if !st.files.contains_key(p) {
                return Err(io::Error::from_raw_os_error(libc::ENOENT));
            }
            // Simulate ESRCH for missing PIDs when writing to tasks file
            if p.file_name() == Some(std::ffi::OsStr::new("tasks")) {
                for line in data.lines() {
                    if let Ok(pid) = line.trim().parse::<i32>() {
                        if st.missing_pids.contains(&pid) {
                            return Err(io::Error::from_raw_os_error(libc::ESRCH));
                        }
                    }
                }
            }
            let e = st.files.entry(p.to_path_buf()).or_default();
            if !e.ends_with('\n') && !e.is_empty() {
                e.push('\n');
            }
            e.push_str(data);
            if !data.ends_with('\n') {
                e.push('\n');
            }
            Ok(())
        }

        fn read_to_string(&self, p: &Path) -> io::Result<String> {
            let st = self.state.lock().unwrap();
            if st.no_perm_files.contains(p) {
                return Err(io::Error::from_raw_os_error(libc::EACCES));
            }
            match st.files.get(p) {
                Some(s) => Ok(s.clone()),
                None => Err(io::Error::from_raw_os_error(libc::ENOENT)),
            }
        }

        fn check_can_open_for_write(&self, p: &Path) -> io::Result<()> {
            let st = self.state.lock().unwrap();
            if st.no_perm_files.contains(p) {
                return Err(io::Error::from_raw_os_error(libc::EACCES));
            }
            if st.files.contains_key(p) || st.dirs.contains(p.parent().unwrap_or(Path::new("/"))) {
                Ok(())
            } else {
                Err(io::Error::from_raw_os_error(libc::ENOENT))
            }
        }

        fn read_child_dirs(&self, p: &Path) -> io::Result<Vec<String>> {
            let st = self.state.lock().unwrap();
            if let Some(v) = st.child_dir_overrides.get(p) {
                return Ok(v.clone());
            }
            if st.no_perm_dirs.contains(p) {
                return Err(io::Error::from_raw_os_error(libc::EACCES));
            }
            if !st.dirs.contains(p) {
                return Err(io::Error::from_raw_os_error(libc::ENOENT));
            }
            let mut out = Vec::new();
            for d in st.dirs.iter() {
                if d.parent() == Some(p) {
                    if let Some(name) = d.file_name() {
                        out.push(name.to_string_lossy().into_owned());
                    }
                }
            }
            Ok(out)
        }

        fn mount_resctrl(&self, target: &Path) -> io::Result<()> {
            let mut st = self.state.lock().unwrap();
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
}
