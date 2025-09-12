# Sub-Issue 06: Startup cleanup of stale resctrl groups

Related epic: https://github.com/unvariance/collector/issues/252

## Summary
Add a safe, one-time startup cleanup that removes only our previously-created resctrl groups under `/sys/fs/resctrl` using the configured group prefix. This is controlled by `cleanup_on_start` (default true) and must align with the epic architecture: cleanup occurs once per plugin lifetime, does not emit pod events, and is implemented via the `resctrl` crate.

## Scope
- `crates/resctrl`
  - Add `Resctrl::cleanup_all(&self) -> Result<usize>` that scans the configured resctrl root and removes direct child directories whose names start with the configured `group_prefix`. Returns the number of groups removed.
  - Internals use the crate’s `Config { root, group_prefix, auto_mount }` and filesystem provider for testability.
  - Behavior when not mounted:
    - If `auto_mount` is true, attempt `ensure_mounted()` first; otherwise return `Error::NotMounted` without modifying the filesystem.
  - Error handling: best-effort removal. Failures to list root or sanitize inputs surface as errors; per-entry deletion errors are logged by the caller (plugin) or silently skipped by the library while continuing the sweep. The method returns the count of successfully removed groups.
- `crates/nri-resctrl-plugin`
  - If `cleanup_on_start` is true, invoke the cleanup exactly once before first reconciliation of pods/containers. Recommended integration point: the first `synchronize()` call, guarded by an internal `did_startup_cleanup` flag to avoid running more than once. Alternative is `configure()`, but `synchronize()` guarantees alignment with initial state processing.
  - Log the removed count (info/debug level). Do not emit `PodResctrlEvent` for cleanup-only actions.

## Out of Scope
- Detecting/cleaning groups not created by this plugin (strictly prefix-based).
- Periodic/background cleanup or any retry loops beyond the epic’s state model.
- Emitting events for startup cleanup or making cleanup observable via NRI events.
- Cross-process coordination of cleanup across multiple plugin instances.

## Architecture Alignment (Epic #252)
- Cleanup is implemented in the `resctrl` crate (library) and invoked by the NRI plugin; the plugin remains a thin orchestrator.
- Prefix-based naming and deletion only at the resctrl root level. No traversal into nested structures; kernel handles removing group internals.
- `auto_mount` handling lives in the `resctrl` crate. The plugin just calls `cleanup_all()`; if the FS isn’t mounted and `auto_mount` is false, `Error::NotMounted` is acceptable and should be logged and ignored.
- No events are emitted for cleanup-only operations. Plugin continues to emit Add/Update/Removed events only for pod lifecycle transitions.

## Deliverables / Acceptance
- `Resctrl::cleanup_all(&self) -> Result<usize>` implemented with tests using a mock `FsProvider`.
- `ResctrlPlugin` runs cleanup once at startup when `cleanup_on_start` is true and logs the removed count.
- Only removes directories beginning with `group_prefix` and only at the resctrl root. Ignores files and non-matching directories (e.g., `info`).
- No pod events are emitted as part of cleanup.
- Documentation updated to include naming convention and cleanup behavior/safety.

## Detailed Implementation Plan
- resctrl crate
  - Extend `FsProvider` with a read-dir capability usable in tests and prod, for example:
    - `fn read_dir_names(&self, p: &Path) -> io::Result<Vec<String>>` returning immediate child names (dirs and files), or
    - `fn read_dir(&self, p: &Path) -> io::Result<Vec<(PathBuf, bool)>>` where the bool indicates is_dir.
  - Implement `RealFs` using `std::fs::read_dir` and `file_type().is_dir()`; map common errno values to existing `Error` variants (NoPermission, Io, etc.).
  - Update mock/test FS to support directory listing consistent with the new method. Ensure it can represent both files and directories.
  - Implement `Resctrl::cleanup_all(&self)`:
    - If `auto_mount` is true, call `ensure_mounted()`.
    - If root does not exist and `auto_mount` is false, return `Error::NotMounted`.
    - List immediate children of the configured root; filter to entries that are directories AND whose name starts with `self.cfg.group_prefix`.
    - For each candidate, call `FsProvider::remove_dir` and increment a counter on success. On `ENOENT`, skip (idempotent). On other errors, let the library continue (best-effort) and bubble a summary only if the root listing failed at the start.
    - Return `Ok(removed_count)`.
  - Keep sanitization minimal: only strict prefix match; do not attempt to parse UIDs here. Rely on group creation to sanitize UIDs.

- nri-resctrl-plugin
  - Add an internal `did_startup_cleanup: AtomicBool` (or `OnceCell`) to `ResctrlPlugin`.
  - On first `synchronize()` when `cfg.cleanup_on_start` is true, call `resctrl.cleanup_all()` and set the flag. If it returns `Error::NotMounted`, log at debug and proceed; otherwise log removed count at info/debug. Never emit `PodResctrlEvent` for cleanup-only actions.
  - Unit-test this behavior using the plugin’s test FS and DI constructor (`with_resctrl` / `with_pid_source`).

## Detailed Test Plan

- resctrl crate unit tests (using mock `FsProvider`):
  - Root not mounted, `auto_mount=false`: `cleanup_all()` returns `Error::NotMounted` and removes nothing.
  - Root not mounted, `auto_mount=true`: mock mount succeeds; `cleanup_all()` proceeds and removes matching dirs.
  - Mixed entries: under `/sys/fs/resctrl`, create entries `pod_a`, `pod_b`, `info` (dir), `tasks` (file), `other` (dir). Only `pod_*` are removed; others remain. Assert returned count is 2.
  - Permission denied on one directory: `remove_dir` for `pod_b` returns `EACCES`; method still returns `Ok(1)` (best-effort) and other entries remain untouched.
  - Idempotency: calling `cleanup_all()` twice removes the same initial set once; second call returns 0 and is not an error.

- nri-resctrl-plugin tests:
  - Startup once: pre-populate resctrl root with `pod_stale1`, `pod_stale2`. Create plugin with `cleanup_on_start=true`. Call `synchronize()` with empty state. Assert both directories are removed and no events were received on the channel. Call `synchronize()` again; assert no additional deletions occur (flag prevents re-run).
  - Not mounted: when root missing and `auto_mount=false`, `cleanup_all()` returns `NotMounted`; plugin logs but continues. Verify no panic, no events, and directories obviously remain absent.
  - Coexistence with active pods: pre-populate one stale dir and then include a pod in `synchronize()`; verify cleanup ran first (stale removed) and pod handling proceeds normally (group created for the pod and Add/Update event emitted once).

## Risks and Mitigations
- Accidental deletion: mitigated by strict prefix filter, root-scope only, and tests covering mixed entries.
- Not mounted / insufficient permissions: handled via `auto_mount` in the library and clear errors; plugin logs and continues.
- Re-entrancy: guarded with a one-time flag so cleanup doesn’t run multiple times.

## Notes
- This issue only covers startup cleanup. Follow-ups can add periodic cleanup, metrics, or richer error reporting if needed.
