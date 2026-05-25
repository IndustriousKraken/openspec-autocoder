## Why

The current `@<bot> status <repo>` reply is shaped for the "something is wrong, show me what" case — every section (active markers, throttled alerts, queue contents) is collapsed when empty. For a healthy repo with no markers, no throttles, and an empty queue, the reply collapses to one line: the repo URL. That tells the operator nothing about whether the daemon is actually doing what they expect.

The "nothing's wrong, am I making progress" case is the more frequent one. An operator who just pushed a new change wants to know: did the daemon notice it? Is it working on it now? When did it last commit? Where's the PR? Today they have to switch to a terminal and run `git log` + `gh pr list` + `journalctl` to answer those questions — which is exactly the friction the operator-commands surface was created to eliminate.

The status reply should always return a live snapshot: branches in scope, latest commit on each, latest PR by the daemon, current busy state, queue summary. Operator can read it cold and know where the daemon is in its cycle.

## What Changes

**Add five always-present sections to the status reply.** The existing marker / throttle sections continue to collapse when empty, but the reply is no longer dominated by collapsed-section invisibility.

1. **Branches.** One line: `branches: base=<base_branch>, agent=<agent_branch>`. Sourced from `RepositoryConfig` already in scope.

2. **Last commit per branch.** Two lines: `last commit on <base>:    <short_sha> "<subject>" (<age> ago)` and the same for the agent branch. Sourced from `git log -1 --pretty=format:'%h%x09%s' <branch>` against the local workspace. No network call. Subject text is Slack-escaped (see Threat model).

3. **Latest PR by the daemon.** One line plus a URL line: `latest PR: #<num> "<title>"  <state> · head=<agent_branch> · <age> ago` and the PR URL on its own line. Sourced from GitHub's `GET /repos/{owner}/{repo}/pulls?head={owner}:{agent_branch}&state=all&sort=created&direction=desc&per_page=1`. The "by the daemon" interpretation is intentional — operators querying status want to know what the daemon is doing for them, not what every contributor is doing. If no PR has ever been opened from the agent branch, the line reads `latest PR: (none)`.

4. **Currently active.** One line: `currently: idle` OR `currently: working on <change> (started <age> ago)`. Sourced from the per-repo busy marker the daemon already maintains for skip-if-busy logic — exposes the same state the polling-loop's skip-when-busy check consults. The line is always present so an operator can tell at-a-glance whether they caught the daemon mid-iteration or between iterations.

5. **Queue one-liner.** Collapses the existing per-line `pending`, `waiting`, `excluded` sections (when small) into one line: `queue: N pending (<list>), M waiting, K excluded`. When `pending` has more than 5 entries, the list truncates with `…+N more`. This replaces the existing multi-line queue format for the common compact case; the multi-line format reappears for `waiting` / `excluded` only when those sections have entries (their existing format is fine — they're rare).

The `next iteration` line (already present in the existing format) stays as-is.

**Threat model: escape user-controlled text.** Commit subjects and PR titles are author-controlled. A malicious commit subject like `<!channel> ping everyone` or `<@U999> someone specific` would, if pasted raw into a Slack message, ping the channel or user. The status formatter SHALL escape `<`, `>`, and `&` to `&lt;`, `&gt;`, `&amp;` before assembling the reply text. This is a defense-in-depth measure consistent with the inbound-listener change's drop-before-dispatch filters: untrusted text never reaches Slack unescaped.

**No new dependencies.** The git-log read uses the existing `git` shell-out pattern in `autocoder/src/git.rs`. The PR lookup uses the existing `github.rs` module + the operator's PAT (no new auth surface). The busy-marker read uses the existing `busy_marker.rs` API.

**Status remains a sync reply.** No change to the dispatcher contract — `status` returns `Some(Reply::Sync(text))` exactly as before. The reply text just contains more information.

## Impact

- **Affected specs:** `chatops-manager` — one ADDED requirement covering the new status reply content + the escaping rule. (The original `status` verb behavior was never canonicalized after the `a03-chatops-operator-commands` archive — a separate sync-specs rebuild can backfill that requirement; this change does not depend on it.)
- **Affected code:**
  - `autocoder/src/chatops/operator_commands.rs` — extend `RepoStatusResponse` with: `base_branch`, `agent_branch`, `last_commit_base: Option<CommitSummary>`, `last_commit_agent: Option<CommitSummary>`, `latest_pr: Option<PrSummary>`, `currently_busy: Option<BusySummary>`. Update `format_status_reply` to emit the new sections in the order above and to escape Slack-special characters in user-controlled fields. Add `CommitSummary { short_sha, subject, age }`, `PrSummary { number, title, state, head_branch, url, age }`, `BusySummary { change, started_at }` types.
  - `autocoder/src/control_socket.rs` — extend `build_repo_status` to populate the new fields. The git-log calls happen in the same call. The GitHub API call happens here too; failures (network, rate-limit, 4xx) downgrade to `latest_pr: None` without failing the whole status response (a status reply should not fail because GitHub is having a bad day).
  - `autocoder/src/git.rs` — small helper `last_commit_summary(workspace, branch) -> Result<Option<CommitSummary>>` that returns `None` if the branch doesn't exist (e.g. fresh clone, agent branch not yet created). Pure local git read.
  - `autocoder/src/github.rs` — small helper `latest_pr_for_head(owner, repo, head_branch) -> Result<Option<PrSummary>>` calling `GET /repos/{owner}/{repo}/pulls`. Reuses existing auth.
  - `autocoder/src/busy_marker.rs` — expose a public read method that returns `Option<(change, started_at)>` without taking the marker. (The existing API takes-or-releases; for status display we need a read-only peek.)
  - Tests:
    - Unit tests for the formatter with each new section populated and empty (latest_pr=None, currently_busy=None, branches with no commits yet).
    - Slack-escape tests: subject containing `<!channel>`, `<@U123>`, `&lt;script&gt;` literal, plain ampersand. Assert escaped output.
    - `last_commit_summary` returns `None` for nonexistent branch (no error).
    - `latest_pr_for_head` mockito-driven: 200 with list, 200 with empty list, 404, rate-limit 403.
- **Operator-visible behavior:** the status reply gains five always-present sections. Healthy-repo replies become actually useful. Existing marker / throttle / queue (when non-trivial) formatting unchanged.
- **Breaking:** no. Reply text is a strict superset (plus reformat of the queue section when small). No JSON schema is exposed across a network boundary — `RepoStatusResponse` is internal between the daemon and itself.
- **Acceptance:** `cargo test` passes (new + existing). `@<bot> status <repo>` against a healthy repo returns the full multi-section reply within ~1 second. Against a repo whose `agent_branch` does not exist yet, the agent-branch-commit and latest-PR lines render gracefully (`(none)`) rather than erroring.
