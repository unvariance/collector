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
      - Update the container's `ContainerSyncState` accordingly and recompute the pod's counts; emit `AddOrUpdate` if counts change.
>> By re-computing, I'd like us to just increment the reconciled count if we changed the collector state. No heavyweight scans please.
    - `retry_all_once(&self) -> resctrl::Result<()>`
      - Iterate all pods. For `Failed`, call `retry_group_creation` until the first `Error::Capacity` is encountered, then skip further group-creation retries in this pass.
      - For pods with `Exists(_)`, iterate their containers in the container map and call `retry_container_reconcile` for each.
>> We do not keep a container map per pod; instead retry_all_once would iterate the container data structure and for containers in Partial state, call retry_container_reconcile.

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
      - For `retry_group_creation`, if `group_state` is now `Exists(_)`, treat the create as idempotent and do not emit unless transitioning from `Failed`.
>> why "unless transitioning"? If it's now Exists we can assume whoever changed it to Exists alrady emitted, there is no unless right?
      - For reconciliation, compute new counts from the container states after updating individual containers; do not rely on previously snapshotted counts.
>> No, we should assume that the data structures are consistent. We re-read the reconciled container's state. If it is still Partial and we successfully reconciled it, we can just increment the pod's reconciled count. No need to rescan all containers and we must not (too expensive).
    - Emit while holding the lock to preserve state/event ordering; keep critical sections short.
- Container enumeration:
  - Reuse `pid_source` and `nri::compute_full_cgroup_path(container, Some(pod))` to generate PIDs per container on demand during reconciliation.
  - Factor out an internal helper (e.g., `reconcile_container_locked_unlocked(...)`) that encapsulates: computing the full cgroup path, invoking `pid_source`, calling `resctrl.reconcile_group`, and returning the resulting `ContainerSyncState`. Use it from both `handle_new_container` and retry flows to avoid duplication.
>> What's the locked_unlocked in the name mean? can't we just call it reconcile_container?
- Event dedup:
  - Compare previous `PodState` to the new one. Only send `AddOrUpdate` when `group_state`, `total_containers`, or `reconciled_containers` change.
>> The statement about sending AddOrUpdate when something changes is correct. But we should not only compare previous PodState, it is confusing. We should actually compare what we did against the *new* PodState and ContainerState.
  - For `retry_group_creation`, emit only on `group_state` transition (`Failed` → `Exists(path)`).
  - For reconciliation, recompute counts from current container states after updates and emit only if the resulting counts differ from the stored pod counts; update the stored counts atomically with the emission decision.
>> again, we should update the counts incrementally, not recompute them. change this in all the document.
- Config knobs:
  - No new config needed;

## Dependencies
- Builds on prior sub-issues that introduced the plugin skeleton, pod/container tracking, and reconciliation.

## Testing
- Use existing test scaffolding in `crates/nri-resctrl-plugin`:
  - `TestFs` (mock `FsProvider`) to simulate ENOSPC, directory existence, and tasks file writes.
  - `MockCgroupPidSource` to control PID enumeration.
- Add targeted tests under `#[cfg(test)]` in the plugin module for retry flows and event dedup.

Test cases and setup details:

- Capacity error → Failed event
  - Setup: `TestFs` with `/sys/fs/resctrl` present and configure `create_dir` for the group path to return ENOSPC. Plugin with channel `(tx, rx)` and empty state. Define a pod sandbox with `uid = u1`.
  - Action: Trigger `RUN_POD_SANDBOX` (or call internal pod handler) so `create_group(u1)` is attempted.
>> call the internal handler please, don't want to use channels for this type of test.
  - Expect: Receive `AddOrUpdate` with `pod_uid = u1`, `group_state = Failed`, counts `0/0`.

- retry_group_creation transitions Failed → Exists
  - Setup: As above, but simulate first call to `create_dir` ENOSPC and second call success. This can be done by flipping a flag in `TestFs` between attempts or injecting a provider that returns ENOSPC once then succeeds.
>> The setup is identical to the previous test, so let's just join the two tests. This tests continues from where the previous test left off and we wouldn't need to repeat the setup.
>> We whouldn't flip a flag in TestFs or inject a provider. TestFs should support removing a folder from the ENOSPC config. If it doesn't we should add that (if so, specify it here)
>> We should also add a container, verify that we get an AddOrUpdate with 1/0. Then after second retry we should still have 1/0 (we will verify that there is no accidental reset or increment of the counts).
  - Action: First attempt: cause a Failed state; call `retry_group_creation("u1")` while ENOSPC is still configured. Then reconfigure `TestFs` to allow directory creation and call `retry_group_creation("u1")` again.
  - Expect: First retry returns `Err(Capacity)` and emits no new event; second retry returns `Exists(path)` and emits one `AddOrUpdate` transitioning `group_state` to `Exists(_)` with counts unchanged.

- retry_container_reconcile improves counts
  - Setup: `TestFs` with `/sys/fs/resctrl/pod_u1` directory and a `tasks` file. One container `c1` belongs to the pod; `MockCgroupPidSource` initially returns PIDs that do not converge (simulate missing), then updated to return stable PIDs presentable in tasks. State shows `c1` as `Partial` and pod counts `total=1, reconciled=0`.
  - Action: Call `retry_container_reconcile("c1")` after updating `MockCgroupPidSource` to return assignable PIDs.
  - Expect: Container `c1` flips to `Reconciled`, pod counts become `total=1, reconciled=1`, and an `AddOrUpdate` event is emitted once. Re-running yields no event.

- retry_all_once behavior
  - Setup: Two pods: `uA` in `Failed` (capacity) and `uB` with `Exists(_)` and one `Partial` container. Configure `TestFs` so the first create attempt for `uA` yields ENOSPC and remains so. `MockCgroupPidSource` for `uB` returns assignable PIDs.
  - Action: Call `retry_all_once()`.
  - Expect: Group-creation retries stop after the first `Err(Capacity)` without attempting further failed pods. `uB` containers are reconciled; counts for `uB` improve and emit an event; `uA` state unchanged and no duplicate event.
>> Do we have a way to verify that we stopped after the first capacity error? I guess we can count calls to create_dir in TestFs and verify that it is exactly one. Is that the best way to do that?
## Risks
- Retry loops can be noisy; rely on event dedup and caller-controlled cadence.
- Holding the lock while emitting events can back pressure if the channel is full; keep critical sections short and track `dropped_events()` (already exposed) for observability.
