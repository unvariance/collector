# Sub-Issue 07: Integration tests and CI wiring

## Summary
Add end-to-end tests that validate resctrl plugin behavior for Add/Remove events, pod group state and per-pod reconciliation counts, startup cleanup, and retries. Align tests with the implemented event model and APIs introduced while working on Issue #252 and its sub-issues:
- Events: `PodResctrlEvent::{AddOrUpdate(PodResctrlAddOrUpdate), Removed(PodResctrlRemoved)}`
- Pod group state: `ResctrlGroupState::{Exists(String), Failed}`
- Per-pod counters: `{ total_containers, reconciled_containers }`

Default integration tests run with a mocked resctrl filesystem provider and a test PID source for determinism. Hardware E2E runs on EC2 use the real resctrl filesystem and real PID enumeration.

## Scope
- Add an integration test crate (or module) covering:
  - Startup with preexisting pods/containers: plugin emits `AddOrUpdate` with `group_state = Exists(_) | Failed` and correct `{total,reconciled}` counts after reconcile.
  - Container add/update: event dedup; counts only change when a container transitions to `Reconciled`.
  - ENOSPC on first attempt → `AddOrUpdate` with `group_state = Failed` → `retry_group_creation` transitions to `Exists(path)`; follow-up container reconciliation improves counts.
  - `cleanup_on_start` removes only prefixed groups at resctrl root and root-level `mon_groups` (no traversal into control-group-local `mon_groups`). No pod events emitted for cleanup.
- Hardware E2E tests on EC2 (real resctrl):
  1) Preexisting containers assigned: Start plugin with running container(s). Verify it creates the pod group and assigns existing container tasks; observe `AddOrUpdate` and verify `/sys/fs/resctrl/<prefix>.../tasks` contains the expected PIDs.
  2) Post-start add container: After plugin start, create a new container in the pod; verify reconciliation adds tasks and counts improve if needed.
  3) RMID exhaustion and caller-driven retry: Pre-fill resctrl capacity using a distinct prefix (so startup cleanup doesn’t remove them); verify group creation fails (`group_state = Failed`). Then free one resource and call `retry_group_creation` or `retry_all_once`; expect transition to `Exists(path)` and tasks assigned.
- Wire tests into CI:
  - Default job (GitHub runners or generic VMs): run all scenarios using mocked resctrl FS and a test PID source for determinism.
  - Optional hardware job (EC2 with resctrl): run the same scenarios with the real resctrl FS provider (no mocks) and real PID enumeration to validate kernel behavior end‑to‑end.

## Shared Test Harness
- PID enumeration: for CI integration tests, inject a test PID source via the plugin’s `with_pid_source(...)` for determinism; for hardware runs, use the real PID source.
- Resctrl provider injection: parameterize `Resctrl<P>` over `FsProvider` from the `resctrl` crate:
  - Mocked run: `Resctrl<TestFs>` (or equivalent mock) simulates create/assign/list/delete and can inject `ENOSPC`.
  - Hardware run (optional): `Resctrl<RealFs>` uses the actual filesystem under `/sys/fs/resctrl`.
- Each scenario body is implemented once and executed under both providers, gated by platform/features as needed.

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
  - Post-start add test: create a new container; verify assignment and improved counts.
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

- Integration tests (mocked provider, deterministic PID source)
  - Add tests under `crates/nri-resctrl-plugin/tests/` or extend the existing `#[cfg(test)]` module to cover:
    - Initial synchronize with preexisting pods/containers: expect `AddOrUpdate` with `Exists(path)` and accurate counts; handle failure path with `Failed` when ENOSPC is injected for `create_dir`.
    - Container add/update: create pod, then container; simulate PIDs that converge; expect counts `{total=1,reconciled=1}`. Change PID source to simulate non-convergence and verify event dedup (no extra events unless counts change).
    - Capacity and retries: inject ENOSPC for `create_group` → `AddOrUpdate` with `Failed`; call `retry_group_creation` while ENOSPC persists → expect `Error::Capacity` and no event; clear ENOSPC → call `retry_group_creation` again → expect transition to `Exists(path)` and updated event; then call `retry_container_reconcile` to improve counts and emit exactly one updated `AddOrUpdate`.
    - `retry_all_once`: prepare multiple pods, one `Failed` (capacity) and one with `Exists(path)` + one `Partial` container; verify early-stop on capacity (instrument mock FS to count `create_dir` calls) and improved counts for the other pod.

- Cleanup tests (after Sub-Issue 06)
  - Introduce a mock FS listing API consistent with `read_child_dirs`; seed root and root `mon_groups` with mixed entries: prefixed/non-prefixed, files/dirs, and disappearing entries to simulate races.
  - Verify `Resctrl::cleanup_all()` returns a `CleanupReport` with accurate counters and removes only prefixed directories.
  - In plugin tests, set `cleanup_on_start=true` and ensure `ensure_mounted(auto_mount)` is invoked prior to cleanup; assert no pod events are emitted and the info log includes the counters (use log capture or a hook if available).

- Hardware E2E (optional job)
  - Provision resctrl-capable EC2 (e.g., m7i.metal-24xl). Install containerd + NRI and deploy the plugin binary.
  - Run scenarios mirroring mocked tests: preexisting assignment, post-start add, and capacity + retry (using a separate helper prefix for pre-filling to avoid startup cleanup).
  - Gate on label or branch; skip gracefully on unsupported kernels/instances.

- CI wiring
  - Default job: run mocked-provider integration tests on Linux runners. Gate hardware-only tests behind `#[cfg(target_os = "linux")]` and feature flags if needed.
  - Hardware job: separate workflow that provisions EC2, builds the plugin, runs the E2E scenarios, and collects logs/metrics as artifacts.

- Documentation
  - Update docs to reflect event model, counters, retry APIs, and cleanup behavior. Include brief runbooks for local runs and hardware job gating.
