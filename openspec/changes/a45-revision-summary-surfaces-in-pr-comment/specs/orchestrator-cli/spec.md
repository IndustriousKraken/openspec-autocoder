# orchestrator-cli — delta for a45-revision-summary-surfaces-in-pr-comment

## MODIFIED Requirements

### Requirement: Revision execution updates the agent branch and posts a reply comment
On a triggering comment for an open PR, the daemon SHALL re-invoke the executor in revision mode (passing the original change material, the current PR diff, AND the revision text). The executor's outcome drives the next step: `Completed` → commit + force-with-lease push + success reply comment; `AskUser` → existing chatops escalation (no commit, no count increment, no PR reply yet, revision treated as in-progress); `Failed` → failure reply comment + count increment.

For the `Completed` outcome, the success reply comment SHALL carry the success line followed (when the executor's `final_answer` is non-empty after trimming) by a blank line AND the agent's `final_answer` text verbatim. The success line stays at the top so operators scanning for the ✓ confirmation see it immediately. When `final_answer` is `None` OR is empty after trimming, the comment body is the single-line success form (today's behavior); the change is purely additive.

The combined body SHALL be passed through the existing GitHub-comment-size truncation helper (`truncate_to_fit` OR equivalent) before posting, with a truncation marker appended when the body exceeds the limit. The marker text names the per-change log file path so operators can recover the full summary from disk.

#### Scenario: Completed revision updates the PR with a substantive summary
- **GIVEN** the executor returns `Completed { final_answer: Some("Did X. Declined Y because Z.") }` for a revision context
- **WHEN** the revision dispatcher composes the success comment
- **THEN** the daemon commits the workspace changes with subject `revise: <change>: <first 60 chars of revision text>`
- **AND** force-pushes with `--force-with-lease` to `repositories[].agent_branch`
- **AND** posts a PR issue comment whose body starts with `✅ Revision applied:`
- **AND** the comment body contains the agent's summary text `Did X. Declined Y because Z.` on the line(s) following a blank line after the success line
- **AND** the PR's diff updates to reflect the revision

#### Scenario: Completed revision without a substantive summary uses the single-line form
- **GIVEN** the executor returns `Completed { final_answer: None }` OR `Completed { final_answer: Some("   ") }` (empty after trim) for a revision context
- **WHEN** the revision dispatcher composes the success comment
- **THEN** the daemon posts a PR issue comment whose body is the single-line `✅ Revision applied: <subject>. Revision count: <n> of <cap>.` (no trailing blank line, no empty summary section)

#### Scenario: AskUser during revision escalates without committing
- **GIVEN** the executor returns `AskUser { question, resume_handle }` during revision execution
- **WHEN** the revision dispatcher processes the outcome
- **THEN** the existing chatops escalation path fires (the question is posted to the configured channel)
- **AND** no commit is made on the agent branch
- **AND** no PR reply comment is posted
- **AND** the revision-count counter is NOT incremented
- **AND** the comment's `created_at` is NOT marked as processed (so the next iteration after the human answer can resume against the same trigger comment)

#### Scenario: Failed revision posts a failure comment
- **GIVEN** the executor returns `Failed { reason }` for a revision context
- **WHEN** the revision dispatcher processes the outcome
- **THEN** the daemon posts a PR issue comment whose body starts with `✗ Revision attempt failed:` AND includes the reason
- **AND** the revision-count counter IS incremented (a failed attempt counts toward the cap)
- **AND** no commit or push is made

#### Scenario: Oversize summary is truncated with a marker pointing at the log file
- **GIVEN** the executor returns `Completed { final_answer: Some(very_long_text) }` where the composed body exceeds the GitHub comment-size limit
- **WHEN** the revision dispatcher composes the success comment
- **THEN** the body is truncated at the largest char boundary fitting under the limit
- **AND** a truncation marker is appended naming the per-change log file path on disk
- **AND** the operator can recover the full summary from `<logs_dir>/runs/<workspace-basename>/<change>.log`
