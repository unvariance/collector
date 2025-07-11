/* SPDX-License-Identifier: (LGPL-2.1 OR BSD-2-Clause) */
#ifndef __COLLECTOR_H
#define __COLLECTOR_H

#define TASK_COMM_LEN 16

// Message types
enum msg_type {
    MSG_TYPE_TASK_METADATA = 1,
    MSG_TYPE_TASK_FREE = 2,
    MSG_TYPE_TIMER_FINISHED_PROCESSING = 3,
    MSG_TYPE_PERF_MEASUREMENT = 4,
    MSG_TYPE_TIMER_MIGRATION_DETECTED = 5,
};

// Sample header structure that matches the one in reader.rs
struct sample_header {
    __u32 size;      // Size field (filled by kernel)
    enum msg_type type;      // Message type
    __u64 timestamp; // Timestamp of the event
};

// Structure for task metadata messages
struct task_metadata_msg {
    struct sample_header header; // Common header
    __u32 pid;                   // Process ID
    __u8 comm[TASK_COMM_LEN];    // Process command name
    __u64 cgroup_id;             // Cgroup ID (inode number in cgroup filesystem)
};

// Structure for task free messages
struct task_free_msg {
    struct sample_header header; // Common header
    __u32 pid;                   // Process ID
};

// Structure for timer finished processing messages
struct timer_finished_processing_msg {
    struct sample_header header; // Common header
    // No additional data needed, timestamp in header is sufficient
};

// Structure for performance measurement messages
struct perf_measurement_msg {
    struct sample_header header; // Common header
    __u32 pid;                   // Process ID
    __u64 cycles_delta;          // CPU cycles delta
    __u64 instructions_delta;    // Instructions delta
    __u64 llc_misses_delta;      // LLC misses delta
    __u64 cache_references_delta; // Cache references delta
    __u64 time_delta_ns;         // Time delta in nanoseconds
    __u32 is_context_switch;     // 1 if context switch event, 0 if timer event
    __u32 next_tgid;             // Thread group ID of the process being context switched in. Only valid when is_context_switch == 1
};

// Structure for timer migration detection messages
struct timer_migration_msg {
    struct sample_header header; // Common header
    __u32 expected_cpu;          // CPU ID the timer was supposed to fire on
    __u32 actual_cpu;            // CPU ID the timer actually fired on
};

#endif /* __COLLECTOR_H */ 