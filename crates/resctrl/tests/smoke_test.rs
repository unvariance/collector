use resctrl::{AssignmentResult, Config, Error, Resctrl};
use std::process::Command;

fn try_mount_resctrl() -> std::io::Result<()> {
    // Attempt to mount the resctrl filesystem if not mounted
    let status = Command::new("mount")
        .args(["-t", "resctrl", "resctrl", "/sys/fs/resctrl"])
        .status()?;
    if status.success() {
        return Ok(());
    }
    let out = Command::new("mount")
        .arg("-v")
        .args(["-t", "resctrl", "resctrl", "/sys/fs/resctrl"])
        .output()?;
    let stderr = String::from_utf8_lossy(&out.stderr);
    if stderr.to_lowercase().contains("busy") || stderr.to_lowercase().contains("mounted") {
        return Ok(());
    }
    Err(std::io::Error::other(format!(
        "mount resctrl failed: status {:?}, err: {}",
        status.code(),
        stderr
    )))
}

fn try_umount_resctrl() -> std::io::Result<()> {
    // Attempt to unmount resctrl; treat "not mounted" as success
    let status = Command::new("umount").arg("/sys/fs/resctrl").status()?;
    if status.success() {
        return Ok(());
    }
    // Check /proc/mounts; if not present, consider unmounted
    let mounted = std::fs::read_to_string("/proc/mounts")
        .unwrap_or_default()
        .lines()
        .any(|l| l.split_whitespace().nth(2) == Some("resctrl"));
    if !mounted {
        return Ok(());
    }
    Err(std::io::Error::other(
        "umount /sys/fs/resctrl failed and resctrl still mounted",
    ))
}

#[test]
fn resctrl_smoke() -> anyhow::Result<()> {
    // Only run on explicit opt-in (hardware E2E). Otherwise skip to keep CI/dev fast.
    if std::env::var("RESCTRL_E2E").ok().as_deref() != Some("1") {
        eprintln!("RESCTRL_E2E not set; skipping hardware smoke test");
        return Ok(());
    }

    // First, validate detection and ensure_mounted behavior (mounted/unmounted paths)
    let rc = Resctrl::default();
    // Force unmounted state; fail if we cannot unmount.
    try_umount_resctrl()?;
    let info_unmounted = rc.detect_support()?;
    if info_unmounted.mounted {
        return Err(anyhow::anyhow!(
            "expected resctrl unmounted after umount, but detect reported mounted"
        ));
    }
    // ensure_mounted should fail with auto_mount=false
    let rc_no_auto = Resctrl::new(Config::default());
    match rc_no_auto.ensure_mounted(false) {
        Err(Error::NotMounted { .. }) => {}
        other => return Err(anyhow::anyhow!("expected NotMounted, got: {other:?}")),
    }
    // Now with auto_mount=true it should mount successfully
    let rc_auto = Resctrl::new(Config::default());
    rc_auto.ensure_mounted(true)?;
    let info_after = rc_auto.detect_support()?;
    if !info_after.mounted {
        return Err(anyhow::anyhow!(
            "ensure_mounted succeeded but detect shows not mounted"
        ));
    }
    if !info_after.writable {
        return Err(anyhow::anyhow!(
            "resctrl mounted but root tasks file not writable by test process"
        ));
    }
    // Verify calling ensure_mounted again when already mounted is a no-op and succeeds
    rc_auto.ensure_mounted(true)?;
    let uid = format!("smoke_{}", uuid::Uuid::new_v4());

    let group = match rc.create_group(&uid) {
        Ok(p) => p,
        Err(resctrl::Error::NotMounted { .. }) => {
            try_mount_resctrl()?;
            let rc2 = Resctrl::default();
            rc2.create_group(&uid)?
        }
        Err(e) => return Err(anyhow::anyhow!("create_group failed: {e}")),
    };

    let pid = std::process::id() as i32;
    let AssignmentResult { assigned, missing } = rc.assign_tasks(&group, &[pid])?;
    if assigned != 1 || missing != 0 {
        return Err(anyhow::anyhow!(
            "unexpected assignment result: assigned={}, missing={}",
            assigned,
            missing
        ));
    }

    let tasks = rc.list_group_tasks(&group)?;
    if !tasks.contains(&pid) {
        return Err(anyhow::anyhow!(
            "pid {} not found in group task list: {:?}",
            pid,
            tasks
        ));
    }

    // Detach and cleanup
    let AssignmentResult { assigned, .. } = rc.assign_tasks("/sys/fs/resctrl", &[pid])?;
    if assigned != 1 {
        eprintln!(
            "warning: could not detach pid {} back to root (assigned={})",
            pid, assigned
        );
    }
    rc.delete_group(&group)?;
    Ok(())
}
