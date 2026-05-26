## ADDED Requirements

### Requirement: Audit posts a chatops notification when it creates a queue-bound proposal
Every LLM-driven audit (`architecture_consultative`, `drift_audit`, `missing_tests_audit`, `security_bug_audit`) SHALL post a chatops notification immediately after `openspec validate <slug> --strict` passes for its just-written proposal AND before the audit function returns to the scheduler. The notification names the audit type, the change slug, and a one-line excerpt of the proposal's `## Why` section, so operators have clear provenance when the next polling iteration begins implementing the change. The notification fires regardless of the audit's `notify_on_clean` setting, since it signals "something was found" rather than "nothing was found." The pure-data `architecture_brightline` audit, which does not generate LLM proposals, is unaffected.

#### Scenario: Validated proposal fires the notification on first attempt
- **WHEN** an LLM-driven audit's proposal passes `openspec validate <slug> --strict` on the first attempt (`retries_used == 0`)
- **THEN** the audit posts exactly one chatops notification whose text matches `🔍 <repo_url>: <audit_type> created proposal \`<change_slug>\` — <why_excerpt>`
- **AND** the notification text does NOT contain a parenthetical about retries

#### Scenario: Validated proposal after retry includes the retry-count parenthetical
- **WHEN** an LLM-driven audit's proposal passes validation after one or more retries (`retries_used > 0`)
- **THEN** the notification text appends ` (validated on retry <retries_used> of <max_validation_retries>)`

#### Scenario: ValidationExhausted does NOT fire the proposal-created notification
- **WHEN** an LLM-driven audit's proposal fails validation through every retry and the audit returns `ValidationExhausted`
- **THEN** the `🔍 created proposal` notification SHALL NOT fire
- **AND** the existing `❌ <audit-type> produced an invalid proposal` notification (from `a01-audit-proposal-self-validation`) fires instead

#### Scenario: notify_on_clean=false does not suppress this notification
- **WHEN** an LLM-driven audit configured with `notify_on_clean: false` produces a valid proposal
- **THEN** the `🔍 created proposal` notification still fires
- **AND** the existing `notify_on_clean=false` semantics still suppress only the empty-findings success message

#### Scenario: architecture_brightline produces no proposal-created notification
- **WHEN** the `architecture_brightline` audit runs to completion AND produces any number of findings
- **THEN** no `🔍 created proposal` notification fires from this audit
- **AND** the audit's existing notification behaviour (if any) is unchanged

#### Scenario: chatops backend absent does not affect audit outcome
- **WHEN** the daemon has no chatops backend configured AND an LLM-driven audit produces a valid proposal
- **THEN** the audit returns its `Reported` outcome normally
- **AND** the missing notification does NOT affect the proposal commit, the queue insertion, or the iteration's overall success

#### Scenario: chatops post_notification failure does not affect audit outcome
- **WHEN** the chatops backend is configured AND `post_notification` returns Err during the `🔍` notification post
- **THEN** the failure is logged at WARN
- **AND** the audit's `Reported` outcome is unaffected
- **AND** the proposal commit proceeds normally
