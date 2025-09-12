# resctrl-plugin 05: Resource exhaustion handling and retry APIs (updated)

Updated to match current code and terminology introduced while implementing Issue #252.

## Summary
Handle resctrl RMID/monitoring capacity exhaustion on group creation and expose caller-invoked retry APIs to reattempt group creation and/or container reconciliation later. No internal timers or background loops.

## Current Terminology (as implemented)
- `crates/resctrl` errors: `Error::Capacity` maps ENOSPC; `Error::NotMounted`, `Error::NoPermission`, `Error::Unsupported`, `Error::Io` exist.
- Plugin crate: `crates/nri-resctrl-plugin`.
  - Events: `PodResctrlEvent::{AddOrUpdate(PodResctrlAddOrUpdate), Removed(PodResctrlRemoved)}`.
  - Group state: `ResctrlGroupState::{Exists(String), Failed}`.
  - Per-pod payload: `PodResctrlAddOrUpdate { pod_uid, group_state, total_containers, reconciled_containers }`.
  - Internal tracking:
    - Per pod: `{ group_state, total_containers, reconciled_containers }`.
    - Per container: `{ pod_uid, state: ContainerSyncState::{NoPod, Partial, Reconciled} }`.

## Scope
- `crates/resctrl` (confirm/retain):
  - Ensure ENOSPC maps to `Error::Capacity` for both `create_group` and `assign_tasks` (already true via `map_basic_fs_error` and per-call handling).

- `crates/nri-resctrl-plugin`:
  - On group creation failure (e.g., ENOSPC): emit `PodResctrlEvent::AddOrUpdate` with `group_state = ResctrlGroupState::Failed` and container counts reflecting current knowledge (`reconciled_containers = 0`).
  - Track pod/containers in the existing internal maps; rely on `ResctrlGroupState` and the per-pod container counts (no separate assignment enum).
  - Expose retry APIs (invoked by the embedding application):
    - `retry_group_creation(&self, pod_uid: &str) -> resctrl::Result<ResctrlGroupState>`
      - If current `group_state` is `Failed`, call `resctrl.create_group(pod_uid)`.
      - On success, set `group_state = Exists(path)`.
      - Emit `AddOrUpdate` if `group_state` changes. Return the new `ResctrlGroupState`.
      - On `Error::Capacity`, leave state as `Failed` and do not emit a duplicate event.
    - `retry_container_reconcile(&self, container_id: &str) -> resctrl::Result<ContainerSyncState>`
      - Look up the container; if its pod's `group_state` is `Exists(path)`, reconcile just this container using the current cgroup path and `max_reconcile_passes`.
      - Do not attempt reconciliation for containers in `NoPod` state or when the pod group is `Failed`.
      - Update the container's `ContainerSyncState` accordingly and, if it transitions from `Partial` to `Reconciled`, increment the pod's `reconciled_containers` by one. Do not rescan all containers. Emit `AddOrUpdate` only if the count changed.
    - `retry_all_once(&self) -> resctrl::Result<()>`
      - Iterate all pods. For `Failed`, call `retry_group_creation` until the first `Error::Capacity` is encountered, then skip further group-creation retries in this pass.
      - Iterate the global container data and, for containers in `Partial` state, call `retry_container_reconcile`.

## Out of Scope
- Internal timers/backoff or autonomous retries; cadence is caller-controlled.
- Changes to the event model beyond the above (keep `AddOrUpdate` + counts).

## Deliverables / Acceptance
- Correct `AddOrUpdate` emission with `group_state = Failed` on capacity errors at group creation.
- Retry APIs implemented as above, emitting events only when pod state changes (group_state and/or counts).
- Unit tests covering (see Testing for details):
  - ENOSPC mapping to `Error::Capacity` (resctrl) and `Failed` event emission (plugin).
  - `retry_group_creation`: first attempt `Capacity` → no state change; second attempt success → transitions to `Exists(path)` and emits updated counts.
  - `retry_container_reconcile`: improves `reconciled_containers` after additional PIDs appear, with deduped events.
  - `retry_all_once`: early-stop on first `Capacity` for Failed pods; still reconciles pods with existing groups.

## Implementation Notes
- Locking and ordering:
  - Use the existing `Mutex<InnerState>` to guard state. Avoid holding the lock across filesystem operations:
    - Under lock, snapshot required data (e.g., current `group_state`, list of container IDs for the pod, and `max_reconcile_passes`).
    - Drop the lock to perform `resctrl.create_group` and per-container `reconcile_group` calls.
    - Reacquire the lock and re-read current state before mutating it. If the state has changed (e.g., group already created by another thread, containers added/removed), adjust behavior accordingly:
      - For `retry_group_creation`, if `group_state` is now `Exists(_)`, treat the create as idempotent and do not emit (the actor that performed the transition is responsible for the event).
      - For reconciliation, re-read the container's state and, if it is still `Partial` and the reconcile succeeded, increment the pod's `reconciled_containers`. Avoid rescanning all containers.
    - Emit while holding the lock to preserve state/event ordering; keep critical sections short.
