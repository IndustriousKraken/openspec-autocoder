## 1. `.brightline-ignore` schema

- [ ] 1.1 Define the YAML schema in `autocoder/src/audits/brightline/ignore.rs`:
  ```rust
  #[derive(Debug, Clone, Deserialize, Serialize)]
  pub struct BrightlineIgnoreFile {
      pub ignore: Vec<IgnoreEntry>,
  }
  #[derive(Debug, Clone, Deserialize, Serialize)]
  pub struct IgnoreEntry {
      pub file: PathBuf,
      pub function: String,
      pub signature_match: String,
      pub reason: String,
  }
  ```
- [ ] 1.2 Loading: read `<workspace_root>/.brightline-ignore` via serde_yaml. Missing file → empty list (no ignores). Malformed file → WARN log naming the parse error, treat as empty (no suppression).
- [ ] 1.3 Tests: load valid file; load empty file; load missing file; load malformed file (WARN fires).

## 2. Match-suppression

- [ ] 2.1 In the brightline audit's finding-emission path, BEFORE adding a duplicate-signature finding to the output, check whether every constituent site matches an `IgnoreEntry`. Match criteria:
  - `IgnoreEntry.file` exactly equals the site's file path (relative to workspace root).
  - `IgnoreEntry.function` exactly equals the site's function/method name.
  - `IgnoreEntry.signature_match` is a substring of the site's signature line.
- [ ] 2.2 Suppression rule: if ALL constituent sites of a finding match an ignore entry, suppress the finding entirely. If only SOME match, emit the finding with the matched sites omitted from the body (the operator sees only the unmatched site(s)).
- [ ] 2.3 Tests:
  - Finding with 3 sites, all 3 match ignores → suppressed.
  - Finding with 3 sites, 2 match → finding emitted with 1 site listed.
  - Finding with 3 sites, 0 match → finding emitted with all 3 listed (today's behavior).
  - Empty ignore list → no suppression occurs.

## 3. Stale-entry pruning (informational)

- [ ] 3.1 During each brightline run, after loading `.brightline-ignore`, validate every entry:
  - Does `entry.file` exist?
  - Does the file contain a function with name `entry.function`?
  - Does that function's signature contain `entry.signature_match`?
- [ ] 3.2 Collect a list of stale entries (any failed validation). The audit does NOT modify the on-disk file (brightline's `WritePolicy::None` is unchanged); the cleanup is informational only.
- [ ] 3.3 When the stale list is non-empty, the brightline chatops top-line gains a trailing clause:
  ```
  📐 architecture_brightline on <repo>: <N> file(s) over line threshold; <M> duplicate signature(s); <K> stale ignore entries to clean up
  ```
  AND the threaded body lists the stale entries with their `file + function + reason` so the operator knows what to remove.
- [ ] 3.4 Tests:
  - Ignore entry with non-existent file → marked stale.
  - Ignore entry with file present but function gone → marked stale.
  - Ignore entry with file + function present but signature_match no longer matches → marked stale.
  - Ignore entry with all three valid → NOT stale.

## 4. `send it` prompt update

- [ ] 4.1 In `prompts/audit-reply-acts.md` (the triage prompt for `send it`), add a section describing the `.brightline-ignore` shape AND when to populate it:
  > **For brightline duplicate-signature findings**: classify each finding as one of:
  > - **Fix** (extract to a shared helper, refactor to remove the duplication): proceed with the standard fix output path.
  > - **Spec-worthy** (the duplication signals a missing abstraction that needs design work): write a proposal under `openspec/changes/<slug>/`.
  > - **Mark as intentional**: add an entry to `.brightline-ignore` at the workspace root for EACH constituent site of the finding. Each entry includes `file`, `function`, `signature_match`, AND a one-line `reason` explaining why the duplication is deliberate.
  >
  > Use "Mark as intentional" when the duplication reflects a design choice (example sites mirroring an API, generated scaffolding, multi-platform protocol implementations) that fixing would actively harm.
- [ ] 4.2 The prompt template's YAML example shows the entry shape so the LLM emits well-formed entries.
- [ ] 4.3 The LLM's diff for "Mark as intentional" outputs touches ONLY `.brightline-ignore`. The triage handler's diff-scope validation (existing path) must permit `.brightline-ignore` as an allowed write target (alongside `openspec/changes/<slug>/`).
- [ ] 4.4 Tests (the triage handler): mocked LLM produces a diff that adds 3 entries to `.brightline-ignore` → handler accepts AND opens a PR. Diff touching `src/foo.rs` AND `.brightline-ignore` → handler rejects (only `.brightline-ignore` AND `openspec/changes/` are permitted).

## 5. Docs

- [ ] 5.1 In `docs/OPERATIONS.md`'s `architecture_brightline` audit description, add a `.brightline-ignore` subsection:
  - The file's purpose AND location (`<workspace_root>/.brightline-ignore`).
  - The YAML schema (file, function, signature_match, reason — all required).
  - The match-suppression behavior (all-sites-match → suppress entirely; partial match → emit with remaining sites only).
  - The stale-entry pruning (cleanup is informational; the operator removes stale entries manually).
  - The `send it` integration (the LLM populates entries when classifying findings as intentional).
- [ ] 5.2 Cross-link from the `send it` documentation in `docs/CHATOPS.md` to the `.brightline-ignore` subsection.

## 6. Spec deltas

- [ ] 6.1 `openspec/changes/a15-brightline-ignore-with-llm-maintenance/specs/orchestrator-cli/spec.md` MODIFIES the brightline-audit requirement to add the ignore-file loading + suppression + stale-pruning behaviors.
- [ ] 6.2 `openspec/changes/a15-brightline-ignore-with-llm-maintenance/specs/chatops-manager/spec.md` MODIFIES the brightline chatops-notification requirement to admit the stale-cleanup clause.
- [ ] 6.3 `openspec/changes/a15-brightline-ignore-with-llm-maintenance/specs/project-documentation/spec.md` ADDs the OPERATIONS.md AND CHATOPS.md cross-link.

## 7. Verification

- [ ] 7.1 `cargo test` passes (new + existing).
- [ ] 7.2 `openspec validate a15-brightline-ignore-with-llm-maintenance --strict` passes.
- [ ] 7.3 `cargo clippy --all-targets --all-features -- -D warnings` produces no new warnings.
