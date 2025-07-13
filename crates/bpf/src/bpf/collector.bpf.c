// SPDX-License-Identifier: GPL-2.0
#include "vmlinux.h"
#include <bpf/bpf_helpers.h>
#include <bpf/bpf_core_read.h>

#include "collector.h"
#include "sync_timer.bpf.h"
// Map to track which tasks have had metadata reported
struct {
    __uint(type, BPF_MAP_TYPE_TASK_STORAGE);
    __uint(map_flags, BPF_F_NO_PREALLOC);
    __type(key, int);
    __type(value, __u64);
} task_metadata_storage SEC(".maps");

// Hash map to track exited group leaders that need to be reported during process_free
struct {
    __uint(type, BPF_MAP_TYPE_HASH);
    __uint(max_entries, 8192);
    __type(key, __u32);  // PID of the exited group leader
    __type(value, __u8); // Just a presence indicator
} exited_leaders SEC(".maps");

// Performance event output for events
struct {
    __uint(type, BPF_MAP_TYPE_PERF_EVENT_ARRAY);
    __uint(key_size, sizeof(u32));
    __uint(value_size, sizeof(u32));
} events SEC(".maps");

// Timer firing state tracking
enum timer_fire_state {
    TIMER_RESET = 0,
    TIMER_FIRED = 1,
    TIMER_MIGRATION_DETECTED = 2,
};

// Structure to track timer firing state with expected CPU
struct timer_fire_info {
    enum timer_fire_state state;
    __u32 expected_cpu;
};

// Per-CPU array to track timer firing state and expected CPU
struct {
    __uint(type, BPF_MAP_TYPE_PERCPU_ARRAY);
    __uint(key_size, sizeof(__u32));
    __uint(value_size, sizeof(struct timer_fire_info));
    __uint(max_entries, 1);
} timer_fired SEC(".maps");

// Declare the perf event arrays for hardware counters
struct {
    __uint(type, BPF_MAP_TYPE_PERF_EVENT_ARRAY);
    __uint(key_size, sizeof(__u32));
    __uint(value_size, sizeof(__u32));
} cycles SEC(".maps");

struct {
    __uint(type, BPF_MAP_TYPE_PERF_EVENT_ARRAY);
    __uint(key_size, sizeof(__u32));
    __uint(value_size, sizeof(__u32));
} instructions SEC(".maps");

struct {
    __uint(type, BPF_MAP_TYPE_PERF_EVENT_ARRAY);
    __uint(key_size, sizeof(__u32));
    __uint(value_size, sizeof(__u32));
} llc_misses SEC(".maps");

struct {
    __uint(type, BPF_MAP_TYPE_PERF_EVENT_ARRAY);
    __uint(key_size, sizeof(__u32));
    __uint(value_size, sizeof(__u32));
} cache_references SEC(".maps");

// Structure to store previous counter values per CPU
struct prev_counters {
    __u64 cycles;
    __u64 instructions;
    __u64 llc_misses;
    __u64 cache_references;
    __u64 timestamp;
};

// Per-CPU map to store previous counter values
struct {
    __uint(type, BPF_MAP_TYPE_PERCPU_ARRAY);
    __uint(max_entries, 1);
    __type(key, __u32);
    __type(value, struct prev_counters);
} prev_counters_map SEC(".maps");

// Dummy instances to make skeleton generation work
enum msg_type msg_type_ = 0;
struct task_metadata_msg task_metadata_msg_ = {0};
struct task_free_msg task_free_msg_ = {0};
struct timer_finished_processing_msg timer_finished_processing_msg_ = {0};
struct perf_measurement_msg perf_measurement_msg_ = {0};
struct timer_migration_msg timer_migration_msg_ = {0};
enum timer_fire_state timer_fire_state_ = 0;

// Initialize value for task storage
static const __u64 TASK_METADATA_INIT = 0;  // 0 = not reported yet
// Value to store in the exited_leaders map
static const __u8 LEADER_PRESENT = 1;

