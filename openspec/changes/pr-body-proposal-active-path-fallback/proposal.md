## Why

The polling iteration's PR-body assembly (`polling_loop.rs::read_archived_why`) reads each change's `proposal.md` from `openspec/changes/archive/*-<change>/proposal.md` to populate the PR description's per-change `## Why` excerpt. If a change was not archived during the iteration that produced the PR — because `queue::archive` silently skipped, because the implementer agent never invoked archive, or because of a race the autocoder didn't anticipate — the lookup misses and the PR body renders `_(no proposal.md available)_` for that change.

A real-world example: a PR contained two changes whose proposal.md files were sitting at `openspec/changes/<slug>/proposal.md` (active path) because the iteration's archive step silently failed (separately specced as `queue-archive-aborted-detection`). The PR body said "no proposal.md available" for both changes despite the proposal files being present at the active path in the same commit the PR was reviewing. The human reviewer lost the spec's intent — the most useful context the PR body normally provides — to a cosmetic miss caused by an unrelated upstream bug.

The fix is small: when the archive-path lookup misses, fall back to the active path. If `openspec/changes/<change>/proposal.md` exists, read its `## Why` section the same way `read_archived_why` reads the archived one. Log a WARN when the fallback fires so operators see "this PR contains changes that were not archived in-iteration" as a yellow flag — that condition usually means a separate bug (`queue::archive` aborted), and surfacing it in the daemon's logs gives operators a hook to investigate.

This change does not paper over the archive-failure bugs. It makes the PR body honest about what's in the commit when those bugs (or any future similar gap) cause changes to land unarchived. Operators reviewing the PR see the intent text; operators reading the daemon logs see the WARN that flags the underlying issue.

## What Changes

**`read_archived_why` becomes `read_change_why` with a two-step lookup.** Step 1 is the existing archive-path lookup. Step 2 is a fallback to the active path. The function name change reflects the broadened scope (it no longer reads only from archive). The single-caller refactor at `polling_loop.rs::build_pr_body` updates the call site.

**Fallback semantics:**

1. Try `openspec/changes/archive/*-<change>/proposal.md` (lexicographically last match, current behaviour).
2. On miss, try `openspec/changes/<change>/proposal.md`.
3. On match in either step, parse and return the `## Why` section.
4. On miss in both steps, return `None` (the existing `_(no proposal.md available)_` fallback in `build_pr_body` continues to render).

**WARN log when fallback fires.** When step 2 succeeds (the archive miss + active-path hit), emit:

```
WARN polling_loop: change `<slug>` proposal read from active path, not archive — likely indicates an upstream archive failure for this iteration
```

The WARN is intentionally per-change rather than per-iteration so operators can see in `journalctl` which change(s) triggered the fallback. Operators correlating with the chatops PR-opened notification can spot "PR mentions 2 changes; 2 WARN lines about active-path fallback" and know to investigate the archive path.

**No change to the PR-body format otherwise.** The `_(no proposal.md available)_` text remains for the all-misses case (proposal genuinely not on disk at either path). Operators see the same body shape; the only difference is that the previously-empty case where the active path had the file is now populated.

## Impact

- **Affected specs:** `orchestrator-cli` — one ADDED requirement codifying the fallback contract and the WARN-log behavior.
- **Affected code:**
  - `autocoder/src/polling_loop.rs` — rename `read_archived_why` → `read_change_why`, extend it with the active-path fallback, add the WARN log. Update the one caller (`build_pr_body`).
  - Tests:
    - Fixture with archive path populated → existing behaviour: returns `Some(why)`, no WARN.
    - Fixture with archive path missing + active path populated → returns `Some(why)`, emits one WARN naming the change. Use `tracing-test` (already a dev-dependency) to capture the WARN.
    - Fixture with both paths missing → returns `None`, no WARN (operators don't need WARNs for genuinely-missing files).
    - Fixture with both paths populated → archive path wins (the existing behaviour). No WARN.
    - Fixture where active path's `proposal.md` is malformed (no `## Why` section) → returns `None`. The fallback finding a file but no `## Why` is treated identically to the archive case finding a file but no `## Why`.

- **Operator-visible behavior:** PR bodies for changes that didn't archive in-iteration now contain the proposal's `## Why` text instead of `_(no proposal.md available)_`. The daemon's WARN log gains a line per such change.
- **Breaking:** no. The function is module-internal (renamed but with the same call signature aside from name). The PR body format is unchanged for already-working cases.
- **Acceptance:** `cargo test` passes (new + existing). A polling iteration that produces a PR containing a change whose proposal sits at the active path renders the PR body with that change's `## Why` excerpt; the daemon's logs contain one WARN line naming the change.
