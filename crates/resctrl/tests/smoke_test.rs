use resctrl::{AssignmentResult, Resctrl};
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
    Err(std::io::Error::new(
        std::io::ErrorKind::Other,
        format!("mount resctrl failed: status {:?}, err: {}", status.code(), stderr),
    ))
}

#[test]
fn resctrl_smoke() -> anyhow::Result<()> {
    // Only run on explicit opt-in (hardware E2E). Otherwise skip to keep CI/dev fast.
    if std::env::var("RESCTRL_E2E").ok().as_deref() != Some("1") {
        eprintln!("RESCTRL_E2E not set; skipping hardware smoke test");
        return Ok(());
    }

    let rc = Resctrl::default();
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
            assigned, missing
        ));
    }

    let tasks = rc.list_group_tasks(&group)?;
    if !tasks.iter().any(|p| *p == pid) {
        return Err(anyhow::anyhow!(
            "pid {} not found in group task list: {:?}",
            pid, tasks
        ));
    }

    // Detach and cleanup
    let AssignmentResult { assigned, .. } = rc.assign_tasks("/sys/fs/resctrl", &[pid])?;
    if assigned != 1 {
        eprintln!("warning: could not detach pid {} back to root (assigned={})", pid, assigned);
    }
    rc.delete_group(&group)?;
    Ok(())
}

