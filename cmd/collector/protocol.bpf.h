#ifndef PROTOCOL_BPF_H
#define PROTOCOL_BPF_H

#define TASK_COMM_LEN 16

// Message types for all events
enum msg_type {
    MSG_TYPE_PERF = 0,
    MSG_TYPE_TASK_METADATA = 1,  // Renamed from MSG_TYPE_RMID_ALLOC
    MSG_TYPE_TASK_FREE = 2,      // Renamed from MSG_TYPE_RMID_FREE
};

// Perf event map for benchmark output - extern declaration
/* Perf event map for benchmark output - actual definition */
struct events_map {
    __uint(type, BPF_MAP_TYPE_PERF_EVENT_ARRAY);
    __uint(key_size, sizeof(__u32));
    __uint(value_size, sizeof(__u32));
};

extern struct events_map events SEC(".maps");

// Define the event structures
struct perf_measurement_msg {
    __u64 timestamp;  // Must be first field for ring buffer ordering
    __u32 type;      // MSG_TYPE_PERF
    __u32 pid;       // Process ID (renamed from rmid)
    __u64 cycles_delta;
    __u64 instructions_delta;
    __u64 llc_misses_delta;
    __u64 time_delta_ns;
};

// Structure for task metadata messages (renamed from rmid_alloc_msg)
struct task_metadata_msg {
    __u64 timestamp;  // Must be first field for ring buffer ordering
    __u32 type;      // MSG_TYPE_TASK_METADATA
    __u32 pid;       // Process ID
    char comm[TASK_COMM_LEN];
};

// Structure for task free messages (renamed from rmid_free_msg)
struct task_free_msg {
    __u64 timestamp;  // Must be first field for ring buffer ordering
    __u32 type;      // MSG_TYPE_TASK_FREE
    __u32 pid;       // Process ID
};

// Struct for passing perf measurement parameters
struct perf_measurement_params {
    __u32 pid;
    __u64 cycles_delta;
    __u64 instructions_delta;
    __u64 llc_misses_delta;
    __u64 time_delta_ns;
    __u64 timestamp;
};

// Helper function to send task metadata messag
static __always_inline int send_task_metadata(void *ctx, __u32 pid, const char *comm, __u64 timestamp) {
    struct task_metadata_msg msg = {
        .timestamp = timestamp,
        .type = MSG_TYPE_TASK_METADATA,
        .pid = pid,
    };
    __builtin_memcpy(msg.comm, comm, TASK_COMM_LEN);

    return bpf_perf_event_output(ctx, &events, BPF_F_CURRENT_CPU, &msg, sizeof(msg));
}

// Helper function to send task free message
static __always_inline int send_task_free(void *ctx, __u32 pid, __u64 timestamp) {
    struct task_free_msg msg = {
        .timestamp = timestamp,
        .type = MSG_TYPE_TASK_FREE,
        .pid = pid,
    };
    
    return bpf_perf_event_output(ctx, &events, BPF_F_CURRENT_CPU, &msg, sizeof(msg));
}

// Helper function to send perf measurement data - static inline implementation
static __always_inline int send_perf_measurement(void *ctx, struct perf_measurement_params *params) {
    struct perf_measurement_msg msg = {
        .timestamp = params->timestamp,
        .type = MSG_TYPE_PERF,
        .pid = params->pid,
        .cycles_delta = params->cycles_delta,
        .instructions_delta = params->instructions_delta,
        .llc_misses_delta = params->llc_misses_delta,
        .time_delta_ns = params->time_delta_ns
    };
    
    return bpf_perf_event_output(ctx, &events, BPF_F_CURRENT_CPU, &msg, sizeof(msg));
}

#endif // PROTOCOL_BPF_H 