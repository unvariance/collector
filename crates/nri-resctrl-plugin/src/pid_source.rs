/// Source of PIDs for a container based on cgroup metadata.
pub trait CgroupPidSource: Send + Sync {
    fn pids_for_container(&self, c: &nri::api::Container) -> Vec<i32>;
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
    fn pids_for_container(&self, c: &nri::api::Container) -> Vec<i32> {
        let cg_path = c
            .linux
            .as_ref()
            .map(|l| l.cgroups_path.clone())
            .unwrap_or_default();
        
        if cg_path.is_empty() {
            return vec![];
        }

        // Read cgroup.procs or tasks file directly
        let procs_path = std::path::PathBuf::from(&cg_path).join("cgroup.procs");
        let tasks_path = std::path::PathBuf::from(&cg_path).join("tasks");
        
        let s = std::fs::read_to_string(&procs_path)
            .or_else(|_| std::fs::read_to_string(&tasks_path));
            
        match s {
            Ok(content) => content
                .lines()
                .filter_map(|l| l.trim().parse::<i32>().ok())
                .collect(),
            Err(_) => vec![],
        }
    }
}

#[cfg(not(target_os = "linux"))]
impl CgroupPidSource for RealCgroupPidSource {
    fn pids_for_container(&self, _c: &nri::api::Container) -> Vec<i32> {
        vec![]
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
        fn pids_for_container(&self, c: &nri::api::Container) -> Vec<i32> {
            self.pids_map.get(&c.id).cloned().unwrap_or_default()
        }
    }
}