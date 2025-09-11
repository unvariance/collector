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
        use cgroups_rs::{cgroup::Cgroup, hierarchies};
        use std::io;
        use std::path::Path;

        if cgroup_path.is_empty() {
            return Err(resctrl::Error::Io {
                path: std::path::PathBuf::from("<cgroup path>"),
                source: io::Error::new(io::ErrorKind::InvalidInput, "empty cgroup path"),
            });
        }

        // Explicitly error if the cgroup path does not exist
        if !Path::new(cgroup_path).exists() {
            return Err(resctrl::Error::Io {
                path: std::path::PathBuf::from(cgroup_path),
                source: io::Error::from_raw_os_error(libc::ENOENT),
            });
        }

        let hier = hierarchies::auto();
        let cg = Cgroup::load(hier, cgroup_path);

        let procs = cg.procs();
        Ok(procs.into_iter().map(|pid| pid.pid as i32).collect())
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
