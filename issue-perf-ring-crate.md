# Move the `events` perf ring into `perf_events`

## Why
- Issue [#290](https://github.com/unvariance/collector/issues/290) already tracks splitting the sync timer helpers out of `crates/bpf`. The perf ring that backs the `events` map is the other large chunk of runtime-only logic that is still living in `bpf` user space glue (`crates/bpf/src/lib.rs:83` onwards).
- `PerfMapReader::new` in `crates/perf_events/src/map_reader.rs` already knows how to create the per-CPU `perf_event_array`, mmap the ring, and write the file descriptors back into the map. We should expose that as a reusable primitive so every consumer does not need to repeat those steps.
- Owning the ring setup inside `perf_events` leaves `crates/bpf` focused on loading the collector skeleton and dispatching events, and makes it possible to share the ring between future crates without threading raw file descriptors around manually.

## Goals
- `perf_events` exports an API that creates and owns the `events` perf ring (including the mmap storage and the backing `MapMut` fd updates), and exposes a handle for other crates to reuse.
- `crates/bpf` reuses the map FD that the new API prepares instead of calling `PerfMapReader::new(&mut skel.maps.events, ...)` directly.
- No functional regressions: the collector still receives samples from every CPU, watermark and page-size configuration stays configurable, and the existing dispatcher continues to read via `PerfMapReader`.

## Non-goals
- Rewriting the eBPF side of the `events` map (`crates/bpf/src/bpf/collector.bpf.c`) or changing record formats.
- Moving command/control maps (e.g. `sync_timer_states_collect`) into the new crate.

## Proposed plan
1. Introduce a new struct in `perf_events` (e.g. `EventsPerfRing`) backed by the current `PerfMapReader` implementation. It should:
   - Allocate the per-CPU perf event storage and keep it alive for the lifetime of the reader.
   - Expose both a borrowed `PerfMapReader` and the raw perf-event file descriptors so that other crates can reuse/pin the map as needed.
   - Hide the direct dependency on `libbpf_rs::MapMut` from consumers once initialization finished.
2. Provide helpers on that struct to hand out the perf-event array FD(s). One option is to expose the FD for CPU 0 plus accessors for the rest, or to return an `OwnedFd`/`BorrowedFd` view over the vector that `helpers::update_map_with_fds` already builds.
3. When loading the collector skeleton, call `OpenCollectorMaps::events.reuse_fd(...)` (or the equivalent `MapMut::reuse_fd`) with the FD supplied by `EventsPerfRing` before `load()` so the `events` map inside the skeleton shares the same backing perf ring.
4. Replace the direct call to `PerfMapReader::new` in `BpfLoader::new` with the new API and keep a handle to the reader so `poll_events` can continue to use it.
5. Update docs/tests as needed (e.g. note the new API in `crates/perf_events/README` if we add one) and ensure `cargo test -p perf_events` and `cargo test -p bpf` still pass.

## Open questions / follow-ups
- Decide on the ownership model for the perf event file descriptors (do we hand out borrowed references, or clone them via `dup` so multiple consumers can hold them?).
- Confirm whether we want to pin the perf-event array in bpffs for reuse across processes and expose a convenience for that while we are touching the setup code.
- Once this lands we can reassess whether any other perf map setup (hardware counters, etc.) should move into similar helpers.