// Helper function to check if a task is a kernel thread
static __always_inline int is_kernel_thread(struct task_struct *task)
{
    if (!task)
        return 0;
    
    // Kernel threads either have PF_KTHREAD flag or no mm
    return (task->flags & 0x00200000 /* PF_KTHREAD */) || !task->mm;
}

// Send task metadata to userspace
// Note: This function should be called with the current task as it collects the cgroup ID
// of the current task context using bpf_get_current_cgroup_id().
static __always_inline int send_task_metadata(void *ctx, struct task_struct *task)
{
    if (!task)
        return 0;
    
    struct task_metadata_msg msg = {};
    
    msg.header.type = MSG_TYPE_TASK_METADATA;
    // size field is filled by the kernel
    msg.pid = task->pid;
    
    bpf_probe_read_kernel_str(&msg.comm, sizeof(msg.comm), task->comm);
    
    // Get cgroup ID for the current task
    msg.cgroup_id = bpf_get_current_cgroup_id();

    msg.header.timestamp = bpf_ktime_get_ns();
    
    // Skip the size field (first 4 bytes) when sending
    return bpf_perf_event_output(ctx, &events, BPF_F_CURRENT_CPU, 
                                ((void*)&msg) + sizeof(__u32), 
                                sizeof(msg) - sizeof(__u32));
}

// Send task free event to userspace
static __always_inline int send_task_free(void *ctx, __u32 pid)
{
    struct task_free_msg msg = {};
    
    msg.header.type = MSG_TYPE_TASK_FREE;
    // size field is filled by the kernel
    msg.pid = pid;
    msg.header.timestamp = bpf_ktime_get_ns();
    
    // Skip the size field (first 4 bytes) when sending
    return bpf_perf_event_output(ctx, &events, BPF_F_CURRENT_CPU, 
                                ((void*)&msg) + sizeof(__u32), 
                                sizeof(msg) - sizeof(__u32));
}

// Helper function to compute delta with wraparound handling
static __always_inline __u64 compute_delta(__u64 current, __u64 previous) {
    return current - previous;
}

// Send perf measurement event to userspace
static __always_inline int send_perf_measurement(void *ctx, __u32 pid, __u64 cycles_delta, 
                                               __u64 instructions_delta, __u64 llc_misses_delta,
                                               __u64 cache_references_delta, __u64 time_delta_ns, __u64 timestamp,
                                               __u32 is_context_switch, __u32 next_tgid)
{
    struct perf_measurement_msg msg = {};
    
    msg.header.timestamp = timestamp;
    msg.header.type = MSG_TYPE_PERF_MEASUREMENT;
    // size field is filled by the kernel
    msg.pid = pid;
    msg.cycles_delta = cycles_delta;
    msg.instructions_delta = instructions_delta;
    msg.llc_misses_delta = llc_misses_delta;
    msg.cache_references_delta = cache_references_delta;
    msg.time_delta_ns = time_delta_ns;
    msg.is_context_switch = is_context_switch;
    msg.next_tgid = next_tgid;
    
    // Skip the size field (first 4 bytes) when sending
    return bpf_perf_event_output(ctx, &events, BPF_F_CURRENT_CPU, 
                                ((void*)&msg) + sizeof(__u32), 
                                sizeof(msg) - sizeof(__u32));
}

// Send timer migration detection event to userspace
static __always_inline int send_timer_migration_alert(void *ctx, __u32 expected_cpu, __u32 actual_cpu)
{
    struct timer_migration_msg msg = {};
    
    msg.header.timestamp = bpf_ktime_get_ns();
    msg.header.type = MSG_TYPE_TIMER_MIGRATION_DETECTED;
    // size field is filled by the kernel
    msg.expected_cpu = expected_cpu;
    msg.actual_cpu = actual_cpu;
    
    // Skip the size field (first 4 bytes) when sending
    return bpf_perf_event_output(ctx, &events, BPF_F_CURRENT_CPU, 
                                ((void*)&msg) + sizeof(__u32), 
                                sizeof(msg) - sizeof(__u32));
}

