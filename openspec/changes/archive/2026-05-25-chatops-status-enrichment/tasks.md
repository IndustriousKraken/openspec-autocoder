## 1. Data-shape additions

- [x] 1.1 In `autocoder/src/chatops/operator_commands.rs`, add three new public types:
  ```rust
  pub struct CommitSummary {
      pub short_sha: String,
      pub subject: String,
      pub age: chrono::Duration,
  }
  pub struct PrSummary {
      pub number: u64,
      pub title: String,
      pub state: String,    // "open" | "closed" | "merged"
      pub head_branch: String,
      pub url: String,
      pub age: chrono::Duration,
  }
  pub struct BusySummary {
      pub change: String,
      pub started_at: DateTime<Utc>,
  }
  ```
- [x] 1.2 Extend `RepoStatusResponse` with the new fields:
  ```rust
  pub base_branch: String,
  pub agent_branch: String,
  pub last_commit_base: Option<CommitSummary>,
  pub last_commit_agent: Option<CommitSummary>,
  pub latest_pr: Option<PrSummary>,
  pub currently_busy: Option<BusySummary>,
  ```
  The existing fields (`url`, marker lists, throttled alerts, pending/waiting changes, last_iteration) are unchanged.

## 2. Local git-log reads

- [x] 2.1 In `autocoder/src/git.rs`, add `pub fn last_commit_summary(workspace: &Path, branch: &str) -> Result<Option<CommitSummary>>`. Implementation: `git log -1 --pretty=format:%h%x09%ct%x09%s <branch>` (tab-separated: short-sha, committer-timestamp, subject). On `unknown revision` / `bad revision` stderr, return `Ok(None)` — the branch does not exist (fresh clone, agent branch not yet created). Compute `age` as `Utc::now() - committer_timestamp`.
- [x] 2.2 Tests:
  - Happy path: fixture workspace with a commit on the branch returns `Some(CommitSummary)` with correct sha + subject.
  - Nonexistent branch returns `Ok(None)`, not `Err`.
  - Subject with a tab character is handled (the tab is part of the third field after splitting on the first two).
  - Detached HEAD / no commits at all returns `Ok(None)`.

## 3. GitHub PR lookup

- [x] 3.1 In `autocoder/src/github.rs`, add `pub async fn latest_pr_for_head(api_base: &str, token: &str, owner: &str, repo: &str, head_branch: &str) -> Result<Option<PrSummary>>`. Implementation: `GET {api_base}/repos/{owner}/{repo}/pulls?head={owner}:{head_branch}&state=all&sort=created&direction=desc&per_page=1`. On 200 with a non-empty list, return `Some(PrSummary)` constructed from `number`, `title`, `state` (mapping `state==closed && merged_at.is_some()` to `"merged"`), `head.ref`, `html_url`, and `Utc::now() - created_at`. On 200 with an empty list, return `Ok(None)`. On any error (network, 4xx, 5xx, decode failure), return `Err` so the caller can decide whether to swallow (which the status path does — see task 4.3).
- [x] 3.2 Tests (mockito):
  - 200 with one PR → returns `Some(PrSummary)` with all fields populated correctly.
  - 200 with `merged_at: "<timestamp>"` and `state: "closed"` → maps to `state: "merged"`.
  - 200 with empty list → returns `Ok(None)`.
  - 404 → returns `Err`.
  - 403 rate-limit → returns `Err` (caller swallows).
  - Decode failure on missing required field → returns `Err`.

## 4. Wire into the status handler

- [x] 4.1 In `autocoder/src/busy_marker.rs`, add a read-only peek method `pub fn current(&self) -> Option<BusySummary>`. Reads the marker file (if present), parses the `change` field and the marker's mtime as `started_at`. Does NOT take, hold, or release the marker — purely informational. Return `None` if the marker file does not exist.
- [x] 4.2 In `autocoder/src/control_socket.rs::build_repo_status`, populate the new fields:
  - `base_branch` / `agent_branch` from `repo.base_branch` / `repo.agent_branch`.
  - `last_commit_base` / `last_commit_agent` via the new `git::last_commit_summary` helper.
  - `currently_busy` via the new `busy_marker::current` peek.
  - `latest_pr` via the new `github::latest_pr_for_head` — derive `owner` / `repo` from `repo.url` via the existing `github::parse_repo_url` helper.
