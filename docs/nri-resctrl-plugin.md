# NRI Resctrl Plugin: Events, Counters, Retries, and Cleanup

## Overview

The NRI resctrl plugin manages per-pod resctrl groups under `/sys/fs/resctrl` and assigns container tasks into the appropriate pod group. It emits events to report pod group state and reconciliation progress.

## Event Model

- AddOrUpdate
  - Payload: `{ pod_uid, group_state, total_containers, reconciled_containers }`
  - `group_state`:
    - `Exists(path)`: resctrl pod group exists at `path`
    - `Failed`: group creation failed (e.g., ENOSPC/RMID exhaustion)
- Removed
  - Payload: `{ pod_uid }`
  - Emitted when a pod is removed; the plugin deletes its resctrl group (best effort).

Events are emitted on:
- Initial synchronize: one `AddOrUpdate` per pod (initially reporting `total_containers = 0` and `reconciled_containers = 0`), followed by an event whenever either counter changes.
- Pod sandbox lifecycle events (`RUN_POD_SANDBOX`, `REMOVE_POD_SANDBOX`).
- Container create/update/remove transitions that change the counters or the pod group state.

## Per-Pod Counters

- `total_containers`: number of known containers for the pod
- `reconciled_containers`: number of containers whose PIDs have been assigned to the pod group

Counters increment whenever a container becomes known, increment again when it reconciles successfully, and decrement on removal.

## Retries

- `retry_group_creation(pod_uid)`
  - Attempts to create the failed group again
  - On success, transitions `group_state` to `Exists(path)` and emits AddOrUpdate
  - On capacity error (ENOSPC), returns `Error::Capacity` and emits no event
- `retry_container_reconcile(container_id)`
  - Re-runs PID assignment for a specific container
  - Emits AddOrUpdate only if the container transitions to Reconciled (improving counts)
- `retry_all_once()`
  - Attempts a single pass across all failed pods and partial containers
  - Stops group-creation retries on the first capacity error encountered in this pass

## Cleanup Behavior

- On startup synchronize, when `cleanup_on_start=true` and resctrl is mounted:
  - Removes only resctrl groups at the root that start with the configured `group_prefix`
  - Removes only top-level `mon_groups` under the resctrl root that start with the prefix
  - Does not traverse into per-group `mon_groups`
  - Emits no pod events for cleanup-only activity

## Testing and CI

- Mocked unit/integration tests (default):
  - Use an in-memory filesystem provider and test PID source for determinism
  - Cover startup cleanup, synchronize counts, late-container reconcile, sandbox lifecycle, capacity failures, and retry flows
- Hardware/KIND E2E (optional):
  - Run on an EC2 runner with a KIND cluster configured for NRI
  - Validates that groups are created, tasks are assigned, counts improve, and cleanup occurs on pod removal
  - Tests are `#[ignore]` by default; CI enables them with `cargo test -- --ignored`
- CI wiring uses the orchestrator `.github/workflows/ci.yaml`, which invokes the reusable workflow `.github/workflows/reusable-test-resctrl.yaml` to run mocked tests on GitHub-hosted runners and execute the hardware/K3s jobs on EC2 when enabled.

### Test Inventory and Deltas

- `src/lib.rs` mocked tests (existing) cover auto-mount true/false, startup cleanup, retry flows, and now verify PID assignment via `list_group_tasks` in addition to event sequencing.
- `tests/integration_test.rs::test_plugin_full_flow` (new) drives a live KIND cluster: pre/post registration pods, `kubectl debug` ephemeral container addition, and pod removal cleanup.
- `src/lib.rs::test_cleanup_on_start_removes_only_prefix` (existing) covers startup cleanup with a mocked filesystem.
- `tests/integration_test.rs::test_startup_cleanup_e2e` (new) validates `cleanup_on_start` behavior against the real resctrl filesystem.
- `tests/integration_test.rs::test_capacity_retry_e2e` (new) exercises RMID exhaustion, retry flows, and PID verification on hardware.

## Runbooks

### Local (mocked) tests

```
cargo test -p nri-resctrl-plugin -- --nocapture
```

### Hardware E2E on a resctrl-capable host

1) Ensure `/sys/fs/resctrl` is mountable and you have CAP_SYS_ADMIN
2) Create a KIND cluster with NRI enabled and host mounts:

```
kind create cluster --config - <<'EOF'
kind: Cluster
apiVersion: kind.x-k8s.io/v1alpha4
nodes:
- role: control-plane
  extraMounts:
  - hostPath: /tmp/nri
    containerPath: /var/run/nri
  - hostPath: /tmp/nri-conf
    containerPath: /etc/nri/conf.d
  - hostPath: /tmp/nri-plugins
    containerPath: /opt/nri/plugins
containerdConfigPatches:
- |-
  [plugins."io.containerd.nri.v1.nri"]
    disable = false
    disable_connections = false
    socket_path = "/var/run/nri/nri.sock"
    plugin_config_path = "/etc/nri/conf.d"
    plugin_path = "/opt/nri/plugins"
EOF
```

3) Build and run integration tests against the KIND NRI socket:

```
export NRI_SOCKET_PATH=/tmp/nri/nri.sock
cargo test -p nri-resctrl-plugin --test integration_test -- --ignored --nocapture
```

The test `test_plugin_full_flow` creates pods before and after plugin registration, exercises an ephemeral container via `kubectl debug` (when available), and deletes pods to validate `Removed` events and cleanup.
