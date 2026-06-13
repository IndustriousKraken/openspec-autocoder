## ADDED Requirements

### Requirement: On-demand audit-run queue persists across daemon restart
The per-repo on-demand audit-run queue (`pending_audit_runs`) SHALL be persisted to durable storage so that a daemon restart between an operator's enqueue acknowledgement and the audit's run does not lose the queued request. The queue SHALL be written on every mutation — both enqueue AND the post-run prune — using an atomic write (tempfile + rename), AND SHALL be loaded into memory when a repo's polling task is spawned. A persisted entry whose repo is no longer configured SHALL be reconciled away at load. Persistence SHALL be best-effort: a read or write failure is logged AND does NOT abort the run, with the in-memory queue remaining authoritative for the live process.

#### Scenario: A restart between enqueue and run preserves the queued audit
- **WHEN** an audit is queued AND the daemon restarts before the audit has run
- **THEN** the queued audit is restored from durable storage when the repo's polling task is spawned
- **AND** it runs on a subsequent iteration

#### Scenario: A persisted entry is removed once its audit runs
- **WHEN** a queued audit runs AND is pruned from the in-memory queue
- **THEN** the durable copy is updated to no longer contain that entry
- **AND** a later restart does NOT re-run the already-run audit

#### Scenario: An orphaned persisted entry is reconciled at load
- **WHEN** the daemon starts AND a persisted queue file names a repository that is no longer in the configured set
- **THEN** the orphaned entry is dropped at load rather than resurrected as work

#### Scenario: A corrupt queue file degrades gracefully
- **WHEN** the persisted queue file cannot be read OR parsed at load
- **THEN** the failure is logged AND the repo starts with an empty in-memory queue
- **AND** the daemon does not panic or abort startup
