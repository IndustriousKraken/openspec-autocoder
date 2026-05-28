## Why

autocoder's current deployment model assumes the operator owns every repository it works on: specs live alongside the code; PR creation is automatic; the workspace's only remote is the operator's own host. For OSS-contribution workflows (where the operator wants autocoder to help land small targeted PRs on projects they do NOT own), three assumptions break:

1. **Spec storage**: canonical specs cannot live inside the upstream repo — that would force unrelated `openspec/` directories into PRs to projects that don't use spec-driven development. The operator needs specs in a separate location (typically a sibling public git repo the operator owns).
2. **Auto-submission**: autocoder auto-opens PRs at the end of each iteration. For OSS forks, the operator wants to review locally AND submit the upstream PR themselves after polishing — a bad auto-submitted PR damages the operator's reputation with maintainers in a way an internal PR does not.
3. **Upstream synchronization**: while autocoder is iterating on the operator's fork, upstream keeps moving. The operator needs a way to pull upstream changes into the fork on demand without leaving the chatops surface.

The three knobs needed are loosely coupled (an operator might want any subset) but combine into a coherent "OSS-fork workflow" that this change enables. None of them displace existing fork-PR mode (`github.fork_owner`) — that mechanism remains the right shape for operator-owned repos where a bot account holds the fork. OSS-fork mode is for the inverse: the operator owns the fork, autocoder works directly on it, and upstream is a third party.

## What Changes

**`spec_storage.path` per-repo config field.** When set, autocoder SHALL treat `<spec_storage.path>/openspec/` as the canonical-spec source instead of `<workspace>/openspec/`. The path SHALL be either workspace-relative OR absolute. The directory at the path SHALL be a git working tree (verified at config-load). When unset (default), behavior is unchanged: specs live at `<workspace>/openspec/`.

The polling iteration's spec-reading paths (canonical specs for the implementer prompt, audit-framework spec discovery, `openspec validate` invocations) SHALL all consult `spec_storage.path` when set. Spec-writing paths (brownfield draft outputs, scout spec-it outputs, `openspec archive` commits) SHALL write into the same `<spec_storage.path>/openspec/` directory in the spec_storage repo's working tree. Spec commits are committed locally; whether they are pushed and a PR is opened follows the same `auto_submit_pr` rules as code commits (described below), targeting the spec_storage repo's remote rather than the code workspace's remote.

**`upstream` per-repo config block.** When set, autocoder SHALL ensure the workspace has a git remote named `upstream.remote` (default `upstream`) pointing at `upstream.url` AND SHALL `git fetch upstream` opportunistically at the start of each polling iteration (best-effort; failures log a WARN but do not block the iteration). The `upstream.branch` field names the upstream's primary branch (default `main`). This config block enables — but does NOT trigger — automatic upstream syncing; syncing is operator-initiated via the new `sync-upstream` verb.

**`auto_submit_pr: bool` per-repo config field (default `true`).** When `false`, the git-workflow-manager SHALL push the agent branch per the existing rules (direct-push OR fork-PR mode) BUT SHALL skip the PR-creation API call. The polling iteration SHALL surface the branch's URL AND a templated `gh pr create` command suggestion in its thread notification, so the operator can submit the PR manually after local review. All other end-of-pass behaviors (implementer-summary capture, reviewer run, etc.) SHALL execute unchanged. When `true` (default), behavior is unchanged.

**New `sync-upstream` chatops verb.** Syntax:

```
@<bot> sync-upstream <repo-substring>
```

The dispatcher SHALL emit a `SyncUpstreamAction { repo_url, channel, thread_ts, request_id }`. The polling iteration's handler SHALL:

1. Verify `upstream` is configured for the repo; if not, post a thread reply naming the misconfiguration AND abort.
2. Run `git fetch <upstream.remote>` in the workspace.
3. Identify the workspace's base branch (the configured base branch the polling loop uses).
4. Attempt `git rebase <upstream.remote>/<upstream.branch>` on the base branch.
5. On conflict, abort the rebase (`git rebase --abort`) AND post a thread reply naming the conflicting files AND advising manual resolution.
6. On success, post a thread reply summarizing how many commits were pulled AND whether the workspace is now ahead of OR caught up to upstream.
7. NOT push the rebased base branch (operator decides when to push to their fork; `auto_submit_pr` semantics do not apply to upstream-sync).

The verb SHALL be subject to the existing busy-marker rule: if the repo is currently busy with another iteration's work, the request queues until the next free iteration (the existing per-repo serial-iteration discipline applies).

**No `workspace_mode` discriminator.** The three knobs are independently useful (own-project operators may want `spec_storage.path` to keep specs in a separate tree; bot-PR mode operators may want `auto_submit_pr: false` for sensitive repos). Treating them as separate knobs is more flexible than a coarse `workspace_mode` enum AND keeps config validation focused on per-field invariants.

