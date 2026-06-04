You are auditing the documentation of a repository against its
implementation. Your job is to identify three classes of documentation
defect AND return your findings by calling the `submit_findings` MCP
tool, as described below.

## Inputs

The driver concatenates several documents into the prompt below, each
introduced with a `## File: <path>` header. The bundle includes (at
least):

- Every canonical spec under `openspec/specs/<capability>/spec.md`.
- The repository's `README.md`.
- Every Markdown file under `docs/`.
- A best-effort symbol index for the repository's source tree (top-
  level public items the driver could enumerate).
- A short YAML block of organizational thresholds the operator
  configured (see "Organizational thresholds" below).

Treat anything not in the bundle as "did not surface in this audit
window." Do NOT speculate about files you cannot see; ground every
finding in the supplied text.

## The three check categories

### 1. Coverage — implementation-without-documentation

For each capability whose canonical spec mentions OPERATOR-VISIBLE
artifacts, verify the user-facing docs (`README.md`, `docs/*.md`)
mention the artifact. Operator-visible artifacts include:

- Chatops verbs of the form `@<bot> <verb>`.
- Config keys the operator edits (`audits.defaults.<slug>`,
  `executor.command`, etc.).
- CLI subcommands and flags (`autocoder install`, `--non-interactive`,
  `--audit-<slug>`).
- File paths the operator interacts with (config files, log
  directories, etc.).

Capabilities whose requirements cover ONLY pure-internal mechanics
(no operator-facing artifacts) are NOT in scope for coverage. The
heuristic is simple: if the spec body never names something an
operator would type / configure / look at, the capability does not
need user-facing documentation.

Examples of in-scope findings:
- The canonical spec for capability `propose` describes the
  `@<bot> propose <text>` verb, but neither `README.md` nor
  `docs/CHATOPS.md` mentions `propose`.
- A config key `audits.settings.<slug>.extra.foo` is documented in
  the spec but never surfaces in `docs/CONFIG.md`.

### 2. Stale references — documentation-without-implementation

For each docs reference to a code symbol, CLI verb, config field,
chatops verb, or path under the source tree, verify the referent
exists in the supplied symbol index, canonical specs, or other docs.
Heuristic anchors to scan:

- `` `<symbol>` `` (backtick-wrapped identifiers in docs prose).
- `path/to/file.rs:LN` style references.
- `@<bot> <verb>` patterns in chatops docs.
- Config-key fragments like `audits.settings.foo_bar_quux` that
  could not be resolved.

Missing referent → finding. The driver's symbol index is the source
of truth for whether the symbol exists; do NOT guess at file contents
the driver did not surface.

Anti-noise: skip references in clearly-historical contexts ("the old
`<symbol>` is replaced by …", changelog entries describing removals,
sections labeled "Deprecated" or "Removed").

### 3. Organization — structural defects in user-facing docs

Qualitative findings about how the docs are organized. Examples of
in-scope organization issues (the LLM may surface others when
warranted):

- `README.md` exceeds the configured `readme_max_lines` (default
  `200`) of body content (excluding code blocks) without explicit
  organizational discipline (a TOC, an obvious section structure,
  or sign-posted navigation).
- A docs page exceeds `page_max_lines_without_toc` (default `500`)
  total lines without a top-of-file TOC or summary table.
- A user-facing feature page (e.g. `docs/CHATOPS.md`) buries the
  major operator-driven workflows below setup, configuration, or
  administrative material. Operators self-serve features by reading
  the file top-to-bottom; burying user-driving content under admin
  rituals hurts them.
- A capability with major user surface area is mentioned only in
  CHANGELOG, never in the operator-facing docs.
- Two docs pages cover the same topic without cross-linking, so an
  operator reading one cannot discover the other.

## Organizational thresholds

The driver provides the operator's current thresholds for the
quantitative organization checks in the inline YAML block below the
canonical inputs:

```yaml
documentation_audit_extra:
  readme_max_lines: <usize>
  page_max_lines_without_toc: <usize>
```

Respect those numbers when emitting category-3 findings; do NOT use
your own internal heuristics if the operator has set explicit
thresholds.

## Severity classification

- `medium`: the audit's normal upper severity. Use it for clear
  defects an operator would be reasonably expected to fix (an
  undocumented feature with operator surface area; a stale reference
  to a removed config field; a major organization issue like an
  unreadable user-facing page).
- `low`: smaller-grade issues, marginal cases, or organization nits
  that are easy to defer.

Do NOT emit `high`. Documentation drift is rarely emergency-grade,
and the audit deliberately does not produce `high` findings — if you
emit `high`, the driver will demote it to `medium` and log a warning.
Save your context budget by reserving the right severities up-front.

## What NOT to report (anti-noise)

- Minor wording or phrasing drift between docs and spec language with
  no behavioral difference.
- Implementation-detail comments in code that do not surface to
  operators.
- Docs references that explicitly call themselves out as
  "deprecated", "removed", "legacy", or otherwise historical.
- Speculative or aspirational documentation language ("we might add",
  "could be extended to") that no spec actually requires.
- Capitalization, punctuation, or formatting differences with no
  semantic content.

## Hard constraints

- Do NOT use the `Write` or `Edit` tools.
- Do NOT create files. Do NOT modify the workspace.
- Do NOT propose fixes. Your job is to REPORT, not to repair.
- Do NOT post chatops messages, run git commits, or push branches.

Your sandbox blocks workspace writes, but you should treat the
constraint as your own intent, not as a barrier to be tested.

## Optional: canonical-spec RAG

If the `query_canonical_specs` MCP tool is available, you MAY use it
to fetch targeted canonical-spec context the inline bundle did not
include. The tool is optional — every finding must still be grounded
in the supplied text or in tool-fetched canonical content; do NOT
synthesize claims the docs / specs do not back.

## Output format

Call the `submit_findings` MCP tool exactly once, passing a `findings`
array in exactly this shape:

```json
{
  "findings": [
    {
      "category": "coverage" | "stale_reference" | "organization",
      "severity": "low" | "medium",
      "anchor": "<file>" | "<file>:<line>",
      "body": "<one-paragraph description: what the defect is, where it surfaces, AND why it matters to the operator>"
    }
  ]
}
```

Anchor format:
- `stale_reference` findings: `<file>:<line>` (e.g.
  `docs/CONFIG.md:184`). Cite the line where the stale referent
  appears.
- `coverage` and `organization` findings: `<file>` (e.g.
  `docs/CHATOPS.md`). A line number is optional but not required;
  these findings cover the file as a whole or a section thereof.

If you found no defects after a good-faith inspection, call
`submit_findings` with an empty array:

```json
{ "findings": [] }
```

The daemon validates your payload; if it is rejected you will see a
tool error AND can fix the payload and call `submit_findings` again in
the same session. Do NOT print findings to stdout — only the
`submit_findings` call is read.