- [x] 4.3 GitHub call failure handling: if `latest_pr_for_head` returns `Err`, log at WARN with the underlying error and set `latest_pr: None` in the response. The status reply MUST NOT fail because GitHub is rate-limited or briefly down — an operator hitting `@<bot> status <repo>` during a GitHub incident still gets the local-state half of the reply.
- [x] 4.4 Local git failure handling: if `last_commit_summary` returns `Err` (workspace not yet cloned, .git dir corrupt), log at WARN and set the respective field to `None`. Same principle — partial reply is better than no reply.

## 5. Reply formatter

- [x] 5.1 Add `fn slack_escape(s: &str) -> String` in the same module: replaces `&` → `&amp;`, `<` → `&lt;`, `>` → `&gt;`. (The order matters — `&` first, otherwise the substitution would double-escape its own output.)
- [x] 5.2 Update `format_status_reply` to emit the new sections in this exact order, placed BEFORE the existing marker / throttled-alert / queue sections:
  ```
  📊 <url>

  branches: base=<base>, agent=<agent>
  last commit on <base>:    <short_sha> "<subject>" (<age> ago)   |   (none)
  last commit on <agent>:   <short_sha> "<subject>" (<age> ago)   |   (none)

  latest PR: #<num> "<title>"  <state> · head=<agent> · <age> ago
             <url>
                                                                  |   (none)

  currently: idle   |   working on <change> (started <age> ago)
  next iteration: in <age> (poll_interval <Ns>, jitter ±<N>%)
  ```
  followed by the existing sections (markers, throttled alerts, queue) unchanged.
- [x] 5.3 Queue one-liner: when ALL of `pending_changes`, `waiting_changes`, marker-excluded counts are ≤5, render as a single line `queue: N pending (<list>), M waiting (<list>), K excluded`. When any list exceeds 5 entries, fall back to the existing per-line format. Use `slack_escape` on every change-name field (change names are restricted by the parser's allowlist, but escaping is a cheap belt-and-braces measure).
- [x] 5.4 All user-controlled fields (commit subjects, PR titles) pass through `slack_escape` before being formatted into the reply. The escape is the last step before concatenation.
- [x] 5.5 Tests:
  - Healthy repo with all sections populated produces a reply matching the documented shape (snapshot test).
  - `last_commit_base: None`, `last_commit_agent: None`, `latest_pr: None`, `currently_busy: None` renders `(none)` in each line, not blank.
  - Commit subject `<!channel>` is escaped to `&lt;!channel&gt;` in the reply.
  - PR title `<@U123> ping` is escaped.
  - Queue with 6+ pending entries falls back to the per-line format.
  - The order of escape substitutions: input `&<` produces `&amp;&lt;`, not `&amp;lt;`.

## 6. README documentation update

- [x] 6.1 In the README's "ChatOps operator commands" section, update the `status` row of the verb table to describe the enriched reply (mention the always-present branches / commits / latest-PR / currently-busy lines). Replace the existing one-sentence description.
- [x] 6.2 Update the `status` example in the README's existing reply-examples block to show the enriched shape.

## 7. Spec delta

- [x] 7.1 The ADDED requirement in `openspec/changes/chatops-status-enrichment/specs/chatops-manager/spec.md` covers: always-present sections (branches, last commit per branch, latest PR, currently busy), the (none) placeholder when data is absent, the queue one-liner format, the Slack-escape rule for user-controlled fields, and the partial-reply rule when GitHub or local git fails.

## 8. Verification

- [x] 8.1 `cargo test` passes (new + existing).
- [x] 8.2 `openspec validate chatops-status-enrichment --strict` passes.
- [x] 8.3 `cargo clippy --all-targets --all-features -- -D warnings` produces no new warnings.
