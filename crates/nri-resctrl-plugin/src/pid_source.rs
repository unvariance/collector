/// Source of PIDs for a container based on cgroup path.
pub trait CgroupPidSource: Send + Sync {
    fn pids_for_path(&self, cgroup_path: &str) -> resctrl::Result<Vec<i32>>;
}

pub struct RealCgroupPidSource;

impl RealCgroupPidSource {
    pub fn new() -> Self {
        Self
    }
}

impl Default for RealCgroupPidSource {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(target_os = "linux")]
impl CgroupPidSource for RealCgroupPidSource {
    fn pids_for_path(&self, cgroup_path: &str) -> resctrl::Result<Vec<i32>> {
        use std::io;
        use std::path::{Path, PathBuf};

        if cgroup_path.is_empty() {
            return Err(resctrl::Error::Io {
                path: PathBuf::from("<cgroup path>"),
                source: io::Error::new(io::ErrorKind::InvalidInput, "empty cgroup path"),
            });
        }

        let dir = Path::new(cgroup_path);
        if !dir.exists() {
            return Err(resctrl::Error::Io {
                path: PathBuf::from(cgroup_path),
                source: io::Error::from_raw_os_error(libc::ENOENT),
            });
        }

        // Read PIDs directly from the cgroup's procs file. This works on
        // cgroup v2 (cgroup.procs) and many v1 setups. Try common candidates.
        let candidates = ["cgroup.procs", "cgroups.procs"]; // second is rare, keep for compatibility
        let mut last_err: Option<io::Error> = None;
        for fname in candidates.iter() {
            let p = dir.join(fname);
            if p.exists() {
                match std::fs::read_to_string(&p) {
                    Ok(content) => {
                        let pids: Vec<i32> = content
                            .lines()
                            .filter_map(|l| l.trim().parse::<i32>().ok())
                            .collect();
                        return Ok(pids);
                    }
                    Err(e) => {
                        last_err = Some(e);
                        break; // file exists but unreadable → break and report
                    }
                }
            }
        }

        Err(resctrl::Error::Io {
            path: dir.join("cgroup.procs"),
            source: last_err.unwrap_or_else(|| io::Error::from_raw_os_error(libc::ENOENT)),
        })
    }
}

#[cfg(not(target_os = "linux"))]
impl CgroupPidSource for RealCgroupPidSource {
    fn pids_for_path(&self, _cgroup_path: &str) -> resctrl::Result<Vec<i32>> {
        Ok(vec![])
    }
}

#[cfg(test)]
pub mod test_support {
    use super::*;
    use std::collections::HashMap;

    #[derive(Clone, Default)]
    pub struct MockCgroupPidSource {
        pids_map: HashMap<String, Vec<i32>>,
    }

    impl MockCgroupPidSource {
        pub fn new() -> Self {
            Self::default()
        }

        #[allow(dead_code)]
        pub fn set_pids(&mut self, cgroup_path: String, pids: Vec<i32>) {
            self.pids_map.insert(cgroup_path, pids);
        }
    }

    impl CgroupPidSource for MockCgroupPidSource {
        fn pids_for_path(&self, cgroup_path: &str) -> resctrl::Result<Vec<i32>> {
            Ok(self.pids_map.get(cgroup_path).cloned().unwrap_or_default())
        }
    }
}
