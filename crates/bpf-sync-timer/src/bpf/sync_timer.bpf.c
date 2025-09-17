// SPDX-License-Identifier: GPL-2.0
#include "vmlinux.h"
#include <bpf/bpf_helpers.h>
#include <bpf/bpf_tracing.h>

#include "sync_timer.bpf.h"
#include "sync_timer_bitmap.bpf.h"

/* Provided by userspace before load; becomes constant at load time */
const volatile __u64 sync_timer_interval_ns;

struct {
    __uint(type, BPF_MAP_TYPE_PERCPU_ARRAY);
    __uint(max_entries, 1);
    __type(key, __u32);
    __type(value, struct sync_timer_bitmap_entry);
} sync_timer_bitmap SEC(".maps");

static __always_inline void sync_timer_update_bitmap(__u32 expected_cpu)
{
    __u32 key = 0;
    struct sync_timer_bitmap_entry *entry = bpf_map_lookup_elem(&sync_timer_bitmap, &key);
    if (!entry)
        return;

    entry->expected_cpu = expected_cpu;
    entry->trigger_mask = ~0ull;
}

DEFINE_SYNC_TIMER(shared, sync_timer_update_bitmap, sync_timer_interval_ns);

char LICENSE[] SEC("license") = "GPL";
