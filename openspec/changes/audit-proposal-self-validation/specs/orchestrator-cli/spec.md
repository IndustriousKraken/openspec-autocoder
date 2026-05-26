## ADDED Requirements

### Requirement: LLM-driven audits validate their generated proposals before committing
Every LLM-driven audit (currently `architecture_consultative`, `drift_audit`, `missing_tests_audit`, `security_bug_audit`) SHALL invoke `openspec validate <slug> --strict` against its just-written `openspec/changes/<slug>/` directory before returning success. The `architecture_brightline` audit, which does not generate spec proposals via LLM, is unaffected by this requirement. When validation passes, the audit returns its existing outcome variant. When validation fails AND the configured retry budget is not exhausted, the audit SHALL re-invoke its LLM with the validation error appended to the prompt and overwrite the change directory with the new response. When validation fails AND the retry budget IS exhausted, the audit SHALL discard the change directory AND post a chatops failure notification AND return a `ValidationExhausted` outcome.

#### Scenario: Valid proposal on first attempt
- **WHEN** an LLM-driven audit writes a proposal and `openspec validate <slug> --strict` exits 0 on first invocation
- **THEN** the audit returns its existing success outcome with `retries_used == 0`
- **AND** no retry is attempted
- **AND** no chatops failure notification fires

#### Scenario: Validation passes after one retry
- **WHEN** an LLM-driven audit writes an invalid proposal on attempt 0 AND `audits.max_validation_retries` is 1 AND the LLM produces a valid proposal on attempt 1 (with the prior validation error appended to its prompt)
- **THEN** the audit returns its existing success outcome with `retries_used == 1`
- **AND** the chatops notification (when `notify_on_clean=true` for this audit) includes the clause `validated on retry 1 of 1`
- **AND** the change directory at `openspec/changes/<slug>/` contains the second (valid) proposal, not the first

#### Scenario: Retry budget exhausted
- **WHEN** an LLM-driven audit writes invalid proposals on both attempt 0 and attempt 1 with `audits.max_validation_retries == 1`
- **THEN** the audit returns `AuditOutcome::ValidationExhausted { audit_type, retries_attempted: 1, final_error }`
- **AND** the `openspec/changes/<slug>/` directory does NOT exist after the call
- **AND** no commit is made to git
- **AND** a chatops `❌` notification is posted to the repo's resolved channel containing the audit type, the retry count, and a truncated excerpt of the final validation error

#### Scenario: max_validation_retries = 0 disables retries
- **WHEN** an LLM-driven audit writes an invalid proposal on the first attempt AND `audits.max_validation_retries == 0`
- **THEN** the audit returns `ValidationExhausted { retries_attempted: 0, .. }` immediately
- **AND** no second LLM call is made
- **AND** the discard-and-notify path runs the same as the exhausted case above

#### Scenario: Validation retry passes validation error in addendum
- **WHEN** the retry path invokes the LLM on attempt N > 0
- **THEN** the LLM prompt contains an addendum naming the previous attempt's openspec validation error verbatim
- **AND** the LLM's response replaces the change directory entirely (delete-and-rewrite, not patch)

### Requirement: Retry budget is operator-configurable with sensible defaults and bounds
The `audits` configuration block SHALL accept an optional `max_validation_retries: u32` field that defaults to `1` when absent. Values above `5` SHALL be clamped to `5` at config-load with a WARN log naming both the requested and clamped values. Value `0` is explicitly permitted (disables retries; first validation failure produces ValidationExhausted immediately).

#### Scenario: Default value is 1
- **WHEN** a `config.yaml` has an `audits:` block without `max_validation_retries`
- **THEN** the resolved config has `max_validation_retries == 1`

#### Scenario: Value above 5 is clamped with a WARN
- **WHEN** a `config.yaml` specifies `audits.max_validation_retries: 10`
- **THEN** the resolved config has `max_validation_retries == 5`
- **AND** the daemon emits a WARN at startup naming both the requested value (`10`) and the clamped value (`5`)

#### Scenario: Value 0 is permitted
- **WHEN** a `config.yaml` specifies `audits.max_validation_retries: 0`
- **THEN** the resolved config has `max_validation_retries == 0`
- **AND** no WARN is emitted at startup

### Requirement: Audit-state history records every attempt outcome including validation-failure metadata
Each audit type's state file SHALL maintain an `attempt_history` list of at most 20 entries, each capturing the timestamp, outcome kind, retries used, and (for ValidationExhausted outcomes) a truncated excerpt of the validation error. The list is FIFO-bounded: when a new entry would push it past 20, the oldest entry is dropped.

#### Scenario: Successful audit appends a Reported entry
- **WHEN** an LLM-driven audit returns `Reported { retries_used }`
- **THEN** the audit's state file's `attempt_history` gains one entry with `outcome_kind: "Reported"` and the matching `retries_used` value
- **AND** the entry's `error_excerpt` is `None`

#### Scenario: ValidationExhausted appends an entry with the error excerpt
- **WHEN** an LLM-driven audit returns `ValidationExhausted { retries_attempted, final_error }`
- **THEN** the audit's state file's `attempt_history` gains one entry with `outcome_kind: "ValidationExhausted"`, the matching `retries_used`, AND an `error_excerpt` containing the first 200 characters of `final_error`

#### Scenario: History is bounded at 20 entries
- **WHEN** an audit has produced 25 sequential runs
- **THEN** the audit's state file's `attempt_history` contains exactly 20 entries
- **AND** the entries are the most recent 20 (the oldest 5 have been dropped)

#### Scenario: Backwards compatibility with state files lacking attempt_history
- **WHEN** an audit reads its state file from a prior version that did not include the `attempt_history` field
- **THEN** the deserialization succeeds with `attempt_history` defaulting to an empty list
- **AND** subsequent audit runs append entries normally

### Requirement: Validation-exhausted notification fires regardless of notify_on_clean
The `❌ <audit-type> produced an invalid proposal` chatops notification SHALL fire on every `ValidationExhausted` outcome regardless of the audit's `notify_on_clean` configuration. An audit producing invalid proposals is operator-actionable feedback that the audit's prompt template or LLM is producing low-quality output; suppressing the signal would hide a real failure mode.

#### Scenario: notify_on_clean=false does not suppress validation-exhausted
- **WHEN** an audit configured with `notify_on_clean: false` returns `ValidationExhausted`
- **THEN** the chatops `❌` notification is posted
- **AND** the `notify_on_clean=false` setting does not block the notification

#### Scenario: notify_on_clean=true success-with-retry includes retry-count clause
- **WHEN** an audit configured with `notify_on_clean: true` returns `Reported { retries_used: 1 }`
- **THEN** the chatops success notification text includes the clause `validated on retry 1 of <max>`
- **AND** `<max>` is the resolved `audits.max_validation_retries` for this audit
