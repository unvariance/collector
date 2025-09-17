#pragma once

#include <bpf/bpf_helpers.h>

struct sync_timer_bitmap_entry {
    __u32 expected_cpu;
    __u64 trigger_mask;
};

static __always_inline int sync_timer_check_and_reset(void *timer_map, __u64 id)
{
    if (id >= 64)
        return 0;

    __u32 key = 0;
    struct sync_timer_bitmap_entry *entry = bpf_map_lookup_elem(timer_map, &key);
    if (!entry)
        return 0;

    __u64 bit = 1ULL << id;
    __u64 prev = entry->trigger_mask;
    entry->trigger_mask = prev & ~bit;
    return (prev & bit) ? 1 : 0;
}
