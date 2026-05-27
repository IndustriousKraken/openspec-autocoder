## Why

Three failure modes have surfaced during initial production deployment.
All three force an operator to intervene on what should be unattended
behavior.

**1. The executor archives changes instead of implementing them.**
Faced with a complex change, Claude has been observed responding by
simply renaming the change directory into `openspec/changes/archive/<date>-<name>/`
— a one-rename "shortcut" that satisfies any "are we done?" heuristic
without producing any actual source-code modifications. Blocking the
`openspec archive` CLI doesn't help because the equivalent action is a
plain `git mv`, which the sandbox cannot reasonably restrict.

**2. A dirty workspace silently disables the repository.** When a
repository's workspace at `/tmp/workspaces/<derived-path>` has any
uncommitted changes at startup, autocoder skips the repository for
the entire process lifetime. The behavior was conservative protection
against "human edited the workspace manually" — except the workspace
is daemon-owned ephemeral state, and dirty content is residue from a
prior failed iteration, not human intent. The operator has to SSH in
and reset before the daemon can recover.

**3. `openspec` itself is not documented as a host prerequisite.** The
executor calls `openspec instructions apply --change <name>` to build
its prompt. Without `openspec` on PATH, this command silently fails
and the prompt falls back to raw concatenation of the change's
`proposal.md` / `design.md` / `tasks.md`. The fallback works but
provides less explicit guidance to the agent — which may be
contributing to failure mode #1.

This change closes all three:

- After the executor returns, if the working-tree diff consists
  *only* of renames into `openspec/changes/archive/<date>-<name>/`,
  the daemon overrides the executor's outcome to Failed with reason
  "agent archived without implementing", reverts the staged moves,
  and leaves the change pending for the next iteration. The detection
  is structural — pattern-matching on the rename targets — not
  dependent on which command produced the moves.
- The startup dirty-workspace check now attempts recovery (`git
  checkout <base>`, `git reset --hard origin/<base>`, `git clean
  -fd`) before falling back to "skip for the process lifetime."
- The default sandbox denylist adds `openspec archive:*` and
  `openspec unarchive:*` as defense-in-depth — cheap to include,
  closes the obvious-CLI path even though the structural detection
  is what does the real work.
- README's Deployment section gains an "install openspec" step
  alongside Claude auth setup.

## What Changes

- New `polling_loop::detect_lazy_archive(workspace) -> bool` helper
  that returns true when `git status --porcelain` is non-empty AND
  every entry is a rename whose destination path starts with
  `openspec/changes/archive/`. Used after the executor returns
  Completed.

- `polling_loop::handle_outcome` (or the equivalent call site that
  inspects the executor's outcome) gains a branch: when the outcome
  is Completed AND `detect_lazy_archive` returns true, the daemon:
  - Reverts the staged changes via `git reset --hard HEAD` (the
    repo's state before the executor ran).
  - Treats the outcome as `Failed { reason: "agent appears to have
    archived without implementing the change" }`.
  - Logs a `warn`-level line naming the change.
  - The existing Failed-handling code path unlocks the change so
    the next iteration can retry.

- `repo_passes_startup_check` in `src/cli/run.rs` no longer treats
  dirty workspace as terminal. New flow:
  1. Detect dirty via `git status --porcelain`.
  2. Log a `warn`-level line naming the entry count.
  3. Run recovery: `git checkout <repo.base_branch>` (off any
     stale agent branch), `git reset --hard
     origin/<repo.base_branch>`, `git clean -fd`.
  4. Re-check porcelain. If now clean, log `info` "workspace
     recovered" and return `true`. If still dirty, fall back to
     the existing "skip for the process lifetime" error.

- Extend `default_disallowed_bash_patterns` in `src/config.rs` with
  `"openspec archive:*"` and `"openspec unarchive:*"`. Defense in
  depth.

- README updates:
  - Deployment §2 gains an `openspec` install step (npm or curl
    depending on the operator's tooling).
  - AI Security §8 (executor sandbox) adds a paragraph describing
    the structural lazy-archive detection and noting that the
    bash-pattern denials are belt-and-suspenders.

## Capabilities

### Modified Capabilities

- `executor`: the safe-default sandbox denylist gains two patterns
  blocking `openspec archive` and `openspec unarchive` invocations.
- `orchestrator-cli`: startup dirty-workspace check attempts
  automatic recovery before skipping. Post-execution check detects
  archive-only diffs and treats them as Failed.

## Impact

Three unattended-failure modes recover automatically. Lazy archives
are caught structurally regardless of which command produced them;
dirty workspaces self-heal; openspec installation is documented as a
deployment prerequisite.

The "I genuinely want to archive this change as part of the
implementation" case is now prevented. This is intentional: per
project convention, `openspec archive` is the daemon's job after PR
merge, not the executor's job inside an iteration. If a future
change really does need to mutate openspec state as part of its
implementation (unlikely), that would be a separate spec.

The dirty-workspace recovery is destructive — uncommitted state is
discarded. This matches the operator's stated intent: "I'm
definitely not sitting around logged into the server all day."
Operators who want to inspect a workspace before any daemon action
should stop the systemd unit first
(`sudo systemctl stop autocoder`) before manipulating
`/tmp/workspaces/`.
