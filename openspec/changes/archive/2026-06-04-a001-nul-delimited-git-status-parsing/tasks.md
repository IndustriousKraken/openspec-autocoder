# Implementation tasks

## 1. Add the shared NUL-delimited status helper

- [x] 1.1 In `autocoder/src/git.rs`, add `pub struct StatusEntry { pub staged: char, pub worktree: char, pub path: String, pub orig_path: Option<String> }`.
- [x] 1.2 Add `pub fn status_entries(workspace: &Path) -> Result<Vec<StatusEntry>>` that runs `git status -z --porcelain --untracked-files=all` and parses the NUL-delimited output:
  - Split the raw stdout on the NUL byte (`\0`). Do NOT `.trim()` the whole output (it would strip the leading status-space of the first record); drop only a trailing empty token.
  - For each record: `staged = chars[0]`, `worktree = chars[1]`, index 2 is a space, `path = record[3..]`.
  - For rename/copy records (`staged` or `worktree` is `R` or `C`), the immediately-following NUL token is the original path → `orig_path = Some(that token)`; advance the iterator past it.
  - Skip empty records.
- [x] 1.3 Change `status_porcelain` AND `status_porcelain_untracked_all` (`git.rs:~482/~494`) from `.trim()` to `.trim_end()` so the leading status-space of the first record is preserved for any remaining string-returning caller.

## 2. Migrate all call sites onto the helper

- [x] 2.1 `autocoder/src/changelog_triage.rs` — DELETE `extract_porcelain_path` (~397) AND its test (~790). Both out-of-scope checks (~257, ~666) build `changed` from `status_entries(workspace)?.into_iter().map(|e| e.path)`.
- [x] 2.2 `autocoder/src/polling_loop.rs` — DELETE `extract_porcelain_path` (~6573) AND `triage_status_entries` (~6790); `discard_non_spec_writes` and any other caller use `status_entries`, reading `.path` and (where they classify staged vs untracked) the `staged`/`worktree` codes.
- [x] 2.3 `autocoder/src/polling/brownfield.rs` — DELETE `extract_porcelain_path` (~577); callers use `status_entries`.
- [x] 2.4 `autocoder/src/audits/scheduler.rs` — DELETE `extract_porcelain_path` (~708); callers use `status_entries`.
- [x] 2.5 Confirm no `extract_porcelain_path` / `triage_status_entries` definitions remain in the tree (grep). Status parsing has one source of truth.

## 3. Tests (behavior, against synthetic git states)

- [x] 3.1 In a temp git repo with exactly one worktree-modified tracked file at `openspec/changes/archive/<slug>/proposal.md`, `status_entries` returns one entry whose `path` is the full path (no leading char dropped), `staged == ' '`, `worktree == 'M'`. (Pins the changelog regression.)
- [x] 3.2 A path containing a space (e.g. `dir with spaces/note.md`) parses to the literal path — no surrounding quotes, no truncation.
- [x] 3.3 A staged rename `old.md` → `new.md` yields `path == "new.md"`, `orig_path == Some("old.md")`.
- [x] 3.4 A staged new file `src/new.rs` yields `staged == 'A'` AND `path == "src/new.rs"`.
- [x] 3.5 End-to-end: the changelog out-of-scope check accepts a modified `openspec/changes/archive/<slug>/proposal.md` (i.e. `is_in_scope` receives the intact path and returns true; the diff is NOT rejected).

## 4. Spec delta

- [x] 4.1 `specs/git-workflow-manager/spec.md` — ADD `Working-tree status parsing uses a single NUL-delimited porcelain helper`.

## 5. Acceptance gate

- [x] 5.1 `cargo test` passes for the autocoder crate.
- [x] 5.2 `cargo clippy --all-targets -- -D warnings` is clean.
- [x] 5.3 `openspec validate a001-nul-delimited-git-status-parsing --strict` passes.
