# How resctl-demo Controls System Resources

`resctl-demo` is a project from Meta (formerly Facebook) that demonstrates advanced resource control techniques to improve system reliability and utilization. It doesn't just provide benchmarks; it offers a complete, opinionated system for managing resources on a Linux host. This document explains the core principles and mechanisms it uses to prevent processes from hogging resources, with references to its internal documentation for further reading.

## Core Philosophy: Software-Based Control with cgroup2

The fundamental mechanism behind `resctl-demo` is the **Linux Control Group v2 (cgroup2)** interface. Instead of relying on specialized hardware features for resource throttling, `resctl-demo` uses kernel-level, software-based controls to partition and prioritize system resources.

The system is divided into hierarchical "slices," which are essentially cgroups. The primary slices used in the demo are:

-   `workload.slice`: For the main, latency-sensitive applications.
-   `system.slice`: For system services, maintenance tasks, and other non-critical background work.
-   `hostcritical.slice`: For essential system daemons (like `sshd`, `oomd`) that must remain responsive.
-   `sideload.slice`: For opportunistic, low-priority workloads that run only when resources are free.

By placing processes into these different slices, the kernel can enforce different resource policies for each, ensuring that a misbehaving process in `system.slice` (like a memory leak) cannot cripple the main application in `workload.slice`.

> **Reference:** The overall philosophy is detailed in `resctl-demo/resctl-demo/src/doc/comp.cgroup.rd`.

---

## Does it use explicit hardware throttling like Intel RDT?

**No.** The documentation and design of `resctl-demo` and `resctl-bench` focus exclusively on using the Linux kernel's `cgroup2` controllers. There is no mention of using hardware-specific features like Intel RDT (which includes Cache Allocation Technology - CAT, and Memory Bandwidth Allocation - MBA).

The project's approach is to use portable, software-defined mechanisms that are part of the upstream Linux kernel, making the solution more general and not tied to specific CPU vendors or models.

## Does it influence scheduling?

**Yes, absolutely.** A primary function of `resctl-demo`'s configuration is to directly influence the kernel's CPU and I/O schedulers.

-   **CPU Scheduling:** It uses the `cpu.weight` controller to assign a proportional share of CPU time to each cgroup slice. This is a work-conserving mechanism, meaning if the CPU is not contended, any process can use it. However, during contention, the scheduler will prioritize slices with higher weights (`workload` and `hostcritical`) over those with lower weights (`system`).
-   **I/O Scheduling:** It uses the `io.cost` controller, which provides a sophisticated weight-based model for I/O scheduling. This model understands the different costs of sequential vs. random I/O and helps prevent a low-priority process from saturating the storage device and stalling high-priority work.

---

## Resource Control Mechanisms in Detail

### Memory Control

The goal of memory control in `resctl-demo` is to provide robust, forgiving protection for important workloads rather than imposing strict, brittle limits on non-critical ones.

-   **Mechanism:** The primary tool is the `memory.low` cgroup controller. This sets a "best-effort" protection for a cgroup. If a cgroup is using less memory than its `memory.low` setting, its memory is protected from reclaim. Memory used *above* this amount is reclaimable. This provides a soft, work-conserving protection.
-   **Configuration:**
    -   `workload.slice` is protected with `memory.low` set to `75%` of system memory.
    -   `hostcritical.slice` is protected with `memory.min`, a stricter guarantee, for `768MB`.
    -   `system.slice` and `sideload.slice` have no protection and are the first candidates for memory reclaim under pressure.

This setup ensures the main workload is shielded from memory pressure caused by background tasks, which will be throttled or killed by `oomd` if they consume too much.

> **Reference:** Detailed explanation can be found in `resctl-demo/resctl-demo/src/doc/comp.cgroup.mem.rd`.

### I/O Control

I/O control is critical because memory pressure often translates into I/O pressure (due to swapping and paging).

-   **Mechanism:** The `io.cost` controller provides weight-based proportional I/O distribution. It uses a cost model to account for the difference between sequential and random I/O, providing more accurate control than simple IOPS or bandwidth limits. A key part of this is "owning the queue"—pacing I/Os to prevent overwhelming the storage device's hardware queue, which is crucial for maintaining low latency.
-   **Filesystem Support:** The documentation notes that effective I/O control requires filesystem support to avoid priority inversions (where a low-priority I/O operation blocks a high-priority one). `btrfs` is highlighted as having the necessary support.
-   **Configuration:** Weights are assigned to give the `workload` slice the vast majority of I/O resources during contention.
    -   `workload` : `hostcritical` : `system` : `sideload` = `500` : `100` : `50` : `1`

> **Reference:** Detailed explanation can be found in `resctl-demo/resctl-demo/src/doc/comp.cgroup.io.rd`.

### CPU Control

-   **Mechanism:** The `cpu.weight` controller is used to distribute CPU time proportionally. Like the other controllers, it is work-conserving.
-   **Configuration:** Weights are set to prioritize the main workload and critical services.
    -   `workload` : `hostcritical` : `system` : `sideload` = `100` : `10` : `10` : `1`
-   **Latency Nuance:** The documentation makes an important point that while `cpu.weight` can protect the *throughput* of a latency-sensitive application, it often cannot fully protect its *latency*. When a system's CPUs are saturated, scheduling and cache effects can increase latency for all processes, even if the application is getting its proportional share of CPU time.

> **Reference:** Detailed explanation can be found in `resctl-demo/resctl-demo/src/doc/comp.cgroup.cpu.rd`. 