# Sub-Issue 06: Startup cleanup of stale resctrl groups

Related epic: https://github.com/unvariance/collector/issues/252

## Summary
Add a startup cleanup that removes only our previously-created resctrl groups under `/sys/fs/resctrl` using the configured group prefix. This is controlled by `cleanup_on_start` (default true) and must align with the epic architecture: cleanup runs at startup, does not emit pod events, and is implemented via the `resctrl` crate. The cleanup operates at two locations: (1) root-level control groups, and (2) root-level monitoring groups under `/sys/fs/resctrl/mon_groups`. We do not traverse monitoring groups within individual control groups.

## Scope
- `crates/resctrl`
  - Add `Resctrl::cleanup_all(&self) -> Result<CleanupReport>` that scans the configured resctrl root and removes: (a) direct child directories (control groups) with names starting with `group_prefix`, and (b) direct children of `/sys/fs/resctrl/mon_groups` with names starting with `group_prefix`.
  - Return a `CleanupReport` struct with counters:
    - `removed`: number of groups successfully removed (root + root/mon_groups)
    - `removal_failures`: number of groups that matched the prefix but failed to remove (errors other than ENOENT)
    - `removal_race`: number of matched groups that disappeared between list and remove (ENOENT)
    - `non_prefix_groups`: number of groups observed that did not match `group_prefix` (root + root/mon_groups)
  - Internals use the crate’s `Config { root, group_prefix }` and filesystem provider for testability.
  - Mounted FS assumption: `cleanup_all` assumes resctrl is mounted. It returns errors if access fails. It does not call `ensure_mounted()`.
  - Error handling: best-effort removal. Failures to list root or `mon_groups` surface as errors; per-entry deletion errors increment counters as above while continuing the sweep. The method returns the `CleanupReport`.
- `crates/nri-resctrl-plugin`
  - If `cleanup_on_start` is true, call `ensure_mounted(auto_mount)` first, then invoke `cleanup_all()` before processing `synchronize()`.
  - Rely on NRI calling `synchronize()` once per `configure()` and avoid adding a plugin-level guard flag.
  - Log the `CleanupReport` at info level (all counters). Do not emit `PodResctrlEvent` for cleanup-only actions.

## Out of Scope
- Detecting/cleaning groups not created by this plugin (strictly prefix-based).
- Periodic/background cleanup or any retry loops beyond the epic’s state model.
- Emitting events for startup cleanup or making cleanup observable via NRI events.
- Cross-process coordination of cleanup across multiple plugin instances.

## Architecture Alignment (Epic #252)
- Cleanup is implemented in the `resctrl` crate (library) and invoked by the NRI plugin; the plugin remains a thin orchestrator.
- Prefix-based naming and deletion at two locations only: the resctrl root and the root’s `mon_groups` directory. We do not traverse control-group-local `mon_groups`.
- Mounting responsibility: the embedding component (plugin) must call `ensure_mounted(auto_mount)` prior to any resctrl operations. The `resctrl` crate does not auto-mount as part of each call. We will refactor `ensure_mounted(auto_mount: bool)` to take the `auto_mount` flag as a parameter and remove `auto_mount` from `Config`.
- No events are emitted for cleanup-only operations. Plugin continues to emit Add/Update/Removed events only for pod lifecycle transitions.

## Deliverables / Acceptance
- `Resctrl::cleanup_all(&self) -> Result<CleanupReport>` implemented with tests using a mock `FsProvider`.
- `CleanupReport` includes: `removed`, `removal_failures`, `removal_race`, and `non_prefix_groups`.
- `ResctrlPlugin` calls `ensure_mounted(auto_mount)` and runs cleanup at startup when `cleanup_on_start` is true, and logs the entire `CleanupReport` at info level.
- Only removes directories beginning with `group_prefix` from the resctrl root and from root-level `mon_groups`. Ignores non-matching directories (e.g., `info`) and all files.
- No pod events are emitted as part of cleanup.
- Documentation updated to include naming convention, cleanup behavior/safety, and clear mounting responsibility in crate docs.

