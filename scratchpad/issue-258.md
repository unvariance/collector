# Sub-Issue 06: Startup cleanup of stale resctrl groups

## Summary
Implement a safe cleanup sweep that removes only our previously-created resctrl groups using a configurable prefix, gated by `cleanup_on_start` (default true).

## Scope
- In `crates/resctrl` add:
  - `cleanup_all(prefix: &str) -> Result<usize>` that scans `/sys/fs/resctrl` and removes directories matching naming convention.
- In `ResctrlPlugin`:
  - If `cleanup_on_start` is true, run cleanup before processing `synchronize()`.
  - Log removed count; do not emit events for cleanup-only actions.

## Out of Scope
- Detecting/cleaning groups created by other systems (strictly prefix-based).

## Deliverables / Acceptance
- Correctly identifies and removes only groups with the configured prefix.
- Unit tests: verify match/mismatch behavior via mock FS provider.
- Document naming convention and cleanup safety considerations.

## Implementation Notes
- Use strict prefix match and simple sanitization to avoid accidental deletion.
- Consider a dry-run option or debug logging of candidates.

## Dependencies
- Sub-Issue 01 (crate), Sub-Issue 03 (plugin skeleton).

## Testing
- Mock FS directory listing of resctrl root; ensure only expected entries removed.

## Risks
- Accidental deletion mitigated by strict prefix filter and tests.


