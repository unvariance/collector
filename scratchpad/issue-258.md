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
  - At `synchronize()`, read `cleanup_on_start` and `auto_mount` from the existing `ResctrlPluginConfig` available on `self`; do not change `configure()` behavior and do not persist any extra state.
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
    - Factor common logic into a helper that performs “list child dirs → filter by prefix → remove and update counters” for a given base path. Reuse this helper for both the resctrl root and the root `mon_groups` directory to avoid duplication.
    - Using `read_child_dirs`, list immediate child directories under the configured root. Count non-matching directories to `non_prefix_groups`. For those matching `group_prefix`, attempt `remove_dir` and update `removed`, `removal_race` (ENOENT), or `removal_failures` counters.
    - Also look for `<root>/mon_groups`: if present, list its immediate child directories and invoke the same helper, updating the same counters.
    - Return `Ok(CleanupReport)` with the four counters.
  - Keep sanitization minimal: strict prefix match only; UID parsing/sanitization remains the responsibility of group creation.
  - Refactor mounting: change `ensure_mounted(auto_mount: bool) -> Result<()>` and remove `auto_mount` from `Config`. Update crate docs to emphasize caller responsibility for mounting.

- nri-resctrl-plugin
  - Do not maintain a per-plugin guard flag; rely on NRI’s single `synchronize()` per `configure()`.
  - Ensure `ResctrlPlugin::new()` respects the provided `ResctrlPluginConfig` without overriding values; it must pass `group_prefix` and `auto_mount` through to the internal `resctrl::Config` unchanged.
  - Unit-test using DI (`with_resctrl`, `with_pid_source`). For E2E, use `ResctrlPlugin::new()` so we exercise the real constructor and configuration flow.

## Detailed Test Plan

- resctrl crate unit tests (using mock `FsProvider`):
  - Mixed entries at root and mon_groups: under `/sys/fs/resctrl` create `pod_a`, `pod_b`, `info` (dir), `other` (dir); under `/sys/fs/resctrl/mon_groups` create `pod_m1`, `np_m2`. Verify that only `pod_*` are removed across both locations; others remain. Assert all `CleanupReport` counters: `removed=3`, `non_prefix_groups=2`, `removal_failures=0`, `removal_race=0`.
  - Permission denied on one directory: make removal of `pod_b` return `EACCES`; assert `removal_failures=1`, `removed=2`.
  - Removal race: introduce a minimal scripted test FS for this unit test which allows `remove_dir` to return `ENOENT` for a specific directory name (or inject a post-listing hook). Use it to simulate the directory disappearing between list and remove; assert `removal_race=1` and adjust `removed`/`removal_failures` counters accordingly.
  - Idempotency: calling `cleanup_all()` twice removes the same initial set once; second call yields `removed=0` and unchanged other counters.

- nri-resctrl-plugin tests:
  - Startup cleanup + coexistence + logging (single test): pre-populate root and `mon_groups` as above. Create plugin with `cleanup_on_start=true`. Include a pod in the `synchronize()` call. Assert cleanup ran first (stale removed) and pod handling proceeds normally (group created for the pod and one Add/Update event emitted). Capture logs and verify an info-level entry with the four counters is present. Assert no `PodResctrlEvent` is emitted for cleanup-only actions (i.e., only the pod’s normal event is present).
  - Mount responsibility: a separate test verifying the plugin calls `ensure_mounted(cfg.auto_mount)` before cleanup. If a dedicated test for `auto_mount=false` behavior is missing elsewhere, add one here and note it as a filled test gap (outside this sub-issue’s core logic).
  - Config pass-through: construct the plugin with non-default values for all config fields (`group_prefix`, `cleanup_on_start`, `max_reconcile_passes`, `concurrency_limit`, `auto_mount`). In a unit test (module scope or via a test-only accessor), assert the plugin’s internal config mirrors all provided values. Keep this test pure (no filesystem actions).

### End-to-End Test (Integration)

Goal: exercise the full cleanup flow against a real resctrl mount using `RealFs`, validating both root and root `mon_groups` traversal. This complements the unit tests (which use a mock FS) and proves behavior in a live environment.

- Location: add a new test case to the existing `crates/nri-resctrl-plugin/tests/integration_test.rs` (reuse existing helpers and setup to avoid duplication).
- Guard: run only when `RESCTRL_E2E=1` is set, on Linux, and when the test process has permissions to mount and modify `/sys/fs/resctrl`. Otherwise, skip.
- Pre-conditions:
  - Ensure resctrl is mounted by calling `Resctrl::ensure_mounted(true)` prior to starting the plugin (and the plugin will also call `ensure_mounted(auto_mount)`). Do not shell out.
- Setup:
  - Choose a unique prefix that cannot conflict with other E2E tests: `test_e2e_`.
  - Create root-level directories: `/sys/fs/resctrl/test_e2e_a`, `/sys/fs/resctrl/test_e2e_b`, plus a non-prefix directory `/sys/fs/resctrl/np_e2e_c` (if it doesn’t already exist). Do not touch `info`.
  - Ensure `/sys/fs/resctrl/mon_groups` exists; create `/sys/fs/resctrl/mon_groups/test_e2e_m1` and `/sys/fs/resctrl/mon_groups/np_e2e_m2`.
- Execute:
  - Start a plugin instance using `ResctrlPlugin::new(...)` with `group_prefix = "test_e2e_"`, `cleanup_on_start = true`, and `auto_mount = true`.
  - Call `configure()` (dummy values are fine), then call `synchronize()` with an empty set of pods and containers.
- Verify:
  - Root: `test_e2e_a` and `test_e2e_b` are removed; `np_e2e_c` is intact; `info` untouched.
  - mon_groups: `test_e2e_m1` is removed; `np_e2e_m2` intact.
  - No `PodResctrlEvent` was emitted during cleanup.
  - Do not capture logs in E2E; rely on filesystem verification only. Log assertions are covered in unit tests.
- Teardown:
  - Remove any remaining test artifacts under `/sys/fs/resctrl/mon_groups` and root that match `test_e2e_`.
  - Do not unmount resctrl from the test; leave system state intact.

### CI Integration

- Builder job (GitHub-hosted):
  - Build the plugin integration tests binary without running them:
    - `cargo test -p nri-resctrl-plugin --tests --release --no-run`
  - Collect the integration test binary (e.g., `integration_test-*`), rename to `nri-resctrl-plugin-e2e`, and upload as an artifact.
- Hardware job (resctrl-capable runner):
  - Download the `nri-resctrl-plugin-e2e` artifact and run it with:
    - `RESCTRL_E2E=1 ./nri-resctrl-plugin-e2e --nocapture`
  - This keeps Cargo off the runner while exercising the plugin against a real resctrl mount.

Note: normal nri-resctrl-plugin unit tests remain on a mock FS; this end-to-end test uses `RealFs` and a real kernel resctrl mount.

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
