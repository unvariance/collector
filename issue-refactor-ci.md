## CI Refactor: Consolidate Build Environment, Split Fast vs Heavy Checks, and Reusable Workflows

### Summary

Our CI has grown into many workflows that duplicate setup, run heavy steps by default, and make it hard to quickly see signal on a change. This issue proposes:

- Single multi-stage Containerfile with cargo-chef to standardize build + dev tooling for all Rust components (collector and nri-init), plus a builder stage to run cargo fmt/clippy/tests inside a container.
- A two-tier CI model: fast checks (minutes) on GitHub-hosted runners vs. heavy checks (longer, pricey) on EC2 self-hosted runners.
- Reusable workflows for integration tests, with artifacts passed from a single build stage and optional parameters to control test level (machine size + expected duration).
- Unified caching strategy: container layer cache via cargo-chef + GHA cache for target/registry when applicable, minimizing repeated Rust/tooling installs.

This will reduce duplicated environment setup, speed up feedback on PRs, and keep heavyweight validation available when it’s most valuable (merges to main or manual dispatch).

---

### Inventory of Current Workflows (tests and runners)

Below is a quick inventory of all workflows in `.github/workflows/` with focus on unit vs. integration tests and runner requirements.

1) rust-lints.yaml
- Unit: none
- Fast checks: cargo fmt --check; cargo clippy (workspace)
- Runners: GitHub-hosted `ubuntu-latest`
- Notes: Installs Rust + apt deps + uses Swatinem/rust-cache. Good candidate to run inside the builder container.

2) test-nri-integration.yaml
- Unit: none
- Integration: builds and runs `crates/nri` integration test against a k3s setup (verifies NRI socket, etc.)
- Runners: GitHub-hosted `ubuntu-latest`
- Notes: Sets up k3s and runs a Rust test binary with sudo. Fast-tier compatible.

3) test-nri-init.yaml
- Unit:
  - cargo test -p nri-init --lib
- Integration (GH-hosted):
  - integration_sim (no system services)
  - integration_real (k3s/containerd tests marked `--ignored`)
  - integration_matrix: KIND and k3s scenarios, both running nri-init as binary or container, including restart paths
- Helm validation:
  - helm lint (default + multiple values files)
  - helm template rendering checks across value variants
  - helm install on KIND across k8s versions (1.28-1.31) with nri-init image from the build
- Build: uses reusable `build-component-artifacts.yaml` to build `nri-init` image and binary
- Runners: GitHub-hosted `ubuntu-latest`
- Notes: Many fast-to-medium checks; strong candidate for the fast tier; image/binary artifacts are already centralized.

4) test-ebpf-collector.yaml
- Build: uses reusable `build-component-artifacts.yaml` to build collector image + binary
- Unit-ish/assumption test (GH-hosted):
  - cargo test for BPF cgroup inode assumptions on `ubuntu-latest`
- Heavy integration (Self-hosted EC2, default `c7i.metal-24xl`):
  - test-ebpf: run collector binary with sudo, verify local parquet
  - test-s3-integration: IRSA and Access Key write/read + parquet validation
  - test-multi-kernel: kernel matrix via Little VM Helper (QEMU on the EC2 runner), validates success/failure by kernel version
  - nri-enrichment-e2e: k3s + verify enrichment fields in output parquet
- Runners: Mixed; GH-hosted for assumptions; self-hosted EC2 for hardware/privileged tests
- Notes: Heavy test suite; should live in heavy tier with parameterizable machine type and duration.

5) test-resctrl.yaml
- Unit (GH-hosted):
  - cargo test -p resctrl
  - cargo test -p nri-resctrl-plugin
  - build integration test binaries (no-run) as artifacts
- Integration (Self-hosted EC2, default `m7i.metal-24xl`):
  - resctrl-e2e-smoke: runs `RESCTRL_E2E=1` smoke test binary (hardware)
  - nri-resctrl-plugin-e2e: k3s-based tests using downloaded prebuilt binary
- Runners: GH-hosted for unit builds; self-hosted EC2 for e2e
- Notes: Hardware-bound. Candidate for heavy tier; unit pieces can move into the fast tier container.

6) test-helm-chart.yaml
- Integration (Self-hosted EC2, default `m7i.xlarge`):
  - Set up k3s; helm install/uninstall collector chart in aggregated and trace modes
  - S3 write/read validation; parquet schema and CSV inspection (in follow-up GH-hosted job)
- Runners: self-hosted EC2 for cluster + S3; GH-hosted for artifact verification
- Notes: Medium-heavy; parameterizable machine size; leverages artifacts and GHCR images.

7) benchmark.yaml
- Workload + perf (Self-hosted EC2, default `m7i.metal-24xl`):
  - k3s + OpenTelemetry demo; run collector (trace + aggregated); perf record, pidstat; S3 parquet validation; artifacts
 - generate-visualizations: separate GH-hosted job in a `rocker/tidyverse` container