## Detailed Implementation Plan
- resctrl crate
  - Extend `FsProvider` with a minimal directory API:
    - `fn read_child_dirs(&self, p: &Path) -> io::Result<Vec<String>>` returning the names of immediate sub-directories only.
  - Implement `RealFs` using `std::fs::read_dir` and `file_type().is_dir()`; map common errno values to existing `Error` variants (NoPermission, Io, etc.).
  - Update mock/test FS to support directory listing consistent with `read_child_dirs`.
  - Implement `Resctrl::cleanup_all(&self)`:
    - Assume mounted. Do not call `ensure_mounted()`.
    - Using `read_child_dirs`, list immediate child directories under the configured root. Count non-matching directories to `non_prefix_groups`. For those matching `group_prefix`, attempt `remove_dir` and update `removed`, `removal_race` (ENOENT), or `removal_failures` counters.
    - Also look for `<root>/mon_groups`: if present, list its immediate child directories and repeat the same filtering and removal logic, updating the same counters.
    - Return `Ok(CleanupReport)` with the four counters.
  - Keep sanitization minimal: strict prefix match only; UID parsing/sanitization remains the responsibility of group creation.
  - Refactor mounting: change `ensure_mounted(auto_mount: bool) -> Result<()>` and remove `auto_mount` from `Config`. Update crate docs to emphasize caller responsibility for mounting.

- nri-resctrl-plugin
  - On `configure()` store plugin config. On the subsequent `synchronize()` call: if `cfg.cleanup_on_start` is true, call `resctrl.ensure_mounted(cfg.auto_mount)` and then `resctrl.cleanup_all()`; log the full `CleanupReport` at info; proceed with pod handling.
  - Do not maintain a per-plugin guard flag; rely on NRI’s single `synchronize()` per `configure()`.
  - Unit-test using DI (`with_resctrl`, `with_pid_source`).

## Detailed Test Plan

- resctrl crate unit tests (using mock `FsProvider`):
  - Mixed entries at root and mon_groups: under `/sys/fs/resctrl` create `pod_a`, `pod_b`, `info` (dir), `other` (dir); under `/sys/fs/resctrl/mon_groups` create `pod_m1`, `np_m2`. Verify that only `pod_*` are removed across both locations; others remain. Assert all `CleanupReport` counters: `removed=3`, `non_prefix_groups=2`, `removal_failures=0`, `removal_race=0`.
  - Permission denied on one directory: make removal of `pod_b` return `EACCES`; assert `removal_failures=1`, `removed=2`.
  - Removal race: after listing, delete `pod_a` from the mock before `remove_dir`; assert `removal_race=1` and `removed`/`removal_failures` counters accordingly.
  - Idempotency: calling `cleanup_all()` twice removes the same initial set once; second call yields `removed=0` and unchanged other counters.

- nri-resctrl-plugin tests:
  - Startup cleanup and logging: pre-populate root and `mon_groups` as above. Create plugin with `cleanup_on_start=true`. Call `synchronize()` with empty state. Assert all matching directories are removed; verify a log record at info with the four counters (stub or capture logger where possible). Assert no `PodResctrlEvent` emitted.
  - Mount responsibility: add or update a test verifying the plugin calls `ensure_mounted(cfg.auto_mount)` before cleanup. If a dedicated test for `auto_mount=false` behavior is missing elsewhere, add one here and note it is a gap we’re filling (outside this sub-issue’s core logic).
  - Coexistence with active pods: include a pod in `synchronize()`; verify cleanup ran first (stale removed) and pod handling proceeds normally (group created for the pod and Add/Update event emitted once).

## Risks and Mitigations
- Accidental deletion: mitigated by strict prefix filter, limited traversal (root + root/mon_groups only), and tests covering mixed entries.
- Not mounted / insufficient permissions: the plugin is responsible for mounting via `ensure_mounted(auto_mount)`; cleanup reports errors and continues best-effort per entry.
- Re-entrancy: not a requirement; rely on NRI’s call pattern instead of an internal guard flag.

## Notes
- This issue only covers startup cleanup. Follow-ups can add periodic cleanup, metrics, or richer error reporting if needed.

Reviewer comment responses (not resolving):
- Added `CleanupReport` with counters for `removed`, `removal_failures`, `removal_race`, and `non_prefix_groups` (addresses requests to expose unsuccessful removals, non-prefix counts, and ENOENT races).
- Plugin now logs the full report at info level.
- Traversal clarified: remove matching control groups at root and matching monitoring groups under root `mon_groups`; do not traverse control-group-local `mon_groups`.
- Mounting clarified and refactored: `cleanup_all` assumes mounted; plugin calls `ensure_mounted(auto_mount)`. Plan to refactor `ensure_mounted` signature and remove `auto_mount` from `Config`.
- FS API simplified to `read_child_dirs(&self, p: &Path) -> io::Result<Vec<String>>` as requested.
- Test plan expanded to cover root and `mon_groups`, mixed entries, all counters, permission errors, and removal races; plus plugin mounting responsibility and info-level logging.
