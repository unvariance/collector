# Sub-Issue 07: Integration tests and CI wiring

## Summary
Add end-to-end tests that validate resctrl plugin behavior for Add/Remove events, pod group state and per-pod reconciliation counts, startup cleanup, and retries. Cover both regular containers and containers added to a running Pod via `kubectl debug` (ephemeral containers). Align tests with the implemented event model and APIs introduced while working on Issue #252 and its sub-issues:
- Events: `PodResctrlEvent::{AddOrUpdate(PodResctrlAddOrUpdate), Removed(PodResctrlRemoved)}`
- Pod group state: `ResctrlGroupState::{Exists(String), Failed}`
- Per-pod counters: `{ total_containers, reconciled_containers }`

Default integration tests run with a mocked resctrl filesystem provider and a test PID source for determinism. Hardware E2E runs on EC2 use the real resctrl filesystem and real PID enumeration.
Additionally, validate that a Pod initially synchronized with a subset of its containers (e.g., due to concurrent creation) is fully reconciled when late containers appear (including those added via `kubectl debug`).

## Scope
- Add an integration test crate (or module) covering:
  - Startup with preexisting pods/containers: plugin emits `AddOrUpdate` with `group_state = Exists(_) | Failed` and correct `{total,reconciled}` counts after reconcile.
  - Container add/update: event dedup; counts only change when a container transitions to `Reconciled`.
  - Post-start add container to a running Pod: include adding via `kubectl debug` (ephemeral container) and verify coverage by the plugin; counts and assignments update accordingly.
  - Post-start add Pod: exercise `RUN_POD_SANDBOX` followed by `CREATE_CONTAINER`; verify correct group creation, task assignment, and event emission.
  - Pod removal: remove a Pod that existed before plugin registration and one created after; verify `Removed` events and that resctrl cleanup occurs correctly.
  - ENOSPC on first attempt → `AddOrUpdate` with `group_state = Failed` → `retry_group_creation` transitions to `Exists(path)`; follow-up container reconciliation improves counts.
  - `cleanup_on_start` removes only prefixed groups at resctrl root and root-level `mon_groups` (no traversal into control-group-local `mon_groups`). No pod events emitted for cleanup.
- Hardware E2E tests on EC2 (real resctrl):
  1) Preexisting containers assigned: Start plugin with running container(s). Verify it creates the pod group and assigns existing container tasks; observe `AddOrUpdate` and verify `/sys/fs/resctrl/<prefix>.../tasks` contains the expected PIDs.
  2) Post-start add container: After plugin start, add a new container in the pod. Use both a regular container launch and `kubectl debug` to add an ephemeral container; verify reconciliation adds tasks and counts improve if needed.
  3) Post-start add Pod: create a new Pod after plugin start (triggering `RUN_POD_SANDBOX` and `CREATE_CONTAINER`); verify group creation and assignments.
  4) Pod removal: remove a preexisting Pod and a newly created Pod; verify `Removed` events and that resctrl groups are deleted or left consistent per policy.
  5) RMID exhaustion and caller-driven retry: Pre-fill resctrl capacity using a distinct prefix (so startup cleanup doesn’t remove them); verify group creation fails (`group_state = Failed`). Then free one resource and call `retry_group_creation` or `retry_all_once`; expect transition to `Exists(path)` and tasks assigned.
- Wire tests into CI:
  - Default job (GitHub runners or generic VMs): run all scenarios using mocked resctrl FS and a test PID source for determinism.
  - Optional hardware job (EC2 with resctrl): run the same scenarios with the real resctrl FS provider (no mocks) and real PID enumeration to validate kernel behavior end‑to‑end.

## Shared Test Harness
- PID enumeration: for CI integration tests, inject a test PID source via the plugin’s `with_pid_source(...)` for determinism; for hardware runs, use the real PID source.
- Resctrl provider injection: parameterize `Resctrl<P>` over `FsProvider` from the `resctrl` crate:
  - Mocked run: `Resctrl<TestFs>` (or equivalent mock) simulates create/assign/list/delete and can inject `ENOSPC`.
  - Hardware run (optional): `Resctrl<RealFs>` uses the actual filesystem under `/sys/fs/resctrl`.
- Keep mock-based tests (unit/integration in-crate) and hardware E2E tests separate; do not attempt to reuse identical scenario code across providers. Mock tests validate logic deterministically; E2E validates kernel/runtime behavior.