- Container enumeration:
  - Reuse `pid_source` and `nri::compute_full_cgroup_path(container, Some(pod))` to generate PIDs per container on demand during reconciliation.
  - Factor out an internal helper (e.g., `reconcile_container(...)`) that encapsulates: computing the full cgroup path, invoking `pid_source`, calling `resctrl.reconcile_group`, and returning the resulting `ContainerSyncState`. Use it from both `handle_new_container` and retry flows to avoid duplication.
- Event dedup:
  - Emit `AddOrUpdate` only when our operation changes the current state: either the pod's `group_state` transitions or the pod's counts are incremented by our action. Decide based on the new `PodState` and affected `ContainerState` after applying the operation.
  - For `retry_group_creation`, emit only on `group_state` transition (`Failed` → `Exists(path)`).
  - For reconciliation, emit only if we incremented `reconciled_containers`; update and persist the count atomically with the emission decision (no full rescan).
- Config knobs:
  - No new config needed;

## Dependencies
- Builds on prior sub-issues that introduced the plugin skeleton, pod/container tracking, and reconciliation.

## Testing
- Use existing test scaffolding in `crates/nri-resctrl-plugin`:
  - `TestFs` (mock `FsProvider`) to simulate ENOSPC, directory existence, and tasks file writes. Extend it to support per-path ENOSPC for `create_dir` and the ability to remove a path from the ENOSPC set during a test.
  - `MockCgroupPidSource` to control PID enumeration.
- Add targeted tests under `#[cfg(test)]` in the plugin module for retry flows and event dedup.

Test cases and setup details:

- Capacity error → Failed event
  - Setup: `TestFs` with `/sys/fs/resctrl` present and configure `create_dir` for the group path to return ENOSPC. Initialize the plugin with empty state and a mock PID source. Define a pod sandbox with `uid = u1`.
  - Action: Call the internal pod handler to process `RUN_POD_SANDBOX` so `create_group(u1)` is attempted.
  - Expect: One `AddOrUpdate` with `pod_uid = u1`, `group_state = Failed`, counts `0/0`.

- retry_group_creation transitions Failed → Exists (continuation)
  - Setup: Continue from the previous test state. Add one container `c1` for pod `u1` and process its add event through the internal container handler; verify an `AddOrUpdate` with counts `total=1, reconciled=0` is emitted while the pod remains `Failed`.
  - Action: Call `retry_group_creation("u1")` once while ENOSPC is still configured (expect `Err(Capacity)` and no event). Then remove the group path from the `TestFs` ENOSPC set and call `retry_group_creation("u1")` again.
  - Expect: First retry returns `Err(Capacity)` with no event. Second retry returns `Exists(path)` and emits one `AddOrUpdate` transitioning `group_state` to `Exists(_)` with counts still `1/0` (no accidental reset or increment).

- retry_container_reconcile improves counts
  - Setup: `TestFs` with `/sys/fs/resctrl/pod_u1` directory and a `tasks` file. One container `c1` belongs to the pod; `MockCgroupPidSource` initially returns PIDs that do not converge (simulate missing), then updated to return stable PIDs presentable in tasks. State shows `c1` as `Partial` and pod counts `total=1, reconciled=0`.
  - Action: Call `retry_container_reconcile("c1")` after updating `MockCgroupPidSource` to return assignable PIDs.
  - Expect: Container `c1` flips to `Reconciled`, pod counts become `total=1, reconciled=1`, and an `AddOrUpdate` event is emitted once. Re-running yields no event.

- retry_all_once behavior
  - Setup: Two pods: `uA` in `Failed` (capacity) and `uB` with `Exists(_)` and one `Partial` container. Configure `TestFs` so the first create attempt for `uA` yields ENOSPC and remains so. `MockCgroupPidSource` for `uB` returns assignable PIDs.
  - Action: Call `retry_all_once()`.
  - Expect: Group-creation retries stop after the first `Err(Capacity)` without attempting further failed pods. `uB` containers are reconciled; counts for `uB` improve and emit an event; `uA` state unchanged and no duplicate event. Verify early-stop by instrumenting `TestFs` to count `create_dir` invocations for resctrl group paths and asserting it is exactly one in this pass.
## Risks
- Retry loops can be noisy; rely on event dedup and caller-controlled cadence.
- Holding the lock while emitting events can back pressure if the channel is full; keep critical sections short and track `dropped_events()` (already exposed) for observability.
