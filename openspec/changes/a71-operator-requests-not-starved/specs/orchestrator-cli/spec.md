# orchestrator-cli — delta for a71-operator-requests-not-starved

## ADDED Requirements

### Requirement: The queue walk yields to pending operator chatops requests
The per-repo change-queue walk SHALL NOT defer an operator chatops request — the `send it` audit-triage, the `propose` chat-request, OR the `changelog` request — by more than one change-cycle. These requests are drained at the top of each polling iteration, before the change walk; but because the walk processes a batch of pending changes (each a full executor run) before the iteration ends, a request that arrives mid-batch would otherwise wait for the entire batch to complete.

To bound this, after completing each change in the walk, the daemon SHALL check whether any operator chatops request is pending for the repo (the same `pending_triages` / `pending_proposal_requests` / `pending_changelog_requests` queues the iteration-top drains read). When any is pending, the walk SHALL end the current batch — the caller opens its PR with the changes accumulated so far — AND return, so the next iteration drains the pending operator request before starting a new batch. Operator-request latency is thereby bounded to at most one in-flight change.

The walk SHALL NOT interrupt a change that is already executing: the current change runs to its outcome before the walk yields. It only declines to START the next change when an operator request is waiting. A workspace-resetting operator request (changelog/propose reset to the base branch) therefore never interleaves with an in-flight change.

#### Scenario: A changelog request queued mid-batch is processed within one change-cycle
- **WHEN** a `changelog` request arrives while the walk is processing change N of a multi-change batch
- **THEN** after change N reaches its outcome, the walk ends the batch (the caller opens the PR with the changes accumulated through N) AND returns
- **AND** the next iteration drains the `changelog` request before starting a new batch
- **AND** the request is NOT deferred until changes N+1 … end of the batch complete

#### Scenario: The same bound applies to propose and send it
- **WHEN** a `propose` OR `send it` request is pending after a change completes in the walk
- **THEN** the walk yields after that change exactly as it does for `changelog`
- **AND** the operator request is drained on the next iteration

#### Scenario: An in-flight change is not interrupted
- **WHEN** an operator chatops request arrives while a change is mid-execution
- **THEN** that change runs to its outcome before the walk checks the operator-request queues
- **AND** no workspace-resetting operator request runs while a change occupies the workspace

#### Scenario: No operator request pending leaves batch behavior unchanged
- **WHEN** no operator chatops request is pending after each change
- **THEN** the walk processes the full batch (up to its existing limit) exactly as before
- **AND** there is no change in PR bundling for iterations with no pending operator request