- Exercising late-container reconciliation
  - Ensure scenarios cover pods initially synchronized with a subset of their containers (e.g., race between `synchronize()` and container creation).
  - Add a container to a running pod via `kubectl debug` (ephemeral container) and verify the plugin observes it and fully reconciles the pod: group creation (if needed), PID assignment, and counters update.

## Out of Scope
- Background/periodic retries; cadence is caller-driven via `retry_*` APIs.
- Emitting events for startup cleanup (cleanup is silent aside from logs).

## Success Criteria
- Mocked tests run in CI using mocked resctrl FS and a test PID source, executing the same scenario bodies as hardware where applicable.
- Hardware E2E passes on capable instances; detection/auto-mount behavior is validated where appropriate.
- Coverage for initial sync, container add/update, capacity failure and retries, and startup cleanup.
- Docs explain how to run locally and how hardware CI is gated.

## Implementation Notes
- Mocked path:
  - Use the plugin’s constructors that allow dependency injection: `ResctrlPlugin::with_pid_source(...)` and `Resctrl::with_provider(...)`.
  - Use a mock `FsProvider` to simulate ENOSPC, permission errors, and dynamic `tasks` contents; use a mock PID source to simulate preexisting and changing PIDs.
- Hardware path:
  - Ensure containerd+NRI are installed on EC2. Deploy the `nri-resctrl-plugin` binary with `cleanup_on_start=true` and a unique `group_prefix`.
  - Preexisting test: launch a pod/container before plugin start, then start plugin and observe `AddOrUpdate` and group assignment by inspecting `/sys/fs/resctrl/<prefix>.../tasks`.
  - Post-start add container: create a new container; verify assignment and improved counts. Also add an ephemeral container via `kubectl debug` to a running pod and verify coverage and reconciliation.
  - Post-start add Pod: create a new Pod after plugin start and verify group creation and task assignment (expect `RUN_POD_SANDBOX` then `CREATE_CONTAINER`).
  - Exhaustion test: pre-create many monitoring groups with a separate prefix until ENOSPC; verify `group_state = Failed`. Then delete one helper group and call `retry_group_creation` or `retry_all_once`; verify transition to `Exists(path)` and task assignment.
  - Keep prefixes distinct so startup cleanup doesn’t remove the exhaustion helpers.
  - Emit structured logs/metrics for precise assertions.

## Dependencies
- Sub-Issue 01–06, notably:
  - Sub-Issue 05 (retry APIs): `retry_group_creation`, `retry_container_reconcile`, `retry_all_once`.
  - Sub-Issue 06 (startup cleanup): `Resctrl::cleanup_all()` and plugin calling `ensure_mounted(auto_mount)` + cleanup at `synchronize()`.
- EC2 provisioning workflow and permissions for hardware jobs.

## Risks
- Test flakiness due to async timing; keep timeouts generous and deterministic with mocks.

## Detailed Plan

- Test surfaces and contracts
  - Validate event model: `PodResctrlEvent::{AddOrUpdate, Removed}` with `ResctrlGroupState` and per-pod counts.
  - Validate retry APIs: `retry_group_creation`, `retry_container_reconcile`, `retry_all_once`.
  - Validate startup cleanup semantics once Sub-Issue 06 lands: prefix-only deletion at root and root `mon_groups`; no pod events.
  - Validate late-container reconciliation: if `synchronize()` initially sees a subset of pod containers, verify follow-up events (including from `kubectl debug`) lead to full reconciliation.

- Integration tests (mocked provider, deterministic PID source)
  - Add tests under `crates/nri-resctrl-plugin/tests/` or extend the existing `#[cfg(test)]` module to cover:
    - Initial synchronize with preexisting pods/containers: expect `AddOrUpdate` with `Exists(path)` and accurate counts; handle failure path with `Failed` when ENOSPC is injected for `create_dir`.
    - Container add/update: create pod, then container; simulate PIDs that converge; expect counts `{total=1,reconciled=1}`. Change PID source to simulate non-convergence and verify event dedup (no extra events unless counts change). Include adding a container to a running pod via `kubectl debug` and verify plugin coverage.
    - Pod add post-start: create a Pod after plugin start (simulate `RUN_POD_SANDBOX` + `CREATE_CONTAINER`) and verify group creation, PID assignment, and events.
    - Pod removal: remove a preexisting Pod (present before plugin registration) and a post-registration Pod; expect `Removed` events and resctrl cleanup for both.
    - Capacity and retries: inject ENOSPC for `create_group` → `AddOrUpdate` with `Failed`; call `retry_group_creation` while ENOSPC persists → expect `Error::Capacity` and no event; clear ENOSPC → call `retry_group_creation` again → expect transition to `Exists(path)` and updated event; then call `retry_container_reconcile` to improve counts and emit exactly one updated `AddOrUpdate`.
    - `retry_all_once`: prepare multiple pods, one `Failed` (capacity) and one with `Exists(path)` + one `Partial` container; verify early-stop on capacity (instrument mock FS to count `create_dir` calls) and improved counts for the other pod.
    - Auto-mount behavior: `resctrl::Resctrl::ensure_mounted(auto_mount)` already has unit and smoke tests in the `resctrl` crate exercising mounted/unmounted cases for both `auto_mount=true|false`. Reference those rather than duplicating in plugin mock tests.