- Runners: self-hosted EC2 (workload), GH-hosted (visualization)
- Notes: Long-running and expensive. Heavy tier only; manual trigger only.

8) benchmark-sync-timers.yml
- Build (GH-hosted) Go benchmark, then run on EC2 (default `m7i.xlarge`), optionally visualize
- Runners: GH-hosted + self-hosted EC2
- Notes: Medium-heavy; heavy tier/optional.

9) get-resctrl-and-perf-info.yaml
- Diagnostics only: lists perf/resctrl capabilities and sysfs layout
- Runners: self-hosted EC2 (default `m7i.metal-24xl`)
- Notes: Heavy tier utility.

10) resctrl-demo.yaml
- Demo only: set up resctrl groups and stress workloads
- Runners: self-hosted EC2
- Notes: Heavy tier utility/demo.

11) build-collector.yaml
- Build only (no tests): multi-arch images for `collector` and `nri-init` using current separate Dockerfiles; manifest push
- Runners: GH-hosted (amd64 + arm64)
- Notes: Runs on `main` only; will be updated to use unified multi-stage Containerfile targets.

12) build-component-artifacts.yaml (reusable)
- Build only (no tests): builds `collector` or `nri-init`, can upload image tar and binary
- Runners: configurable
- Notes: Good centralization; keep and switch to unified Containerfile with targets.

13) publish-helm-chart.yaml; tag-collector-latest.yaml; make-docs-to-gh-pages.yml; publish-benchmark-to-gh.yaml
- Utility (no tests): publishing/docs/tagging
- Runners: GH-hosted
- Notes: Not in scope for test split, but benefit from artifact/image reuse.

---

### Problems Observed

- Signal-to-noise: Multiple workflows must be inspected to understand PR health; some heavy jobs start even when faster feedback is available.
- Duplicate setup: Many workflows install Rust tools, apt packages, and create similar caches independently.
- Build fragmentation: Separate Dockerfiles; not all tests run inside a consistent container/tooling environment.
- Cost/latency: Heavy self-hosted jobs (e.g., eBPF, resctrl, benchmarks) run too often or without easy knobs.

---

### Proposal

1) Single multi-stage Containerfile with cargo-chef
- Merge `Dockerfile.collector` and `Dockerfile.nri-init` into one multi-stage file `Containerfile`.
  - Base stage: Debian/Bookworm + Rust + clang/libelf/pkg-config + cargo-chef + rustfmt + clippy (available for `cargo fmt`, `cargo clippy`)
  - Planner stage: `cargo chef prepare` to produce `recipe.json`
  - Builder stage: `cargo chef cook --release` to cache dependencies
  - Targets:
    - `collector`: builds `/usr/local/bin/collector` (release)
    - `nri-init`: builds `/usr/local/bin/nri-init` (release), replacing the Alpine build with the same toolchain base for consistency
    - `builder`: returns the builder stage with build tools and cached dependencies; can be used to run `cargo fmt`, `cargo clippy`, and `cargo test` inside the container
- Rationale: Unify environment and caching; reuse the same dependency layers and APT installs across components and checks.

2) Fast vs. Heavy tiers
- Fast (GitHub-hosted):
  - Devtools container runs: fmt, clippy, unit tests across crates
  - Helm lint/template checks
  - NRI integration on GH-hosted (k3s/KIND where feasible)
  - BPF cgroup inode assumption test (as today)
- Heavy (Self-hosted EC2):
  - eBPF collector run + S3 tests + kernel matrix (LVH)
  - resctrl smoke + nri-resctrl-plugin e2e (k3s)
  - Helm chart end-to-end on EC2
  - Benchmarks and system diagnostics (perf/resctrl)

3) Reusable workflows per integration area
- Convert integration areas (collector eBPF, resctrl, helm chart, NRI) into callable `workflow_call` workflows that:
  - Take inputs for: artifact names, image/tag, test level (machine type), and duration class (short/long)
  - Consume the artifacts built once by a single upstream “Build Artifacts” job
  - Optionally provision a self-hosted runner via `.github/actions/aws-runner`
- Main “CI Orchestrator” workflow (sections; each may include multiple GitHub Actions jobs):
  - Build: Build builder/container + component artifacts (collector + nri-init) and publish artifacts
  - Fast checks: fmt/clippy/units/helm-lint in parallel, in container
  - GH-hosted integrations (short)
  - Heavy workflows (conditional), parameterized by test level

4) Test-level knobs and scheduling
- Levels (inputs/labels):
  - cheap-short: `m7i.xlarge` (or similar) for short tests without PMU/resctrl needs
  - perf-short: `c5.9xlarge` for PMU/perf counters without bare-metal requirements
  - full-short: `m7i.metal-24xl` for short smoke tests that require bare metal (e.g., resctrl smoke)
  - full-long: `m7i.metal-24xl` for bare metal and long runs
