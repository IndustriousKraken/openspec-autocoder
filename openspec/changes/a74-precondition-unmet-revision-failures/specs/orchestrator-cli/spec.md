# orchestrator-cli — delta for a74-precondition-unmet-revision-failures

## MODIFIED Requirements

### Requirement: Revision execution updates the agent branch and posts a reply comment
On a triggering comment for an open PR, the daemon SHALL re-invoke the executor in revision mode (passing the original change material, the current PR diff, AND the revision text). The executor's outcome drives the next step: `Completed` → see the branching below; `AskUser` → existing chatops escalation (no commit, no count increment, no PR reply yet, revision treated as in-progress); a substantive `Failed` (the subprocess ran and the task failed) → failure reply comment + count increment; a **precondition-unmet** failure (the agent subprocess never started because a required precondition was unmet, e.g. the OS-sandbox-mechanism gate) → failure reply comment that directs the operator to resolve the precondition AND post a new revision request, with the trigger consumed (manual re-trigger; the daemon does NOT auto-retry, since an unmet precondition will not heal between polls) but the revision count NOT incremented (no revision work was attempted).

For the `Completed` outcome, the daemon SHALL first determine whether the agent produced code changes (a dirty working tree):

- **Dirty tree** — the agent applied a change: the daemon commits + force-with-lease pushes to `repositories[].agent_branch`, then posts the success reply comment. A genuine commit/push failure on this branch is reported via the failure reply comment + count increment (unchanged).
- **Clean tree** — the agent deliberately made no change (e.g. it declined the request after verifying the claim was wrong, per the executor requirement `Revision prompt instructs critical evaluation of the reviewer's request`): the daemon SHALL NOT attempt a commit (a clean tree is NOT a commit/push failure) AND SHALL NOT post a failure comment. It posts a success reply comment whose first line marks an evaluation with no change made — distinct from the `✅ Revision applied:` line (e.g. `✅ Revision evaluated, no change made:`) — carrying the agent's declination summary.

Both branches count the attempt against the revision cap AND advance the seen-marker.

For a `Completed` success reply comment (either branch), the body SHALL carry its first line followed (when the executor's `final_answer` is non-empty after trimming) by a blank line AND the agent's `final_answer` text verbatim. The first line stays at the top so operators scanning for the ✓ confirmation see it immediately. When `final_answer` is `None` OR is empty after trimming, the dirty-tree branch posts the single-line `✅ Revision applied: <subject>. Revision count: <n> of <cap>.` form (a45 behavior) AND the clean-tree branch posts its no-change line alone.

The combined body SHALL be passed through the existing GitHub-comment-size truncation helper (`truncate_to_fit` OR equivalent) before posting, with a truncation marker appended when the body exceeds the limit. The marker text names the per-change log file path so operators can recover the full summary from disk.

#### Scenario: Completed revision updates the PR with a substantive summary
- **GIVEN** the executor returns `Completed { final_answer: Some("Did X. Declined Y because Z.") }` for a revision context AND the agent produced code changes (dirty working tree)
- **WHEN** the revision dispatcher composes the success comment
- **THEN** the daemon commits the workspace changes with subject `revise: <change>: <first 60 chars of revision text>`
- **AND** force-pushes with `--force-with-lease` to `repositories[].agent_branch`
- **AND** posts a PR issue comment whose body starts with `✅ Revision applied:`
- **AND** the comment body contains the agent's summary text `Did X. Declined Y because Z.` on the line(s) following a blank line after the success line
- **AND** the PR's diff updates to reflect the revision

#### Scenario: Completed revision without a substantive summary uses the single-line form
- **GIVEN** the executor returns `Completed { final_answer: None }` OR `Completed { final_answer: Some("   ") }` (empty after trim) for a revision context AND the agent produced code changes (dirty working tree)
- **WHEN** the revision dispatcher composes the success comment
- **THEN** the daemon posts a PR issue comment whose body is the single-line `✅ Revision applied: <subject>. Revision count: <n> of <cap>.` (no trailing blank line, no empty summary section)

#### Scenario: Completed with no code change is a reported declination, not a failure
- **GIVEN** the executor returns `Completed { final_answer: Some("Declined: the cited test does not exist; verified against the current code. No change made.") }` AND the working tree is clean (the agent made no code change)
- **WHEN** the revision dispatcher processes the outcome
- **THEN** the daemon does NOT attempt a commit (a clean tree is not a commit/push failure) AND does NOT post a `✗ Revision attempt failed` comment
- **AND** it posts a success comment whose first line marks an evaluation with no change made (distinct from `✅ Revision applied:`), followed by a blank line AND the agent's `final_answer` declination summary
- **AND** the attempt counts against the revision cap AND the seen-marker advances

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

#### Scenario: Precondition-unmet revision failure does not count and guides re-trigger
- **GIVEN** the executor surfaces a precondition-unmet failure for a revision context (the agent subprocess never started, e.g. no usable sandbox mechanism)
- **WHEN** the revision dispatcher processes the outcome
- **THEN** the daemon posts a failure reply comment that directs the operator to resolve the precondition AND post a new revision request
- **AND** the revision-count counter is NOT incremented (no revision was attempted)
- **AND** the trigger comment's timestamp is advanced so the daemon does NOT auto-retry (manual re-trigger required)
- **AND** no commit or push is made
