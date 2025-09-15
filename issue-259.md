# Sub-Issue 07: Integration tests and CI wiring

## Summary
Add end-to-end tests that validate resctrl plugin behavior for Add/Remove events, state transitions (Success/Partial/Failure), startup cleanup, and retries. Keep container runtime interaction and PID enumeration real, and mock only the resctrl filesystem via the `FsProvider` in the `resctrl` crate so tests run even on hosts without resctrl support.

## Scope
- Add an integration test crate (similar to `crates/nri/tests`) covering:
  - Startup with preexisting pods/containers: events emitted after reconcile.
  - Container add/update: event dedup and state changes.
  - ENOSPC on first attempt → Failure event → retry → Success path.
  - `cleanup_on_start` removes prefixed groups only.
- Hardware E2E tests on EC2 (real resctrl):
  1) Preexisting containers assigned: Start plugin with running container(s). Verify it creates the pod group and assigns existing container tasks; event `Added` with `Success` or `Partial` converging to `Success`.
  2) Post-start add container: After plugin start, create a new container in the pod; verify reconcile adds tasks and emits state change if needed.
  3) RMID exhaustion and caller-driven retry: Pre-fill resctrl capacity using a distinct prefix (so startup cleanup doesn’t remove them); verify group creation fails (`Failure`). Then free one resource (remove one of the prefilled groups) and invoke the plugin’s retry method (`retry_unavailable` or `retry_all_once`) from the test harness to validate it transitions the pod to `Success` and assigns tasks.
- Wire tests into CI:
  - Default job (GitHub runners or generic VMs): run all scenarios using mocked resctrl FS; real container runtime and PID enumeration.
  - Optional hardware job (EC2 with resctrl): run the same scenarios with the real resctrl FS provider (no mocks) to validate kernel behavior end‑to‑end.

## Shared Test Harness
- Use the real runtime (containerd/NRI) and real PID enumeration in all runs.
- Inject the resctrl dependency as an interface that is parameterized over `FsProvider` from the `resctrl` crate:
  - Mocked run: `Resctrl<MockFsProvider>` simulates group create/assign/list/delete and can inject `ENOSPC`.
  - Hardware run (optional): `Resctrl<RealFsProvider>` uses the actual filesystem under `/sys/fs/resctrl`.
- Each scenario is implemented once and instantiated twice via feature flags:
  - Default build: uses `MockFsProvider`.
  - `--features hw-e2e`: switches to `RealFsProvider`.

## Out of Scope
- None for hardware jobs; they specifically target real resctrl behavior.

- Mocked tests compile and run in CI using mocked resctrl FS (with real runtime and PIDs), running the exact same scenario bodies as hardware.
- Hardware E2E passes on capable instances; negative detection/auto-mount cases pass on non-capable instances.
- Coverage of success and failure paths for all event types, including retry-driven improvements.
- Simple docs on how to run locally and how the hardware CI jobs are gated.

## Implementation Notes
- Mocked path: use `MockFsProvider` in the resctrl crate and reuse real PID enumeration; simulate churn via the runtime.
- Hardware path:
  - Ensure containerd+NRI are installed on EC2. Deploy the `nri-resctrl-plugin` binary with `cleanup_on_start=true` and a unique `group_prefix`.
  - Preexisting test: launch a pod/container before plugin start, then start plugin and observe events and group assignment by inspecting `/sys/fs/resctrl/<prefix>.../tasks`.
  - Post-start add test: create a new container; verify assignment and event.
  - Exhaustion test: use a helper to create many monitoring groups with a separate prefix until ENOSPC; verify plugin Failure. Then delete one helper group and call the plugin retry method (`retry_unavailable`/`retry_all_once`); verify the plugin transitions to Success and tasks appear in the target group.
  - Keep prefixes distinct so cleanup doesn’t remove the exhaustion helpers.
  - Emit structured logs/metrics for precise assertions.

## Dependencies
- Sub-Issue 01–06.
- EC2 provisioning workflow and permissions for hardware jobs.

## Risks
- Test flakiness due to async timing; keep timeouts generous and deterministic with mocks.
