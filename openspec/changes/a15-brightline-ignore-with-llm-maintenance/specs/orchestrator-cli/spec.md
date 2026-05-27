## MODIFIED Requirements

### Requirement: Architecture-brightline audit
autocoder SHALL register an `architecture_brightline` audit that computes pure-code metrics (file line counts, duplicate function signatures across files) AND reports findings via `AuditOutcome::Reported(...)`. The audit SHALL declare `WritePolicy::None` AND `requires_head_change = true`. The audit SHALL load a per-workspace `.brightline-ignore` file (if present) AND apply match-suppression to duplicate-signature findings whose constituent sites are all listed in the ignore file. The audit SHALL also validate ignore entries against the current workspace state AND report stale entries via the chatops top-line (informational; the audit does NOT modify the ignore file itself given its `WritePolicy::None`).

The ignore file's YAML schema:

```yaml
ignore:
  - file: <workspace-relative path>
    function: <function or method name>
    signature_match: <substring of the function's signature line>
    reason: <one-line operator-readable explanation>
```

All four fields are required per entry. An entry with a missing field triggers a WARN log AND the entry is skipped.

Match-suppression rule: a duplicate-signature finding is suppressed in full when EVERY constituent site matches an ignore entry. A partial match (some sites match, some don't) emits the finding with only the unmatched sites listed in the body. No match → the finding is emitted in full (today's behavior).

Stale-entry rule: each ignore entry is validated against the current workspace at audit time. Validation fails when (a) the named file doesn't exist, (b) the file doesn't contain a function with the named name, OR (c) the function's signature no longer contains `signature_match`. The audit collects the stale entries AND adds a trailing clause to the chatops top-line:

```
📐 architecture_brightline on <repo>: <N> file(s) over line threshold; <M> duplicate signature(s); <K> stale ignore entries to clean up
```

The threaded body lists each stale entry's `file + function + reason` so the operator knows what to remove. The audit does NOT modify `.brightline-ignore` on disk (given `WritePolicy::None`); cleanup is operator-driven.

#### Scenario: Audit runs against pure-code metrics and reports findings
- **WHEN** the audit runs against a workspace where 2 files exceed the `file_lines_threshold` (default 800) AND 1 function signature appears in 3 files
- **THEN** the audit returns `AuditOutcome::Reported` with findings naming the oversize files AND the duplicate signature
- **AND** no `.brightline-ignore` exists OR the file's entries don't match the finding
- **AND** the chatops top-line reads `📐 architecture_brightline on <repo>: 2 file(s) over line threshold; 1 duplicate signature(s)`

#### Scenario: Ignore entry suppresses a fully-matching finding
- **WHEN** a duplicate-signature finding involves 3 sites (file1.ts, file2.ts, file3.ts with function `foo` AND signature substring `async function foo(req`)
- **AND** `.brightline-ignore` contains entries for all 3 sites with matching `file`, `function`, AND `signature_match`
- **THEN** the audit does NOT emit the finding
- **AND** the `<M> duplicate signature(s)` count in the chatops top-line does NOT include this finding

#### Scenario: Partial ignore matches still emit with the unmatched sites
- **WHEN** a duplicate-signature finding involves 3 sites
- **AND** `.brightline-ignore` contains entries for 2 of the 3 sites
- **THEN** the audit emits the finding listing only the 1 unmatched site
- **AND** the chatops body for that finding names the unmatched site AND notes that 2 sites were suppressed by ignore entries

#### Scenario: Stale ignore entries are reported but not removed
- **WHEN** `.brightline-ignore` contains an entry for `examples/site-x/auth.ts:handleAuthCallback`
- **AND** that file has been deleted from the workspace
- **THEN** the audit marks the entry as stale
- **AND** the chatops top-line gains the trailing `; <K> stale ignore entries to clean up` clause
- **AND** the threaded body lists the stale entry with its `file + function + reason`
- **AND** the audit does NOT modify `.brightline-ignore` on disk

#### Scenario: Malformed entries WARN and are skipped
- **WHEN** `.brightline-ignore` contains an entry missing the `reason` field
- **THEN** the audit logs a WARN naming the offending entry AND skips it (treats it as if it didn't exist for the run)
- **AND** other valid entries continue to apply
- **AND** the on-disk file is unchanged

#### Scenario: Missing `.brightline-ignore` behaves identically to today
- **WHEN** the workspace has no `.brightline-ignore` file
- **THEN** the audit loads an empty ignore list
- **AND** no suppression occurs
- **AND** no stale-cleanup clause appears in the chatops output
- **AND** behavior is byte-identical to pre-spec runs
