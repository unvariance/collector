/// Source of PIDs for a container based on cgroup metadata.
pub trait CgroupPidSource: Send + Sync {
    fn pids_for_container(&self, c: &nri::api::Container) -> resctrl::Result<Vec<i32>>;
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
    fn pids_for_container(&self, c: &nri::api::Container) -> resctrl::Result<Vec<i32>> {
        use cgroups_rs::cgroup::Cgroup;

        let cg_path = c
            .linux
            .as_ref()
            .map(|l| l.cgroups_path.clone())
            .unwrap_or_default();

        if cg_path.is_empty() {
            return Ok(vec![]);
        }

        // Use cgroups-rs to get PIDs
        let cg = Cgroup::load_from_relative_path(&cg_path).map_err(|e| resctrl::Error::Io {
            path: std::path::PathBuf::from(cg_path.clone()),
            source: std::io::Error::other(format!("cgroup load failed: {e}")),
        })?;

        let procs = cg.procs().map_err(|e| resctrl::Error::Io {
            path: std::path::PathBuf::from(cg_path.clone()).join("cgroup.procs"),
            source: std::io::Error::other(format!("read procs failed: {e}")),
        })?;

        Ok(procs.into_iter().map(|pid| pid.pid as i32).collect())
    }
}

#[cfg(not(target_os = "linux"))]
impl CgroupPidSource for RealCgroupPidSource {
    fn pids_for_container(&self, _c: &nri::api::Container) -> resctrl::Result<Vec<i32>> {
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

        pub fn set_pids(&mut self, container_id: String, pids: Vec<i32>) {
            self.pids_map.insert(container_id, pids);
        }
    }

    impl CgroupPidSource for MockCgroupPidSource {
        fn pids_for_container(&self, c: &nri::api::Container) -> resctrl::Result<Vec<i32>> {
            Ok(self.pids_map.get(&c.id).cloned().unwrap_or_default())
        }
    }
}