// Check and report task metadata if needed
// This function should be called with the current task since send_task_metadata
// collects cgroup ID from the current task context.
static __always_inline int check_and_send_metadata(void *ctx, struct task_struct *task)
{
    if (!task || is_kernel_thread(task))
        return 0;
    
    // Use group leader for process identification
    struct task_struct *leader = task->group_leader;
    if (!leader)
        return 0;
    
    // Get or create metadata tracking entry
    __u64 *reported = bpf_task_storage_get(&task_metadata_storage, leader, 
                                          (void *)&TASK_METADATA_INIT, 
                                          BPF_LOCAL_STORAGE_GET_F_CREATE);
    
    if (!reported)
        return 0;

    if (*reported == 1)
        return 0;
    
    // Use atomic compare-and-swap to check and update the reported status
    if (__sync_val_compare_and_swap(reported, 0, 1) == 0) {
        // We're the first to report this task
        return send_task_metadata(ctx, leader);
    }
    
    return 0;
}

// Collect and report performance measurements
static __always_inline int collect_and_send_perf_measurements(void *ctx, struct task_struct *task, __u32 is_context_switch, __u32 next_tgid)
{
    // Skip if null task
    if (!task)
        return 0;
    
    __u32 pid = task->tgid;
    
    // Get previous counters
    __u32 zero = 0;
    struct prev_counters *prev = bpf_map_lookup_elem(&prev_counters_map, &zero);
    if (!prev) {
        return 0;  // Should never happen since it's a per-CPU array
    }
    
    // Read current counter values
    struct bpf_perf_event_value cycles_val = {};
    struct bpf_perf_event_value instructions_val = {};
    struct bpf_perf_event_value llc_misses_val = {};
    struct bpf_perf_event_value cache_references_val = {};
    
    __u64 cycles_delta = 0;
    __u64 instructions_delta = 0;
    __u64 llc_misses_delta = 0;
    __u64 cache_references_delta = 0;
    __u64 time_delta_ns = 0;
    
    int err = bpf_perf_event_read_value(&cycles, BPF_F_CURRENT_CPU, &cycles_val, sizeof(cycles_val));
    if (err == 0) {
        cycles_delta = compute_delta(cycles_val.counter, prev->cycles);
        prev->cycles = cycles_val.counter;
    }
    
    err = bpf_perf_event_read_value(&instructions, BPF_F_CURRENT_CPU, &instructions_val, sizeof(instructions_val));
    if (err == 0) {
        instructions_delta = compute_delta(instructions_val.counter, prev->instructions);
        prev->instructions = instructions_val.counter;
    }
    
    err = bpf_perf_event_read_value(&llc_misses, BPF_F_CURRENT_CPU, &llc_misses_val, sizeof(llc_misses_val));
    if (err == 0) {
        llc_misses_delta = compute_delta(llc_misses_val.counter, prev->llc_misses);
        prev->llc_misses = llc_misses_val.counter;
    }
    
    err = bpf_perf_event_read_value(&cache_references, BPF_F_CURRENT_CPU, &cache_references_val, sizeof(cache_references_val));
    if (err == 0) {
        cache_references_delta = compute_delta(cache_references_val.counter, prev->cache_references);
        prev->cache_references = cache_references_val.counter;
    }
    
    // Compute time delta and update timestamp
    // If prev->timestamp is 0, this is the first event, don't emit it
    {
        __u64 now = bpf_ktime_get_ns();

        if (prev->timestamp != 0) {
            time_delta_ns = compute_delta(now, prev->timestamp);
            send_perf_measurement(ctx, pid, cycles_delta, instructions_delta, 
                                llc_misses_delta, cache_references_delta, time_delta_ns, now,
                                is_context_switch, next_tgid);
        }
        prev->timestamp = now;
    }

    return 0;
}

SEC("tp_btf/sched_switch")
int handle_sched_switch(u64 *ctx)
{
    struct task_struct *prev = (struct task_struct *)ctx[1];
    struct task_struct *next = (struct task_struct *)ctx[2];
    
    // Get current task (simpler approach as requested)
    struct task_struct *current_task = bpf_get_current_task_btf();
    
    // Get next task TGID for context switch events
    __u32 next_tgid = 0;
    if (next) {
        next_tgid = next->tgid;
    }
    
    // Check and send metadata if needed
    check_and_send_metadata(ctx, current_task);
    
    // Collect and send performance measurements (context switch event)
    collect_and_send_perf_measurements(ctx, current_task, 1, next_tgid);
    
    return 0;
}

