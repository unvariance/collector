# Shared Sync Timer Integration

The `bpf-sync-timer` crate hosts the reusable eBPF timer program and helper
utilities. Downstream crates can depend on it to consume a shared timer tick
without owning their own timer lifecycle.

## Adding the dependency

```toml
[dependencies]
bpf-sync-timer = { workspace = true }
```

## Starting the timer and requesting an ID

```rust
use bpf_sync_timer::SyncTimer;

const INTERVAL_NS: u64 = 1_000_000; // 1 ms

let mut sync_timer = SyncTimer::start(INTERVAL_NS)?;
let subscriber_id = sync_timer.assign_id()?; // returns 0..=63
```

The `SyncTimer` handle keeps the timer program alive, so hold on to it for the
lifetime of your collector.

## Reusing the timer bitmap in another skeleton

`SyncTimer` exposes the bitmap map file descriptor so that other skeletons can
reuse it before they are loaded. You can pass a borrowed FD — libbpf will
duplicate it internally on `reuse_fd`.

```rust
let mut open = skel_builder.open(obj_buf)?;
let borrowed = sync_timer.borrowed_map_fd();
open.maps.sync_timer_bitmap.reuse_fd(borrowed)?;
// Set as const before load so it's baked into the program
open.maps.rodata_data.my_timer_subscriber_id = subscriber_id as u64;
let mut skel = open.load()?;
```

The bitmap map is not pinned anywhere; all programs hold references directly to
the shared map.

## Updating the BPF program

Include the shared bitmap helper header and declare the bitmap map:

```c
#include "sync_timer_bitmap.bpf.h"

struct {
    __uint(type, BPF_MAP_TYPE_PERCPU_ARRAY);
    __uint(max_entries, 1);
    __type(key, __u32);
    __type(value, struct sync_timer_bitmap_entry);
} sync_timer_bitmap SEC(".maps");

const volatile __u64 my_sync_timer_id;
```

Inside the `tracepoint/timer/hrtimer_expire_exit` handler call the helper to
check whether the current subscriber bit is set and clear it:

```c
SEC("tracepoint/timer/hrtimer_expire_exit")
int handle_hrtimer_expire_exit(void *ctx)
{
    if (my_sync_timer_id >= 64)
        return 0;

    if (!sync_timer_check_and_reset(&sync_timer_bitmap, my_sync_timer_id))
        return 0;

    struct sync_timer_bitmap_entry *entry;
    __u32 key = 0;
    entry = bpf_map_lookup_elem(&sync_timer_bitmap, &key);
    if (!entry)
        return 0;

    __u32 cpu = bpf_get_smp_processor_id();
    if (entry->expected_cpu != cpu) {
        // emit migration event
        return 0;
    }

    // normal timer handling here
    return 0;
}
```

Compare the expected CPU from the bitmap entry with `bpf_get_smp_processor_id()`
inside your tracepoint to detect timer migration.

## Legacy header

The legacy `sync_timer.bpf.h` callback API continues to live in the
`bpf-sync-timer/include` directory for use cases that want to manage their own
map updates instead of using the shared bitmap helper.
