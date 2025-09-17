resctrl crate

Summary
- Safe, testable wrapper over Linux resctrl filesystem for:
  - create_group(pod_uid)
  - delete_group(group_path)
  - assign_tasks(group_path, pids) -> AssignmentResult
  - list_group_tasks(group_path)
  - detect_support() -> SupportInfo
  - ensure_mounted(auto_mount)
  - cleanup_all() -> CleanupReport

API Overview
- Construct with defaults:
  - root: /sys/fs/resctrl
  - group_prefix: "pod_"
 - create_group() creates measurement groups under `<root>/mon_groups`

Example
```rust
use resctrl::{Resctrl, AssignmentResult};

let res = Resctrl::default();
let group = res.create_group("abc-123").expect("create group");
let result: AssignmentResult = res.assign_tasks(&group, &[1234, 5678])?;
let tasks = res.list_group_tasks(&group)?;
res.delete_group(&group)?;
```

Detection and auto-mount
- `detect_support()` returns `SupportInfo { mounted, mount_point, writable }`.
- `ensure_mounted(auto_mount)` verifies resctrl is mounted; if not and `auto_mount=false`, returns `Error::NotMounted`.
- When `auto_mount=true`, attempts `mount -t resctrl resctrl <root>` (via syscall). Failures map to:
  - `NoPermission` (e.g., missing CAP_SYS_ADMIN)
  - `Unsupported` (e.g., kernel lacks resctrl)
  - `Io` with path context for other errors

Startup cleanup
- `cleanup_all()` removes only groups created by this component (prefix match) at two locations:
  - immediate child directories under the resctrl root
  - immediate child directories under `<root>/mon_groups`
- It ignores non-matching directories (e.g., `info`) and all files.
- It assumes resctrl is already mounted and does not call `ensure_mounted()`.
- Returns `CleanupReport { removed, removal_failures, removal_race, non_prefix_groups }`.

Errors
- NotMounted: resctrl root is missing when creating groups
- NoPermission: permission denied for mkdir/read/write/remove
- Capacity: ENOSPC from kernel (e.g., RMID exhaustion)
- Io: other io errors with path context

Notes
- Pod UID is sanitized to [a-zA-Z0-9_-] and truncated (<64) before prefixing.
- Filesystem access goes through a trait to enable mocking in tests.
- Auto-mount is opt-in due to privileges and risk. Prefer mounting from the host or orchestrator; when enabled, failures return errors and do not leave a resctrl mount if the mount attempt fails.
- Mounting responsibility: callers should invoke `ensure_mounted(auto_mount)` once during startup before performing resctrl operations such as cleanup, group creation, or assignment.

Hardware smoke test
- Integration test `tests/smoke_test.rs` is gated by `RESCTRL_E2E=1` and will:
  - mount resctrl if needed
  - create a group, assign current PID, verify, detach, delete
- Run with: `RESCTRL_E2E=1 cargo test -p resctrl -- --nocapture`