SEC("tracepoint/sched/sched_process_exit")
int handle_process_exit(struct trace_event_raw_sched_process_template *ctx)
{
    struct task_struct *task = bpf_get_current_task_btf();

    // Skip if not a group leader or kernel thread
    if (!task || task != task->group_leader)
        return 0;

    // Add task to the list of tasks to be reported
    __u32 pid = task->pid;
    bpf_map_update_elem(&exited_leaders, &pid, &LEADER_PRESENT, BPF_ANY);

    return 0;
}


SEC("tracepoint/sched/sched_process_free")
int handle_process_free(struct trace_event_raw_sched_process_template *ctx)
{
    __u32 pid = ctx->pid;
    
    // Check if this is a registered group leader that needs reporting
    __u8 *present = bpf_map_lookup_elem(&exited_leaders, &pid);
    if (!present) {
        // Not an exited group leader we care about
        return 0;
    }
    
    // Remove from the tracking map
    bpf_map_delete_elem(&exited_leaders, &pid);
    
    // Report task free event
    send_task_free(ctx, pid);
    
    return 0;
}

// Send timer finished processing event to userspace
static __always_inline int send_timer_finished_processing(void *ctx)
{
    struct timer_finished_processing_msg msg = {};
    
    msg.header.type = MSG_TYPE_TIMER_FINISHED_PROCESSING;
    msg.header.timestamp = bpf_ktime_get_ns();
    
    // Skip the size field (first 4 bytes) when sending
    return bpf_perf_event_output(ctx, &events, BPF_F_CURRENT_CPU, 
                                ((void*)&msg) + sizeof(__u32), 
                                sizeof(msg) - sizeof(__u32));
}

void sync_timer_callback(__u32 expected_cpu)
{
    // Set the timer fired flag for this CPU
    __u32 key = 0;
    __u32 actual_cpu = bpf_get_smp_processor_id();
    
    struct timer_fire_info info = {};
    info.expected_cpu = expected_cpu;
    
    // Check if timer fired on the wrong CPU
    if (actual_cpu != expected_cpu) {
        info.state = TIMER_MIGRATION_DETECTED;
    } else {
        info.state = TIMER_FIRED;
    }
    
    bpf_map_update_elem(&timer_fired, &key, &info, BPF_ANY);
}

/* HR Timer expire exit tracepoint handler */
SEC("tracepoint/timer/hrtimer_expire_exit")
int handle_hrtimer_expire_exit(void *ctx)
{
    __u32 key = 0;
    
    // Check if our timer fired on this CPU
    struct timer_fire_info *info = bpf_map_lookup_elem(&timer_fired, &key);
    if (!info || info->state == TIMER_RESET) {
        // Not our timer or no timer fired
        return 0;
    }
    
    // Handle timer migration detection
    if (info->state == TIMER_MIGRATION_DETECTED) {
        // Send migration alert to userspace
        __u32 cpu = bpf_get_smp_processor_id();
        send_timer_migration_alert(ctx, info->expected_cpu, cpu);
        goto reset_and_exit;
    }
    
    // Normal timer processing (no migration detected)
    // Get current task
    struct task_struct *current_task = bpf_get_current_task_btf();
    
    // Check and send metadata if needed
    check_and_send_metadata(ctx, current_task);

    // Collect and send performance measurements before sending timer finished message (timer event)
    collect_and_send_perf_measurements(ctx, current_task, 0, 0);
    
    // Send the timer processing finished message
    send_timer_finished_processing(ctx);
    
reset_and_exit:
    // Reset the flag
    struct timer_fire_info reset_info = {};
    reset_info.state = TIMER_RESET;
    reset_info.expected_cpu = info->expected_cpu;
    bpf_map_update_elem(&timer_fired, &key, &reset_info, BPF_ANY);
    
    return 0;
}

DEFINE_SYNC_TIMER(collect, sync_timer_callback);

char LICENSE[] SEC("license") = "GPL"; 