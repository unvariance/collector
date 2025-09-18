mod test_bpf {
    include!("../src/bpf/cgroup_inode_test.skel.rs");
}

use anyhow::{Context, Result};
use libbpf_rs::skel::{OpenSkel, SkelBuilder};
use libbpf_rs::{MapCore as _, MapFlags, OpenObject, ProgramInput};
use std::fs::{self, File};
use std::io::Read;
use std::mem::MaybeUninit;
use std::os::unix::fs::MetadataExt;
use std::path::PathBuf;

// Import the cgroup inode test skeleton
use test_bpf::CgroupInodeTestSkelBuilder;

/// Reads the cgroup path for the current process from /proc/self/cgroup
fn read_cgroup_path() -> Result<String> {
    let mut file = File::open("/proc/self/cgroup").context("Failed to open /proc/self/cgroup")?;

    let mut contents = String::new();
    file.read_to_string(&mut contents)
        .context("Failed to read /proc/self/cgroup")?;

    // Look for cgroup v2 unified hierarchy
    for line in contents.lines() {
        let parts: Vec<&str> = line.split(':').collect();
        if parts.len() >= 3 && parts[0] == "0" {
            // For cgroup v2, the path is the last part after the colon
            return Ok(parts[2].to_string());
        }
    }

    // For cgroup v1, we would need to implement a different parsing strategy
    // For now, we just return an error
    Err(anyhow::anyhow!("Could not find cgroup v2 path"))
}

/// Gets the inode number for a cgroup path
fn get_cgroup_inode(cgroup_path: &str) -> Result<u64> {
    // Determine the cgroup mount point
    let mount_info = fs::read_to_string("/proc/self/mountinfo")
        .context("Failed to read /proc/self/mountinfo")?;

    let mut cgroup_mount_point = None;

    // Find the cgroup2 mount point
    for line in mount_info.lines() {
        let parts: Vec<&str> = line.split_whitespace().collect();
        if parts.len() >= 9 && parts[8].contains("cgroup2") {
            cgroup_mount_point = Some(parts[4].to_string());
            break;
        }
    }

    let cgroup_mount = cgroup_mount_point.context("Could not find cgroup2 mount point")?;

    // Construct the full path to the cgroup directory
    let full_path = PathBuf::from(cgroup_mount).join(&cgroup_path[1..]);

    // Get the metadata for the cgroup directory
    let metadata = fs::metadata(&full_path)
        .with_context(|| format!("Failed to get metadata for cgroup path: {:?}", full_path))?;

    // Return the inode number
    Ok(metadata.ino())
}

#[test]
#[ignore = "requires BPF + raised memlock; run with --ignored"]
fn test_cgroup_id_matches_inode() -> Result<()> {
    // Build and load the BPF program
    let skel_builder = CgroupInodeTestSkelBuilder::default();
    let obj_ref = Box::leak(Box::new(MaybeUninit::<OpenObject>::uninit()));

    let open_skel = skel_builder.open(obj_ref)?;
    let skel = open_skel.load()?;

    // Run the BPF program to get the cgroup ID
    let input = ProgramInput::default();
    let result = skel.progs.get_cgroup_id.test_run(input)?;

    // Check return value
    if result.return_value != 0 {
        return Err(anyhow::anyhow!(
            "BPF program returned error: {}",
            result.return_value
        ));
    }

    // Get the cgroup ID from the map
    let key = 0u32;
    let key_bytes = key.to_ne_bytes();
    let value = skel.maps.cgroup_id_map.lookup(&key_bytes, MapFlags::ANY)?;

    if value.is_none() {
        return Err(anyhow::anyhow!("Failed to lookup cgroup ID in map"));
    }

    // Interpret the bytes as a u64
    let cgroup_id_bytes = value.unwrap();
    let cgroup_id = u64::from_ne_bytes(cgroup_id_bytes.try_into().unwrap());

    println!("Cgroup ID from BPF: {}", cgroup_id);

    // Get the cgroup path for the current process
    let cgroup_path = read_cgroup_path().context("Failed to read cgroup path")?;

    println!("Cgroup path: {}", cgroup_path);

    // Get the inode number for the cgroup path
    let inode_number =
        get_cgroup_inode(&cgroup_path).context("Failed to get cgroup inode number")?;

    println!("Inode number from filesystem: {}", inode_number);

    // Compare the cgroup ID from BPF with the inode number
    assert_eq!(
        cgroup_id, inode_number,
        "Cgroup ID from BPF ({}) does not match inode number from filesystem ({})",
        cgroup_id, inode_number
    );

    Ok(())
}