**Implementer-prompt guidance (not specced here).** The operator is expected to author a tight implementer-prompt override (via `a24`'s uniform PromptLoader at `executor.implementer.prompt_path`) that emphasizes minimal diff, follow-existing-conventions, no-large-refactors. The spec does NOT mandate prompt content — that varies per upstream project. Documentation in `docs/OPERATIONS.md` SHALL include a recommended-snippets section operators can adapt.

## Impact

- **Affected specs:**
  - `chatops-manager` — ADDED: `Inbound listener recognizes the sync-upstream verb AND submits a SyncUpstreamAction`.
  - `orchestrator-cli` — ADDED: `spec_storage.path config redirects spec reads AND writes to an external git working tree`. ADDED: `upstream config block declares a fetch-only remote AND opportunistic fetch on iteration start`. ADDED: `sync-upstream polling-iteration handler rebases the base branch onto upstream/<branch>`. ADDED: `auto_submit_pr config field gates PR creation per repo`.
  - `git-workflow-manager` — MODIFIED: `Monolithic PR at end of pass` (the requirement gains an auto_submit_pr conditional; all 5 existing scenarios preserved verbatim; 2 new scenarios cover the auto_submit_pr: false branch-only path).
  - `project-documentation` — ADDED: `docs/CHATOPS.md, docs/OPERATIONS.md, AND docs/CONFIG.md document the sync-upstream verb, the spec_storage AND upstream config blocks, AND the auto_submit_pr field`.
- **Affected code:**
  - `autocoder/src/config.rs` — extend per-repo schema with `spec_storage.path`, `upstream.{remote, branch, url}`, AND `auto_submit_pr`. Config-load verifies `spec_storage.path` (when set) is a git working tree; verifies `upstream.url` parses; fails-fast on invalid values.
  - `autocoder/src/workspace/paths.rs` (or wherever workspace paths are resolved) — introduce a `SpecRoot` resolver that returns either `<workspace>/openspec` (default) OR `<spec_storage.path>/openspec` (when configured). Every existing call site that constructs `<workspace>/openspec/...` SHALL go through the resolver.
  - `autocoder/src/polling/iteration.rs` — opportunistic `git fetch upstream` at iteration start when configured; log WARN on failure but do NOT block.
  - `autocoder/src/polling/sync_upstream.rs` (new) — `sync-upstream` action handler.
  - `autocoder/src/git_workflow/pr.rs` (or wherever PR creation lives) — honor `auto_submit_pr`; when false, skip PR creation AND return a `BranchPushedNoPr { branch_url, suggested_command }` outcome to the caller.
  - `autocoder/src/chatops/listener.rs` — recognize `sync-upstream`; emit `SyncUpstreamAction`.
  - `autocoder/src/control_socket/actions.rs` — `SyncUpstreamAction` variant.
  - Spec-writing call sites (brownfield draft, scout spec-it, `openspec archive` invocations) — all route through the SpecRoot resolver AND target the spec_storage repo's working tree when configured. Commit + push + PR-creation logic for spec changes targets the spec_storage repo's remote.
- **Operator-visible behavior:**
  - `@<bot> help` lists `sync-upstream` alongside the existing verbs.
  - With `auto_submit_pr: false`, end-of-iteration thread notifications post `📦 Branch pushed: <branch-url>` followed by `Run: gh pr create --base <upstream-branch> --head <branch>` instead of `✅ PR opened: <pr-url>`.
  - With `spec_storage.path` set, the operator sees spec changes land in the configured sibling repo. Code changes land in the workspace repo. The two are independent git histories.
  - `@<bot> sync-upstream <repo>` produces a thread reply summarizing the rebase result OR naming conflicts that need manual resolution.
- **Breaking:** no. All new fields are optional; defaults preserve existing behavior. Existing `github.fork_owner` mode continues to work unchanged.
- **Acceptance:** `cargo test` passes; `openspec validate a26-oss-fork-support --strict` passes. New tests:
  - `spec_storage.path` config parses; invalid path (non-existent OR non-git) fails-fast.
  - `upstream` config parses; opportunistic fetch logs WARN on failure but doesn't block.
  - `auto_submit_pr: false` causes the PR-creation step to be skipped; branch is still pushed; the alternative thread notification is posted.
  - `sync-upstream` happy-path rebases AND posts the success thread reply.
  - `sync-upstream` conflict-path aborts the rebase AND posts the conflict thread reply.
  - `sync-upstream` without configured `upstream` block aborts with the misconfiguration reply.
  - `SpecRoot` resolver returns the correct path under both configurations.