- Cleanup tests (after Sub-Issue 06)
  - Introduce a mock FS listing API consistent with `read_child_dirs`; seed root and root `mon_groups` with mixed entries: prefixed/non-prefixed, files/dirs, and disappearing entries to simulate races.
  - Verify `Resctrl::cleanup_all()` returns a `CleanupReport` with accurate counters and removes only prefixed directories.
  - In plugin tests, set `cleanup_on_start=true` and ensure `ensure_mounted(auto_mount)` is invoked prior to cleanup; assert no pod events are emitted and the info log includes the counters (use log capture or a hook if available).

- Hardware E2E (optional job)
  - Provision resctrl-capable EC2 (e.g., m7i.metal-24xl). Install containerd + NRI and deploy the plugin binary.
  - Run scenarios mirroring mocked tests: preexisting assignment, post-start add container (including `kubectl debug`), post-start add Pod, pod removal (preexisting and post-registration), and capacity + retry (using a separate helper prefix for pre-filling to avoid startup cleanup).
  - Gate on label or branch; skip gracefully on unsupported kernels/instances.

- Test inventory and deltas
  - In-crate unit/integration (mocked) in `crates/nri-resctrl-plugin/src/lib.rs`:
    - `test_cleanup_on_start_removes_only_prefix` — already exists; covers startup cleanup. Keep.
    - `test_configure_event_mask` — already exists; validates minimal NRI events set. Keep.
    - `test_reconcile_emits_counts` — already exists; initial synchronize counts. Expand to cover late-container reconciliation. Modify.
    - `test_run_pod_sandbox_creates_group_and_emits_event` — already exists; validates `RUN_POD_SANDBOX` and removal. Optionally add create-after-run assertion. Modify.
    - `test_capacity_error_emits_failed_and_retry_group_creation_transitions` — already exists. Keep.
    - `test_retry_container_reconcile_improves_counts` — already exists. Keep.
    - `test_retry_all_once_early_stop_on_capacity_and_reconcile_others` — already exists. Keep.
    - NEW: container added via `kubectl debug` (ephemeral) — add mocked test to simulate post-sync container; verify counts/assignments.
    - NEW: removal of a preexisting Pod (present before registration) — add test to expect `Removed` and cleanup.
  - E2E tests:
    - `crates/nri-resctrl-plugin/tests/integration_test.rs::test_resctrl_plugin_registers_with_nri` — exists; registration path. Keep.
    - `crates/nri-resctrl-plugin/tests/integration_test.rs::test_startup_cleanup_e2e` — exists; cleanup and ensure_mounted(true). Keep.
    - NEW: preexisting assignment and post-start container add (regular + `kubectl debug`).
    - NEW: post-start pod add and removal for both preexisting and post-registration pods.
  - Resctrl crate tests (mounting):
    - `crates/resctrl/tests/smoke_test.rs` and unit tests in `crates/resctrl/src/lib.rs` exercise `ensure_mounted` for mounted/unmounted with `auto_mount=true|false` — rely on these; no plugin duplication.

- CI wiring
  - Use existing `.github/workflows/test-resctrl.yaml`:
    - Build + unit tests job runs plugin mocked tests and builds binaries.
    - KIND job validates plugin registration against an NRI-enabled containerd.
    - Hardware jobs execute resctrl smoke and plugin E2E on EC2; opt-in via `RESCTRL_E2E=1`.

- Documentation
  - Update docs to reflect event model, counters, retry APIs, and cleanup behavior. Include brief runbooks for local runs and hardware job gating.
