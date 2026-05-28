## ADDED Requirements

### Requirement: OPERATIONS.md and CHATOPS.md document the transient vs. permanent classification
`docs/OPERATIONS.md`'s workspace-recovery sections SHALL include a paragraph describing the mid-iteration classification (transient retries; permanent skips). `docs/CHATOPS.md`'s chatops-alert text examples SHALL show the new ` (transient; retrying)` AND ` (permanent; skipped until daemon restart) — operator inspection required` suffixes.

#### Scenario: OPERATIONS.md names the classification rule
- **WHEN** an operator reads `docs/OPERATIONS.md`'s workspace-recovery sections
- **THEN** a paragraph names the mid-iteration classification AND enumerates the patterns that classify as transient (network, transport, auth blip) vs. permanent (config errors, irrecoverable state)
- **AND** the paragraph notes that startup-time recovery is unchanged (still skip-for-lifetime for any failure)
- **AND** the paragraph cross-links to the chatops-alert section for the visible suffix examples

#### Scenario: CHATOPS.md alert examples show the new suffixes
- **WHEN** an operator reads `docs/CHATOPS.md`'s `Throttled failure alerts` section
- **THEN** the example alert text includes a transient case with the ` (transient; retrying)` suffix
- **AND** the example includes a permanent case with the ` (permanent; skipped until daemon restart) — operator inspection required` suffix
- **AND** a one-line note explains the operator action: transient → wait; permanent → SSH and investigate
