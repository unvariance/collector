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
        use cgroups_rs::{cgroup::Cgroup, hierarchies};

        let cg_path = c
            .linux
            .as_ref()
            .map(|l| l.cgroups_path.clone())
            .unwrap_or_default();

        if cg_path.is_empty() {
            return Ok(vec![]);
        }

        let hier = hierarchies::auto();
        let cg = Cgroup::load(hier, &cg_path);

        let procs = cg.procs();
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
