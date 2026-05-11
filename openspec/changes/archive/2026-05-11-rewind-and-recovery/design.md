## Context

The architecture spec already defines rewind's behavior in `orchestrator-cli/spec.md`: hard rewind deletes the agent branch, soft rewind requires confirmation, repo-relative archived directories are matched by date-prefix regex. Phase-1-foundation stubbed the implementation; multi-repo-manager extended the daemon but not the rewind path. This change finishes the implementation and adds the selector argument that makes rewind unambiguous in multi-repo deployments.

## Goals / Non-Goals

**Goals:**
- A working `rewind` subcommand end-to-end against a real workspace.
- A `--repo <selector>` argument that matches against the URL exactly OR against a derived short-name (basename minus `.git`).
- Clear errors when the selector matches zero or multiple configured repos.
- Default-to-no confirmation prompt for soft rewind.

**Non-Goals:**
- Selective git revert (granular fixes to individual commits). Rewind is "delete the branch and unarchive the changes"; if you want to keep partial work, do that manually before invoking rewind.
- Recovering deleted branches that have already been pushed and pulled by another machine. The `--force-with-lease` push semantics from `git-workflow-manager` make hostile branch resurrection an explicit override; rewind respects that.
- Unarchiving changes whose archive entries have been manually corrupted (date prefix removed, etc.). Rewind requires the canonical `<YYYY-MM-DD>-<name>` format.

## Decisions

- **`--repo` selector resolution:**
  - With multiple configured repositories AND `--repo` absent: exit non-zero, stderr listing the available selectors.
  - With multiple configured repositories AND `--repo` present: match the value against each configured repo's URL exactly OR against the URL's basename minus `.git`. Exactly one match → proceed; zero matches → exit non-zero with the available selectors; multiple matches → exit non-zero naming the conflicting repos.
  - With exactly one configured repository AND `--repo` absent: default to that repo, log a confirmation line naming the chosen repo.
- **Soft-rewind confirmation:** the prompt is `"This will delete branch '<agent_branch>' (local) and unarchive <N> change(s). Proceed? [y/N]"`. Default on bare Enter is "no". Any input other than `y` or `Y` (after trimming whitespace) is treated as decline.
- **Hard rewind:** skips the confirmation prompt, deletes both local AND remote agent branch, then unarchives. If remote deletion fails (branch did not exist remotely, or auth failure), the failure is logged but does not block the unarchive step — the local cleanup and unarchive still happen.
- **Unarchive ordering:** if multiple changes are passed, unarchive happens in the order specified on the command line. If any unarchive fails (no matching archive entry, destination collision), subsequent unarchives still attempt; the process exits non-zero at the end with a summary of failures.
- **Post-rewind state:** after a successful rewind, the workspace's `<agent_branch>` no longer exists locally (and remotely if `--hard`), the unarchived change directories are present in `<workspace>/openspec/changes/`, and the workspace's checkout is on `<base_branch>` at its current `HEAD`.

## Risks / Trade-offs

- **Risk:** A user types the wrong change name and rewinds the wrong work.
  - **Mitigation:** Soft rewind requires `[y/N]` confirmation listing the changes. Hard rewind is explicitly opt-in via `--hard`. Archived directories are not deleted; if the wrong rewind happens, the user can re-archive manually.
- **Risk:** A `--repo` selector ambiguously matches multiple repos (e.g. two repos with the same basename across different orgs).
  - **Mitigation:** The orchestrator detects ambiguity at startup and exits with a clear error listing all matches. The user must use the full URL to disambiguate.
- **Risk:** Hard rewind destroys remote branch work that another developer pushed concurrently.
  - **Mitigation:** Hard rewind is by design a destructive operation. The README documents this and recommends against it for shared agent branches. The orchestrator's normal `--force-with-lease` push semantics already protect against accidental overwrite during normal operation; rewind is a different code path because the user has explicitly asked to destroy.
