## Why

The `architecture_brightline` audit's "duplicate signature across files" check is structural: any function or method with the same signature appearing in N+ files trips the threshold. The check has no concept of intentional duplication. Real cases where duplication is deliberate:

- Example/demo sites that intentionally mirror the production API surface (each example client implements the same auth contract).
- Generated code that produces identical scaffolding per entity (e.g., serde derive output, ORM model code).
- Multiple integration test harnesses for different platform targets implementing the same protocol.
- Polyglot codebases where similar-looking but semantically-distinct functions live in different language modules.

Currently brightline reports these every run, AND operators have no way to silence them short of suppressing brightline entirely. Suppressing the audit defeats the value (it would also stop reporting unintentional duplication). The fix is a per-workspace ignore file the audit honors AND the `send it` LLM can populate when classifying duplications as intentional.

## What Changes

**New per-workspace file `.brightline-ignore` at the workspace root.** YAML format with one entry per intentional duplication:

```yaml
ignore:
  - file: examples/site-a/auth.ts
    function: handleAuthCallback
    signature_match: "async function handleAuthCallback(req"
    reason: "All example sites implement the same auth contract; intentional"
  - file: examples/site-b/auth.ts
    function: handleAuthCallback
    signature_match: "async function handleAuthCallback(req"
    reason: "All example sites implement the same auth contract; intentional"
  - file: examples/site-c/auth.ts
    function: handleAuthCallback
    signature_match: "async function handleAuthCallback(req"
    reason: "All example sites implement the same auth contract; intentional"
```

**Anchors are file + function + signature_match, NEVER line numbers.** Line numbers shift on every code edit; anchoring on them would make every entry rot within days. The current shape:

- `file` (required): workspace-relative path. Exact match.
- `function` (required): function or method name. Exact match.
- `signature_match` (required): a substring that must appear in the function's signature line. Allows partial matching for tolerant survival across cosmetic edits (e.g., parameter renames).
- `reason` (required): one-line operator-readable explanation. The audit treats an entry with no `reason` as malformed AND warns.

An entry matches a duplicate-signature finding when all three primary fields (file, function, signature_match) match. The audit suppresses any finding whose every constituent site matches an ignore entry. A finding involving 3 files where 2 match ignores AND 1 doesn't is reported (the 1 unmatched site is a new occurrence worth surfacing).

**Audit-time stale-entry pruning.** Each brightline run validates every entry: does the named file still exist? Does it contain a function with the named name? Does the signature_match still appear? Entries whose target is gone are pruned with a one-line chatops `📐 cleaned <N> stale brightline-ignore entries` notification AND a corresponding entry in the audit-run log naming each pruned ignore. The pruning happens to the in-memory copy used for THIS run; the on-disk file is rewritten when the audit's `WritePolicy::OpenSpecOnly` permission OR a future ignored-write extension allows it.

**Pruning needs a write-policy update.** Brightline today declares `WritePolicy::None`. To prune `.brightline-ignore` in place, the audit needs to write to that one specific file. Two options: (a) extend brightline's WritePolicy to `OpenSpecOnly` AND allow `.brightline-ignore` as a special case; (b) leave brightline at `None` AND have the chatops notification name the stale entries so the operator removes them manually. Option (b) is simpler AND chosen for this spec. Pruning is purely informational until an operator commits the cleanup.

**`send it` populates the ignore file.** When an operator runs `@<bot> send it` on a brightline-finding thread AND the LLM triage classifies a duplicate-signature finding as intentional (not worth fixing), the LLM's output diff SHALL include an update to `.brightline-ignore` adding one entry per constituent site of the finding. Each entry's `reason` field is populated by the LLM from its judgment (operators can revise via the standard PR-comment revision loop if the reason is off).

**Prompt update.** A new section in `prompts/audit-reply-acts.md` (the `send it` triage prompt) describes the `.brightline-ignore` shape AND when to populate it. The LLM's classification expands from "quick fix vs. spec-worthy" to "quick fix vs. spec-worthy vs. mark as intentional duplication."

**No global ignore.** The file is per-workspace, not global. Different repos have different intentional-duplication patterns; a global ignore would either be too permissive (suppress real findings on repo A because they look like intentional patterns from repo B) OR too restrictive (force every repo to share one canonical pattern).

## Impact

- **Affected specs:**
  - `orchestrator-cli` — one MODIFIED requirement: the existing brightline-audit requirement gains the `.brightline-ignore` loading + match-suppression + stale-entry pruning behaviors.
  - `chatops-manager` — one MODIFIED requirement: the brightline-finding chatops notification's body MAY include the `cleaned <N> stale ignore entries` line when pruning happened in the same run.
  - `project-documentation` — one ADDED requirement: `OPERATIONS.md describes the .brightline-ignore file format AND the send-it LLM's role in populating it`.
- **Affected code:**
  - `autocoder/src/audits/brightline.rs` (or wherever the brightline audit lives) — load `.brightline-ignore` from workspace root via the workspace-path helper. Apply match-suppression to each duplicate-signature finding before emitting. Validate every loaded entry against the current workspace state; collect a list of stale entries.
  - The brightline finding emission gains a "cleaned <N> stale ignore entries" line in the chatops top-line when stale-pruning is non-empty.
  - `prompts/audit-reply-acts.md` — extend the prompt with the "mark as intentional" output path AND the YAML shape for `.brightline-ignore` entries.
  - `docs/OPERATIONS.md` — add a `.brightline-ignore` subsection under the `architecture_brightline` audit's description.
- **Operator-visible behavior:**
  - Operators with intentionally-duplicated code (example sites, generated scaffolding) stop seeing brightline reports about it after running `@<bot> send it` once AND letting the LLM populate the ignore file.
  - Future runs honor the ignore.
  - When files containing ignored entries are deleted/renamed, the audit notes the cleanup in chatops so the operator can rm the stale entries.
- **Breaking:** no. A workspace without `.brightline-ignore` behaves exactly as today (no entries to match; no suppression; no pruning).
- **Acceptance:** `cargo test` passes; `openspec validate a15-brightline-ignore-with-llm-maintenance --strict` passes. Unit tests cover: an ignore entry suppresses a matching finding; a partial-match (only some sites ignored) is still reported with the unmatched sites; stale entries (file gone) are detected AND the cleanup chatops line fires.
