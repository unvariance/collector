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
  - Track pod/containers in the existing internal maps; rely on `ResctrlGroupState` + counts instead of an `AssignmentState` enum.
>> regarding the "instead of AssignmentState" that refers to the old version of this sub-issue. Instead assume that the reader has not seen the old version, and just describe the current state of the code.
  - Expose retry APIs (invoked by the embedding application):
    - `retry_group_creation(&self, pod_uid: &str) -> resctrl::Result<ResctrlGroupState>`
      - If current `group_state` is `Failed`, call `resctrl.create_group(pod_uid)`.
      - On success, set `group_state = Exists(path)` and immediately attempt reconciliation for all known containers in the pod (see below).
>> let's not immediately attempt reconciliation here; leave that to `retry_pod_reconcile` to keep single responsibility      
      - Emit `AddOrUpdate` if group_state or counts change. Return the new `ResctrlGroupState`.
      - On `Error::Capacity`, leave state as `Failed` and do not emit a duplicate event.
    - `retry_pod_reconcile(&self, pod_uid: &str) -> resctrl::Result<(usize /*total*/, usize /*reconciled*/)>`
>> we should reconcile containers, not pods. We will not reconcile containers in NoPod state (these should never happen if the NRI plugin is correct, and we do not want to add complexity to our implementation). We will need to make sure to maintain Pod counts and emit an AddOrUpdate event if the container moves to Reconciled state.
      - For pods with `Exists(path)`, re-run reconciliation for all known containers of that pod using current cgroup paths and `max_reconcile_passes`.
      - Update `total_containers` and `reconciled_containers` accordingly; emit `AddOrUpdate` only if counts change.
    - `retry_all_once(&self) -> resctrl::Result<()>`
      - Iterate all pods. For `Failed`, call `retry_group_creation` until the first `Error::Capacity` is encountered, then skip further group-creation retries in this pass.
      - For pods with `Exists(_)`, call `retry_pod_reconcile`.
>> the reconcile is not per container and iterates the container data structure.

## Out of Scope
- Internal timers/backoff or autonomous retries; cadence is caller-controlled.
- Changes to the event model beyond the above (keep `AddOrUpdate` + counts).

## Deliverables / Acceptance
- Correct `AddOrUpdate` emission with `group_state = Failed` on capacity errors at group creation.
- Retry APIs implemented as above, emitting events only when pod state changes (group_state and/or counts).
- Unit tests covering:
  - ENOSPC mapping to `Error::Capacity` (resctrl) and `Failed` event emission (plugin).
  - `retry_group_creation`: first attempt `Capacity` → no state change; second attempt success → transitions to `Exists(path)` and emits updated counts.
  - `retry_pod_reconcile`: improves `reconciled_containers` after additional PIDs appear, with deduped events.
  - `retry_all_once`: early-stop on first `Capacity` for Failed pods; still reconciles pods with existing groups.
>> Can you please give more detail on each of these tests: what is the setup, what is the action, what is the expected result? How we set up mocks.

## Implementation Notes
- Locking and ordering:
  - Use the existing `Mutex<InnerState>` to guard state. Avoid holding the lock across filesystem operations:
    - Under lock, snapshot required data (e.g., current `group_state`, list of containers for a pod, config values).
    - Drop the lock to perform `resctrl.create_group` and `reconcile_group` calls.
    - Reacquire the lock to update state and decide whether to emit an event. Emit while holding the lock to preserve state/event ordering; keep critical sections short.
>> We need to be careful here in case the state changes between dropping and reacquiring the lock. For example, if we drop the lock to create a group, another thread could come in and create the group for us. When we reacquire the lock, we need to check if the group already exists before trying to create it again. This could lead to unnecessary errors or state changes. We should also consider the case where a container is added or removed while we're reconciling. We need to ensure that our snapshot is consistent and that we handle any changes that occur while we're working.
- Container enumeration:
  - Reuse `pid_source` and `nri::compute_full_cgroup_path(container, Some(pod))` to generate PIDs per container on demand during reconciliation.
>> can we refactor the reconciliation logic so we do not have duplicate code?
- Event dedup:
  - Compare previous `PodState` to the new one. Only send `AddOrUpdate` when `group_state`, `total_containers`, or `reconciled_containers` change.
>> we should compare the old group_state to the new group_state when we're doing a retry_group_creation and emit accordingly. For counts, we should consider the new container state, and whether our reconciliation was successful under the second lock. If we indeed changed the container state, we modify the pod counts (under the same lock) and emit an event if the counts changed. This means we do not look at the old counts, since they might have changed between lock acquisitions.
- Config knobs:
  - Reuse existing `max_reconcile_passes`, `group_prefix`, and `cleanup_on_start` as-is; no new config required for this sub-issue.
>> No need to use cleanup_on_start for this sub-issue.

## Dependencies
- Builds on prior sub-issues that introduced the plugin skeleton, pod/container tracking, and reconciliation.

## Testing
- Use existing test scaffolding in `crates/nri-resctrl-plugin`:
  - `TestFs` (mock `FsProvider`) to simulate ENOSPC, directory existence, and tasks file writes.
  - `MockCgroupPidSource` to control PID enumeration.
- Add targeted tests under `#[cfg(test)]` in the plugin module for retry flows and event dedup.

## Risks
- Retry loops can be noisy; rely on event dedup and caller-controlled cadence.
- Holding the lock while emitting events can back pressure if the channel is full; keep critical sections short and track `dropped_events()` (already exposed) for observability.

