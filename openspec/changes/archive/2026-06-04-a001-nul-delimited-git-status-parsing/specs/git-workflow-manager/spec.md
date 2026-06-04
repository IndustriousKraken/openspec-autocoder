# git-workflow-manager — delta for a001-nul-delimited-git-status-parsing

## ADDED Requirements

### Requirement: Working-tree status parsing uses a single NUL-delimited porcelain helper
All working-tree status parsing in the daemon SHALL go through one helper, `git::status_entries(workspace) -> Result<Vec<StatusEntry>>`, which runs `git status -z --porcelain --untracked-files=all` AND parses the NUL-delimited output. `StatusEntry` SHALL carry the staged status code, the worktree status code, the path, AND an optional original path for rename/copy records. Per-module hand-sliced parsers (the `extract_porcelain_path` copies AND `triage_status_entries`) SHALL be removed; there is one source of truth for status parsing.

The parser SHALL obey these rules:

- Records are delimited by the NUL byte (`\0`), NOT by newlines. The raw output SHALL NOT be trimmed as a whole — doing so strips the leading staged-status space of the first record (a blank staged column for a worktree-modified file), which would decapitate that record's path.
- Within a record, the first two characters are the staged (X) AND worktree (Y) status codes; the third character is a space; the remainder is the path.
- For a rename or copy record (X or Y is `R` or `C`), the immediately-following NUL-delimited token is the original path, captured as `orig_path`.
- Because `-z` mode emits paths verbatim (no C-style quoting), paths containing spaces or special characters parse correctly without an unquoting step.

The helper exposing the staged AND worktree codes lets callers distinguish staged-new (`A `) and renamed (`R`) entries from untracked (`??`) and worktree-modified (` M`) ones, rather than collapsing them to a single untracked-or-not boolean.

#### Scenario: Worktree-modified first record keeps its full path
- **GIVEN** a working tree whose only change is a worktree-modified tracked file at `openspec/changes/archive/<slug>/proposal.md`
- **WHEN** `status_entries` parses the `git status -z --porcelain` output
- **THEN** it returns one entry whose `path` is exactly `openspec/changes/archive/<slug>/proposal.md` — no leading character dropped
- **AND** the entry's staged code is a space AND its worktree code is `M`

#### Scenario: Path containing spaces parses literally
- **GIVEN** an untracked file at `dir with spaces/note.md`
- **WHEN** `status_entries` parses the output
- **THEN** the entry's `path` is `dir with spaces/note.md` — no surrounding quote characters AND no truncation

#### Scenario: Rename record captures the original path
- **GIVEN** a staged rename from `old.md` to `new.md`
- **WHEN** `status_entries` parses the output
- **THEN** the entry's `path` is `new.md` AND its `orig_path` is `Some("old.md")`

#### Scenario: Staged-new file is distinguishable from untracked
- **GIVEN** a staged new file `src/new.rs` (status `A `)
- **WHEN** `status_entries` parses the output
- **THEN** the entry's `path` is `src/new.rs` AND its staged code is `A`
- **AND** a caller can tell it apart from an untracked (`??`) file

#### Scenario: Changelog scope check accepts a modified archive proposal end-to-end
- **GIVEN** the changelog flow's working tree has a modified `openspec/changes/archive/<slug>/proposal.md` (a legitimate `changelog:` frontmatter edit)
- **WHEN** the out-of-scope check builds its changed-path list from `status_entries`
- **THEN** the path reaches `is_in_scope` intact AND is accepted
- **AND** the diff is NOT refused as out-of-scope
