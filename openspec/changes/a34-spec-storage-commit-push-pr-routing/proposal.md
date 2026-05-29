## Why

`a26-oss-fork-support` archived on 2026-05-29 with the canonical `spec_storage.path config redirects spec reads AND writes to an external git working tree` requirement fully specified, including five scenarios covering the resolver redirect, write redirect, AND config-load validation. The §2.3 SpecRoot call-site refactor work landed in the merged PR (101 SpecRoot references in production code; raw `workspace.join("openspec")` remains only in test fixtures). What did NOT land is §3 of the original tasks.md: **spec-change commit/push/PR routing into the spec_storage repo when configured.**

The canonical spec says "Spec-change commits (brownfield draft, scout spec-it, `openspec archive`) SHALL be made in the spec_storage repo's working tree when `spec_storage` is set; the spec_storage repo's remote AND base branch determine the push target AND PR base." Today's implementation honors the spec_storage path for SpecRoot READS, but the commit/push/PR path still operates against the code workspace's working tree unconditionally. An operator who configures `spec_storage.path` AND runs a brownfield iteration today gets:

- Brownfield draft files written to the spec_storage repo's working tree (correct).
- The commit + push attempted in the code workspace repo (wrong — the change files are in a different working tree).
- The PR opened against the code workspace's base branch (wrong — should target the spec_storage repo's remote).

The result is a half-broken pipeline: spec writes go to the right place AND become uncommitted changes in the spec_storage repo; the code workspace repo's commit step finds nothing to commit (the writes happened elsewhere) AND emits the "nothing to commit" failure. The operator sees a PR-comment failure AND has to manually commit + push the spec changes from the spec_storage repo.

This change closes that gap. It also pins down implementation-narrowing requirements the canonical leaves under-specified:

- **Which remote AND base branch** to push to in the spec_storage repo (the canonical says "the spec_storage repo's remote AND base branch determine" but doesn't pin defaults).
- **PR title prefix** to distinguish spec-only PRs from code PRs in operator UIs (so reviewers can sort).
- **Reviewer behavior** on spec-only PRs (run? skip? same prompt?).
- **`auto_submit_pr: false` interaction** with the spec_storage pipeline (the post-push notification's `gh pr create --repo <owner>/<spec-repo>` form).

## What Changes

**Polling iteration's commit + push + PR step routes to the spec_storage repo when configured.** Three of the existing iteration paths that produce spec changes:

1. Brownfield-draft iteration (per `a23`).
2. Scout spec-it iteration (per `a25`).
3. `openspec archive` execution (the iteration's standard archive step).

SHALL, when `spec_storage` is configured for the active repo:

- Stage the changes in `<spec_storage.path>` (the working tree where the writes happened per the existing canonical SpecRoot-write-redirect requirement).
- Determine the spec_storage repo's push remote: default `origin` (the remote present in the spec_storage repo's working tree). Operators may override via a new optional `spec_storage.push_remote: String` field (default `"origin"`).
- Determine the spec_storage repo's base branch: default to the remote-tracked default branch (`git -C <spec_storage.path> symbolic-ref refs/remotes/origin/HEAD`). Operators may override via a new optional `spec_storage.base_branch: String` field (default = remote-tracked HEAD).
- Commit in the spec_storage working tree with the standard implementer-summary commit message.
- Push the branch to the resolved remote.
- Open a PR against the resolved base branch (when `auto_submit_pr: true`) OR post the `gh pr create --repo <spec-repo-owner>/<spec-repo-name>` suggestion (when `false`).

The code workspace's git operations are entirely skipped for the spec-only path. The polling iteration's commit step SHALL detect the no-code-changes case (clean code working tree + dirty spec_storage working tree) AND route exclusively through the spec_storage path. When BOTH working trees have changes (a brownfield iteration that drafted a spec AND modified code-workspace fixtures), the iteration SHALL commit + push BOTH separately AND open TWO PRs.

**PR title prefix for spec-only PRs.** The PR title SHALL be prefixed with `[specs] ` to distinguish from code-change PRs in operator UIs that sort by title. Example: `[specs] a36-spec-edit-foo (+1 more)`. The prefix applies to brownfield AND scout spec-it AND archive-driven spec-change PRs equally. Code PRs are unprefixed (existing format preserved).

**Reviewer behavior on spec-only PRs.** Default: the reviewer SHALL run against spec-only PRs using the same prompt as code PRs. The reviewer's existing prompt evaluates spec deltas naturally (it reads the diff). For operators who want to skip reviewer cost on spec-only PRs, a new `reviewer.skip_spec_only_prs: bool` config knob (default `false`) gates the behavior: when `true`, the polling iteration SHALL skip the reviewer step for PRs whose ENTIRE diff is under `openspec/` (no source-code or test changes). This is a cost-optimization knob, not a quality requirement.

**`auto_submit_pr: false` on spec-only PRs.** The post-push notification's `gh pr create` suggestion SHALL target the spec_storage repo's owner/name when `spec_storage` is configured. Format:

```
📦 Spec branch pushed to <spec-repo-url>:<branch>. Open a PR with:
  gh pr create --repo <spec-repo-owner>/<spec-repo-name> --base <resolved-base-branch> --head <branch> --title "[specs] <change-list-summary>"
```

The existing `gh pr create` suggestion for code-change PRs is unchanged.

**Implementation enforcement.** A new integration test SHALL exercise the spec-only commit/push/PR path end-to-end against a temp-workspace fixture with a configured `spec_storage` AND a brownfield iteration. The test asserts that the spec_storage repo's working tree was committed AND pushed, the code workspace was NOT modified, AND the PR's `--repo` argument targets the spec_storage repo.

## Impact

- **Affected specs:**
  - `orchestrator-cli` — ADDED requirements for: the spec_storage push-remote AND base-branch resolution rules, the polling-iteration routing per outcome (spec-only / code-only / dual), AND the `[specs] ` PR title prefix.
  - `git-workflow-manager` — ADDED requirements for: the spec_storage PR creation `--repo` argument, the auto_submit_pr-false suggestion format for spec-only PRs, AND the reviewer-skip toggle.
- **Affected code:**
  - `autocoder/src/config.rs` — `SpecStorageConfig` gains optional `push_remote: Option<String>` AND `base_branch: Option<String>` (both default unset; runtime resolves defaults). `ReviewerConfig` gains `skip_spec_only_prs: bool` (default `false`).
  - `autocoder/src/polling_loop.rs` — the iteration's commit + push + PR step gains spec-storage routing. The detection of "spec-only" vs "code-only" vs "dual-tree" diffs uses `git -C <each-path> status --porcelain`.
  - `autocoder/src/git.rs` — new helpers `commit_in_tree`, `push_in_tree`, `default_branch_for_remote` (all `(path, ...)` parameterized to operate on any working tree).
  - `autocoder/src/github.rs` — `create_pr` gains an optional `--repo <owner>/<name>` parameter for targeting a non-origin repo. (The `gh` CLI supports this natively; the helper passes it through.)
  - `autocoder/src/code_reviewer.rs` — the iteration-time invocation gains a "skip if spec-only AND `reviewer.skip_spec_only_prs: true`" gate.
- **Operator-visible behavior:**
  - Operators with `spec_storage.path` configured AND `auto_submit_pr: true` see their brownfield + scout spec-it + archive iterations produce PRs in the spec_storage repo, NOT the code workspace repo.
  - PR titles for spec-only PRs are prefixed with `[specs] `.
  - `chatops_channel_id` per-repo routing applies unchanged (both code-workspace AND spec-storage PRs go through the configured channel).
  - Operators without `spec_storage` configured see NO behavioral change.
- **Backward compatibility:** existing configs without `spec_storage` continue to operate against the code workspace exclusively. Existing configs WITH `spec_storage` (which currently produce half-broken iterations) gain working spec-storage routing — this is a bug-fix migration, not a behavioral break.
- **Dependencies:** synergizes with `a2705` (strict-since filter) AND the `a27a*` outcome-tools stack — those changes harden the dispatcher pipeline broadly. Independent of `a35` (the paths-globals removal). Can land in any order relative to the rest.
- **Acceptance:** `cargo test` passes; `openspec validate a34-spec-storage-commit-push-pr-routing --strict` passes. Tests:
  - Spec-only iteration with `spec_storage` configured commits to the spec_storage repo's working tree AND pushes to its remote AND opens a PR targeting its base branch.
  - Spec-only iteration with `spec_storage` configured AND `auto_submit_pr: false` produces the `gh pr create --repo <spec-repo>/<name>` suggestion in the post-push notification.
  - Code-only iteration with `spec_storage` configured commits to the code workspace repo's working tree (the spec_storage repo is untouched).
  - Dual-tree iteration commits + pushes + opens PRs in BOTH the code workspace repo AND the spec_storage repo (two separate PRs).
  - PR title format for spec-only PRs starts with `[specs] `.
  - PR title format for code-only AND dual-tree PRs is unchanged from pre-spec behavior.
  - Reviewer's `skip_spec_only_prs: true` skips the reviewer step for spec-only PRs; default `false` runs the reviewer.
  - Per-repo `chatops_channel_id` routing applies to spec-only PR notifications.
  - `push_remote` default resolution: when unset, uses `origin` from `git -C <spec_storage.path> remote`.
  - `base_branch` default resolution: when unset, uses the remote-tracked HEAD via `git -C <spec_storage.path> symbolic-ref refs/remotes/origin/HEAD`.
  - Override resolution: when `spec_storage.push_remote: "upstream-fork"` is set, the push targets `upstream-fork`; when `spec_storage.base_branch: "develop"` is set, the PR base is `develop`.
