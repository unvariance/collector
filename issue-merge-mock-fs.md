Title: Merge the two mock FS implementations into a single reusable utility

Summary

- We currently have two very similar in-memory filesystem mocks used in tests:
  - `crates/resctrl/src/test_utils.rs::mock_fs::MockFs`
  - `crates/nri-resctrl-plugin/src/lib.rs` test module’s `TestFs`
- The `nri-resctrl-plugin` crate accidentally re-implemented a mock FS which overlaps with and partially diverges from the one in `resctrl`.
- Goal: consolidate to a single, feature-complete mock FS in `crates/resctrl/src/test_utils.rs` and use it from both crates.

What `nri-resctrl-plugin`’s TestFs has that resctrl’s MockFs lacks

- mkdir call counting for assertions
  - `TestFs` tracks create_dir invocations per path via `mkdir_calls: HashMap<PathBuf, usize>` and exposes `mkdir_count(&Path) -> usize` used in tests to verify we attempt creation exactly once per retry pass.
- Convenience toggles to clear simulated conditions
  - `clear_nospace_dir(&Path)` and `clear_missing_pid(i32)` helpers (resctrl’s MockFs exposes setters but not symmetric clear helpers; tests currently work around by re-creating instances or not clearing).
- Auto-create `tasks` file for new resctrl group directories
  - When `create_dir` is called on a directory that looks like a resctrl group (name starts with the configured prefix like `pod_`), `TestFs` auto-creates the `tasks` file beneath it. This mirrors kernel behavior and simplifies tests that immediately write PIDs.
- Premounted default in /proc/mounts (quality-of-life default)
  - `TestFs::default()` seeds `/proc/mounts` with a `resctrl /sys/fs/resctrl resctrl` line so `detect_support()` sees it mounted without extra setup in each test.

Notes on behavior differences to keep in mind

- `check_can_open_for_write` semantics:
  - RealFs requires the file to exist. `TestFs` returns Ok only if the file exists (closer to RealFs). resctrl’s MockFs currently returns Ok if either the file exists OR the parent directory exists; this can over-report writability.
>> actually it seems that resctrl's MockFs is more realistic. But not sure it matters -- are there any tests affected by this?
- Mount simulation:
  - resctrl’s MockFs implements `mount_resctrl()` and can append to `/proc/mounts` and create a mountpoint `tasks` file. `TestFs` returns `ENOSYS` for `mount_resctrl()` and relies on a premounted default.
- Permission/race simulation richness:
  - resctrl’s MockFs supports more scenarios (permission denied on files/dirs, permission denied on remove_dir, per-dir listing overrides to simulate races). `TestFs` doesn’t have these but doesn’t need them for plugin tests.

Proposal: make resctrl’s MockFs the canonical implementation and extend it

We’ll keep `MockFs` in `crates/resctrl/src/test_utils.rs` as the single mock and add small, opt-in features needed by `nri-resctrl-plugin` tests. This avoids duplicating richer behaviors already present in resctrl’s mock.

API and behavior additions to MockFs

- Add mkdir call counting
  - State: add `mkdir_calls: HashMap<PathBuf, usize>`; increment in `create_dir`.
  - API: `fn mkdir_count(&self, p: &Path) -> usize`.
- Add clear helpers for toggles
  - API: `fn clear_nospace_dir(&self, p: &Path)`; `fn clear_missing_pid(&self, pid: i32)`.
- Optional auto-creation of `tasks` for new group dirs
  - Add a configurable option on MockFs (off by default to preserve existing resctrl tests) to auto-create `tasks` when `create_dir` is called for a directory that matches a provided group prefix.
>> This seems like enabling always would have a positive effect on resctrl tests too? Which tests would be affected? List them here and we'll fix them.
  - API: `fn set_group_tasks_autocreate(&self, enabled: bool, group_prefix: impl Into<String>)`.
  - Behavior: when enabled and `create_dir(/path/to/<prefix>...)` succeeds, insert an empty `/path/to/<prefix...>/tasks` file.
- Optional premounted convenience builder
>> Again, can we enable this always? Which tests would be affected?
  - Provide a constructor that seeds `/proc/mounts` and the resctrl root directory (no behavior change to Default):
    - `fn with_premounted_resctrl() -> Self` (or `fn new_premounted_at(root: &Path)`), which writes a `resctrl <root> resctrl` line to `/proc/mounts`, ensures `<root>` exists, and creates `<root>/tasks`.
- Align `check_can_open_for_write` with RealFs when possible
>> Let's keep the resctrl implementation. Which nri-resctrl-plugin tests would be affected by this change?
  - Consider changing to return Ok only if the file exists and is not marked no-perm, to better emulate `OpenOptions::open()` behavior. If any existing resctrl tests rely on the more lenient behavior, we can gate the strictness under an opt-in flag (defaulting to strict).

Migration plan

1) Extend resctrl’s MockFs with the APIs above (behind test-only code as it is today).
2) In `nri-resctrl-plugin` tests, replace the local `TestFs` with `resctrl::test_utils::mock_fs::MockFs`.
   - Use `with_premounted_resctrl()` or manually seed `/proc/mounts` to retain the premounted behavior where needed.
   - Enable `set_group_tasks_autocreate(true, "pod_")` so group creation produces a `tasks` file automatically, preserving test simplicity.
   - Where tests assert mkdir attempts, call `mkdir_count(&group_path)`.
   - Where tests previously cleared toggles, call the new `clear_*` helpers.
3) Remove the `TestFs` from `crates/nri-resctrl-plugin/src/lib.rs` test module.
4) Run the full test suite for both crates and adjust any tests that made implicit assumptions differing from the unified behavior (e.g., `check_can_open_for_write` strictness). If needed, temporarily enable the lenient mode via a MockFs option and then follow up with a targeted fix to the affected tests.

Acceptance criteria

- No tests in `crates/resctrl` or `crates/nri-resctrl-plugin` rely on the plugin’s bespoke `TestFs`.
- All plugin tests compile and pass using `resctrl::test_utils::mock_fs::MockFs` with the added APIs.
- The unified MockFs remains test-only and does not leak into production code paths.
- Behavior parity is preserved (e.g., mkdir call count checks continue to work; `tasks` file is present for groups in tests that expect it; premounted convenience remains available for tests that used it implicitly).

Out of scope (can be follow-ups)

- Expanding permission/race simulation coverage in plugin tests to leverage resctrl MockFs features (e.g., directory permission denials, remove_dir permission denials, child dir listing overrides) — beneficial but not required for the merge.
- Re-evaluating and standardizing `check_can_open_for_write` semantics across mocks and RealFs beyond the minimal compatibility needed for current tests.

References

- Canonical target: `crates/resctrl/src/test_utils.rs`
- Duplicate to replace: `crates/nri-resctrl-plugin/src/lib.rs` (test module `TestFs`)