- Defaults:
  - PRs: fast tier only (no EC2/self-hosted on PRs)
  - Pushes (org branches): fast tier + cheap-short
  - Main merges (upstream): full-long
 - Each reusable test workflow declares its required level and duration; the orchestrator only invokes those matching the selected level.

5) Caching and artifacts
- Prefer `docker/build-push-action` with `cache-from/to: gha` and cargo-chef to avoid repeated dependency builds.
- Run Rust fmt/clippy/tests in the builder container, so repeated apt/rust installs disappear from Actions steps.
- Keep `Swatinem/rust-cache@v2` only for GH-hosted steps that still run native (if any remain) or for non-containerized jobs.
- Build collector/nri-init once; share binaries and images via `actions/upload/download-artifact` across downstream jobs.

6) Required checks and usability
- Mark fast tier jobs as required checks for PRs to give a quick, clear signal.
- Heavy tier surfaces consolidated summaries (e.g., one job per area) that link to their reusable workflows.
- Keep per-area artifacts (parquet samples, logs) attached to their jobs.

---

### Migration Plan (phased)

Phase 1: Build unification
- [ ] Create `Containerfile` with multi-stage cargo-chef base and targets `collector`, `nri-init`, and `builder`.
 - [ ] Update `build-component-artifacts.yaml` to use unified Containerfile targets.
- [ ] Update `build-collector.yaml` to reference targets and keep multi-arch manifest logic.

Phase 2: Fast checks inside container
- [ ] Add a new `ci-fast.yaml` that uses the `builder` container to run fmt, clippy, and unit tests (workspace-wide).
- [ ] Move helm lint/template checks here as a job.
- [ ] Make these jobs the required checks on PRs.

Phase 3: Reusable integrations
- [ ] Extract eBPF collector tests into `reusable/test-collector.yaml` (workflow_call), parameterized by level and artifact names.
- [ ] Extract resctrl tests into `reusable/test-resctrl.yaml` (workflow_call), parameterized similarly.
- [ ] Extract helm e2e into `reusable/test-helm.yaml` (workflow_call).
- [ ] Extract NRI integration matrices into `reusable/test-nri.yaml` (workflow_call).

Phase 4: Orchestrator + policy
- [ ] Add `ci-orchestrator.yaml` that builds artifacts then calls the reusable integrations based on:
  - event (PR vs push vs main merge)
  - user input (workflow_dispatch) for test level and duration policy
- [ ] Update branch protection to require fast checks; keep heavy optional/conditional.

 Phase 5: Cleanup + docs
- [ ] Retire redundant setup in existing workflows.
- [ ] Document knobs (levels/durations) and when heavy tests run.

---

### Acceptance Criteria

- Single unified Containerfile with `collector`, `nri-init`, and `builder` targets using cargo-chef.
- A fast-tier workflow runs fmt, clippy, unit tests, and helm lint/template on GitHub-hosted runners in under ~2–3 minutes on typical PRs.
- A single build job produces collector/nri-init artifacts (binary + image) for reuse in downstream jobs.
- Reusable workflows exist for collector eBPF, resctrl e2e, helm e2e, and NRI e2e; the main orchestrator invokes them conditionally by level.
- Heavy tests do not run on PRs by default; merges to `main` in the upstream repo run full-long.
- Overall, duplicated installs of Rust/Clang/libelf across workflows drop substantially; cache hit rates improve; self-hosted usage is controlled by clear inputs/policy.

---

### Open Questions / Follow-ups

- Test classification: Unit tests are all short; no classification needed. For integration tests, classify per test as needed.
- Machine type mapping: Confirm minimal instance types:
  - eBPF + PMU/perf counters without metal: `c5.9xlarge` is sufficient for our perf use-cases (c7i.xlarge is not).
  - resctrl: minimum `m7i.metal-24xl`.
  - Kernel-matrix (LVH): needs perf-size (e.g., `c5.9xlarge`), but KVM has not worked there yet, so we run on `m7i.metal-24xl`; these are long tests.
- Image publishing: Push the builder image to GHCR for reuse across jobs/runs to enable better Rust caching in test jobs.
- Security: Review secrets usage in reusable workflows; ensure principle of least privilege on self-hosted.

---

### Appendix: Mapping Tests → Runner Tier (initial)

- Fast (GH-hosted):
  - rust-lints (fmt/clippy)
  - Unit tests for `resctrl`, `nri-resctrl-plugin`, `nri-init` (and other crates)
  - Helm lint/template checks; KIND/k3s smoke installs where workable
  - BPF cgroup inode assumption test
  - NRI integration on GH-hosted k3s (current `test-nri-integration.yaml`)

- Heavy (Self-hosted EC2):
  - Collector eBPF run + S3 integration + kernel-matrix (LVH)
  - resctrl smoke and nri-resctrl-plugin e2e (k3s)
  - Helm chart e2e on EC2 with S3 verification
  - Benchmarks and perf/resctrl diagnostics
