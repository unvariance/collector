#pragma once

#define CLOCK_MONOTONIC 1

/* Define AF_INET constant for BPF context */
#define AF_INET 2

/* Timer initialization modes */
enum sync_timer_mode {
    SYNC_TIMER_MODE_MODERN = 0,      // CPU pinning + absolute time (kernel 6.7+)
    SYNC_TIMER_MODE_INTERMEDIATE = 1, // Absolute time only (kernel 6.4-6.6)
    SYNC_TIMER_MODE_LEGACY = 2,      // Relative time only (kernel 5.15-6.3)
};

// Dummy instance to make skeleton generation work
enum sync_timer_mode sync_timer_mode_ = 0;

/* Error codes for sync timer initialization */
enum sync_timer_init_error {
    SYNC_TIMER_SUCCESS = 0,
    SYNC_TIMER_MAP_UPDATE_FAILED = 1,
    SYNC_TIMER_MAP_LOOKUP_FAILED = 2,
    SYNC_TIMER_TIMER_INIT_FAILED = 3,
    SYNC_TIMER_TIMER_SET_CALLBACK_FAILED = 4,
    SYNC_TIMER_TIMER_START_FAILED = 5,
};

// Dummy instance to make skeleton generation work
enum sync_timer_init_error sync_timer_init_error_ = 0;

/* Common structures and helper functions */
struct sync_timer_state {
    struct bpf_timer timer;
    __u64 last_tick;
    __u64 next_expected;  // Absolute time for next tick
    __u32 expected_cpu;   // CPU ID this timer should fire on
    __u64 timer_flags;    // Pre-computed timer flags for bpf_timer_start()
    __u8 init_mode;       // Initialization mode (0=modern, 1=intermediate, 2=legacy)
    __u64 interval_ns;    // Interval for the timer
};

/* Helper function to calculate absolute difference */
static __always_inline __u64 __sync_timer_abs_diff(__u64 a, __u64 b) {
    return a > b ? a - b : b - a;
}

/* Helper function to align time to next interval */
static __always_inline __u64 __sync_timer_align_to_interval(__u64 time, __u64 interval) {
    return (time / interval) * interval;
}

/* Helper function to compute timer start parameter based on flags and expected time */
static __always_inline __u64 __sync_timer_compute_start_param(__u64 next_expected, __u64 timer_flags) {
    if (timer_flags & BPF_F_TIMER_ABS) {
        /* Absolute time mode - return the expected time directly */
        return next_expected;
    } else {
        /* Relative time mode - compute relative offset from current time */
        __u64 now = bpf_ktime_get_ns();
        if (next_expected > now) {
            return next_expected - now;
        } else {
            /* If expected time is in the past, schedule for immediate execution */
            return 1;
        }
    }
}

/* Shared timer callback implementation */
static __always_inline int __sync_timer_shared_callback(
    void *map,
    int *key,
    struct sync_timer_state *state,
    void (*callback_func)(__u32)  // Modified to pass expected CPU ID
) {
    __u64 now = bpf_ktime_get_ns();
    __u64 interval = state->interval_ns;
    __u64 expected_tick = now / interval;
    __u64 actual_tick = state->last_tick + 1;
    __u64 delta;

    /* Call the provided callback function with expected CPU ID */
    callback_func(state->expected_cpu);

    /* Check for missed ticks */
    if (expected_tick > actual_tick) {
        actual_tick = expected_tick;
    }

    /* Update tick counter */
    state->last_tick = actual_tick;

    /* Calculate timing delta */
    delta = __sync_timer_abs_diff(now, actual_tick * interval);

    /* Calculate next absolute time for timer */
    state->next_expected = __sync_timer_align_to_interval(now + interval, interval);

    /* Reschedule timer using computed start parameter */
    __u64 start_param = __sync_timer_compute_start_param(state->next_expected, state->timer_flags);
    bpf_timer_start(&state->timer, start_param, state->timer_flags);

    return 0;
}

