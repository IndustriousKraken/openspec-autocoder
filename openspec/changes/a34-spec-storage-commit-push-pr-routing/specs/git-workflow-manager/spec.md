## ADDED Requirements

### Requirement: `create_pr` helper accepts an explicit `--repo` argument for cross-repo PR creation

The `autocoder/src/github.rs::create_pr` helper (OR its equivalent shape) SHALL accept an optional `repo: Option<&str>` parameter. The parameter's value SHALL be a `<owner>/<name>` string. When `Some`, the helper's underlying `gh pr create` invocation SHALL receive `--repo <owner>/<name>` as an argument. When `None`, the existing behavior is preserved verbatim: no `--repo` flag is passed, AND `gh` uses the current working tree's origin to determine the target.

This parameter exists to support spec-storage PR creation, where the iteration's commit + push target a DIFFERENT git repo than the code workspace. The `gh` CLI's `--repo` flag natively handles this; the helper just passes it through.

Callers SHALL resolve the `<owner>/<name>` string from the target repo's remote URL (parsed from SSH OR HTTPS form) per the orchestrator-cli's spec-storage push-remote resolution requirement.

#### Scenario: `create_pr` with `repo: Some(...)` passes `--repo` to gh
- **WHEN** `create_pr` is invoked with `repo: Some("speccorp/specs-repo")`, `base: "main"`, `head: "agent-q"`, `title: "[specs] foo"`, AND a body
- **THEN** the underlying `gh pr create` invocation receives `--repo speccorp/specs-repo` in its argv
- **AND** receives `--base main --head agent-q --title "[specs] foo"`

#### Scenario: `create_pr` with `repo: None` omits the `--repo` flag
- **WHEN** `create_pr` is invoked with `repo: None` (the existing code-workspace path)
- **THEN** the underlying `gh pr create` invocation does NOT include `--repo` in its argv
- **AND** `gh` determines the target from the current working tree's origin (existing behavior)

#### Scenario: `create_pr` failure on cross-repo target surfaces a clear error
- **WHEN** `create_pr` is invoked with `repo: Some("speccorp/nonexistent")` AND the `gh` CLI returns non-zero with a "Repository not found" stderr
- **THEN** the helper returns `Err` carrying the captured stderr verbatim
- **AND** the operator-visible error names the target repo AND `gh`'s failure reason

### Requirement: `auto_submit_pr: false` post-push notification for spec-storage PRs uses `--repo`

When `auto_submit_pr: false` AND the iteration's classification is spec-only OR dual-tree's spec half, the post-push notification's `gh pr create` suggestion SHALL include `--repo <spec-owner>/<spec-name>` so the operator's manual invocation targets the correct repo.

Canonical notification body shape:

```
📦 Spec branch pushed to <spec-repo-url>:<branch>. Open a PR with:
  gh pr create --repo <spec-owner>/<spec-name> --base <resolved-base-branch> --head <branch> --title "[specs] <change-list-summary>"
```

The existing code-PR notification format is unchanged (no `--repo` flag, no `[specs] ` prefix).

#### Scenario: Spec-only push suggests cross-repo gh invocation
- **WHEN** a spec-only iteration completes AND `auto_submit_pr: false` AND `spec_storage.path` is configured pointing at `git@github.com:speccorp/specs-repo.git`
- **THEN** the post-push notification body contains `gh pr create --repo speccorp/specs-repo --base main --head agent-q --title "[specs] ..."`
- **AND** does NOT contain a bare `gh pr create` (without `--repo`)

#### Scenario: Code-only push notification is unchanged
- **WHEN** a code-only iteration completes AND `auto_submit_pr: false`
- **THEN** the post-push notification body contains the existing `gh pr create` suggestion WITHOUT `--repo` (the code workspace's origin determines the target)
- **AND** does NOT contain the `[specs] ` title prefix

### Requirement: Reviewer SHALL skip spec-only PRs when `reviewer.skip_spec_only_prs: true`

The `ReviewerConfig` SHALL accept an optional `skip_spec_only_prs: bool` field (default `false`). When `true`, the polling iteration's reviewer-invocation step SHALL skip the reviewer call AND post no `## Code Review` section for any PR whose ENTIRE diff lives under `openspec/`. The detection SHALL use the same diff classification as the iteration's commit + push classification (per the orchestrator-cli "Polling iteration classifies outcome" requirement): a PR opened from a spec-only iteration's classification is a spec-only PR; a PR opened from a code-only iteration's classification is NOT.

When `false` (default), the reviewer runs against spec-only PRs exactly as it runs against code-only PRs (existing canonical behavior preserved).

The toggle is a cost-optimization knob. Operators who want to skip reviewer LLM cost on spec-only PRs (which produce review verdicts that are typically less actionable than code review verdicts) can enable it. Operators who want full review coverage leave it at the default.

#### Scenario: `skip_spec_only_prs: true` skips reviewer on spec-only PR
- **WHEN** `reviewer.skip_spec_only_prs: true` AND a brownfield iteration produces a PR whose diff is entirely under `openspec/`
- **THEN** the reviewer is NOT invoked
- **AND** the PR body contains NO `## Code Review` section
- **AND** the iteration log includes an INFO line `reviewer: skipping spec-only PR per skip_spec_only_prs config`

#### Scenario: `skip_spec_only_prs: true` does NOT skip reviewer on dual-tree code PR
- **WHEN** `reviewer.skip_spec_only_prs: true` AND a dual-tree iteration produces TWO PRs (code + spec)
- **THEN** the spec PR's reviewer step is skipped
- **AND** the code PR's reviewer step runs normally (the diff includes `autocoder/src/...` AND/OR similar non-`openspec/` paths)

#### Scenario: `skip_spec_only_prs: false` (default) runs reviewer on all PRs
- **WHEN** `reviewer.skip_spec_only_prs` is unset OR `false` AND a spec-only iteration produces a PR
- **THEN** the reviewer is invoked exactly as today (existing canonical behavior preserved)
- **AND** the PR body contains the `## Code Review` section