/* Shared timer initialization implementation */
static __always_inline int __sync_timer_shared_init(
    void *timer_states_map,
    int (*timer_callback)(void *, int *, struct sync_timer_state *),
    __u8 init_mode,
    __u64 interval_ns
) {
    __u32 cpu = bpf_get_smp_processor_id();
    struct sync_timer_state *state;
    __u64 now;
    int ret;

    /* Pre-compute timer flags based on mode */
    __u64 timer_flags;
    switch (init_mode) {
        case SYNC_TIMER_MODE_MODERN:
            timer_flags = BPF_F_TIMER_ABS | BPF_F_TIMER_CPU_PIN;
            break;
        case SYNC_TIMER_MODE_INTERMEDIATE:
            timer_flags = BPF_F_TIMER_ABS;
            break;
        case SYNC_TIMER_MODE_LEGACY:
        default:
            timer_flags = 0;  // No flags for legacy mode
            break;
    }

    /* Check if timer state already exists for this CPU and remove it to start fresh */
    state = bpf_map_lookup_elem(timer_states_map, &cpu);
    if (state) {
        /* Cancel any existing timer before removing the state */
        bpf_timer_cancel(&state->timer);
        /* Remove existing state to ensure fresh initialization */
        bpf_map_delete_elem(timer_states_map, &cpu);
    }

    /* Create fresh timer state for this CPU */
    struct sync_timer_state new_state = {};
    new_state.expected_cpu = cpu;  // Store the CPU this timer should fire on
    new_state.timer_flags = timer_flags;
    new_state.init_mode = init_mode;
    ret = bpf_map_update_elem(timer_states_map, &cpu, &new_state, BPF_ANY);
    if (ret < 0) {
        return SYNC_TIMER_MAP_UPDATE_FAILED;
    }
    
    /* Get the newly created state */
    state = bpf_map_lookup_elem(timer_states_map, &cpu);
    if (!state) {
        return SYNC_TIMER_MAP_LOOKUP_FAILED;
    }

    /* Initialize timer */
    now = bpf_ktime_get_ns();
    state->interval_ns = interval_ns;
    state->last_tick = now / interval_ns;
    state->next_expected = __sync_timer_align_to_interval(now + interval_ns, interval_ns);
    
    ret = bpf_timer_init(&state->timer, timer_states_map, CLOCK_MONOTONIC);
    if (ret < 0) {
        return SYNC_TIMER_TIMER_INIT_FAILED;
    }
    
    ret = bpf_timer_set_callback(&state->timer, timer_callback);
    if (ret < 0) {
        return SYNC_TIMER_TIMER_SET_CALLBACK_FAILED;
    }
    
    /* Start timer using computed start parameter */
    __u64 start_param = __sync_timer_compute_start_param(state->next_expected, timer_flags);
    ret = bpf_timer_start(&state->timer, start_param, timer_flags);
    if (ret < 0) {
        return SYNC_TIMER_TIMER_START_FAILED;
    }

    return SYNC_TIMER_SUCCESS;
}

/* Macro to define a complete sync timer implementation */
#define DEFINE_SYNC_TIMER(timer_name, callback_func, interval_ns_param) \
\
/* Timer state map */ \
struct { \
    __uint(type, BPF_MAP_TYPE_HASH); \
    __uint(max_entries, 1024); \
    __type(key, __u32); \
    __type(value, struct sync_timer_state); \
} sync_timer_states_##timer_name SEC(".maps"); \
\
/* Timer callback function */ \
static int sync_timer_callback_##timer_name(void *map, int *key, struct sync_timer_state *state) \
{ \
    return __sync_timer_shared_callback(map, key, state, callback_func); \
} \
\
/* Unified timer initialization function with mode parameter */ \
SEC("syscall") \
int sync_timer_init_##timer_name(struct bpf_sock_addr *ctx) \
{ \
    /* Extract mode from context_in if available, default to modern mode */ \
    __u8 init_mode = SYNC_TIMER_MODE_MODERN; \
    if (ctx && ctx->user_family == AF_INET) { \
        /* Use the first byte of user_ip4 as the mode parameter */ \
        init_mode = (__u8)(ctx->user_ip4 & 0xFF); \
    } \
    return __sync_timer_shared_init(&sync_timer_states_##timer_name, \
                                   sync_timer_callback_##timer_name, \
                                   init_mode, \
                                   (interval_ns_param)); \
} 
