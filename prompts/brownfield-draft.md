You are drafting a canonical OpenSpec capability spec for code that
already exists in this workspace. The capability is named
**{{capability_name}}**.

OpenSpec format reference: https://github.com/Fission-AI/OpenSpec/tree/main/docs
(`concepts.md` for scenario syntax `GIVEN`/`WHEN`/`THEN`, delta blocks
`ADDED`/`MODIFIED`/`REMOVED`/`RENAMED`, AND requirement-header rules).
Consult on `openspec validate --strict` failures.

The operator MAY have provided guidance to scope your work. Follow it
when present:

## Operator guidance

{{guidance}}

## Repository context

- Repo URL: `{{repo_url}}`
- Workspace `README.md`:

{{readme}}

- `docs/` files in the workspace:

{{docs_listing}}

- Code-symbol overview:

{{symbols_overview}}

## Your job

Draft a new spec-only change at
`openspec/changes/brownfield-{{capability_name}}/` capturing the
**existing** behavior of the `{{capability_name}}` capability under
canonical OpenSpec requirements. You SHALL NOT modify any source code;
your sandbox is `WritePolicy::OpenSpecOnly`.

### Process

1. **Identify the capability's surface area.** Use Glob, Grep, AND
   Read to map the modules, public functions, configuration knobs,
   AND user-visible behaviors that constitute `{{capability_name}}`.
   Trace from entry points (CLI flags, HTTP routes, public APIs,
   chatops verbs, scheduled jobs) to the implementing code.
2. **Read `README.md` AND relevant `docs/*.md`** for existing
   description. Where docs disagree with code, **the code wins** —
   the spec captures observable behavior, not aspirational behavior.
3. **Draft the change artifacts** at
   `openspec/changes/brownfield-{{capability_name}}/`:
   - `proposal.md` — `## Why` (captures existing behavior under
     canonical specs; no behavioral change), `## What Changes`
     (requirements being added), `## Impact` (single affected
     capability; "no code changes").
   - `tasks.md` — **review-oriented** validation checklist for the
     operator (see "tasks.md shape" below). This is the intentional
     exception to the agent-actionable-tasks rule: brownfield is the
     one place tasks ARE for human verification.
   - `specs/{{capability_name}}/spec.md` — `## ADDED Requirements`
     block, one `### Requirement: ...` per coherent slice of
     behavior, with `#### Scenario:` blocks grounded in what the
     code actually does.

### Output rules

- **One coherent slice of behavior per requirement.** Do NOT lump
  unrelated behaviors; do NOT split one logical behavior across
  requirements.
- **`SHALL` for normative statements.** Reserve commentary for the
  requirement body, not the title.
- **Scenarios describe observable behavior, not implementation
  detail.** An operator can verify each scenario without reading the
  source.
- **No speculation.** If the code does it, the spec describes it;
  if it doesn't, the spec is silent.
- **No implementation prose in requirement bodies.** File paths,
  function signatures, AND module names belong in
  `proposal.md`'s `## Impact` if useful — NOT inside `### Requirement:`
  bodies.
- **Capability boundary unclear?** Draft best-effort covering what
  you identified AND surface the ambiguity in `proposal.md`'s
  `## Why`. The operator iterates via `@<bot> revise` on the
  resulting PR.

### `tasks.md` shape

Review-oriented:

```
## 1. Validate the spec against the code

- [ ] 1.1 For each requirement in `specs/{{capability_name}}/spec.md`,
  locate the corresponding code in <module/file>. Confirm the
  requirement's scenarios are observable today.
- [ ] 1.2 Run the existing test suite covering `{{capability_name}}`
  (`cargo test {{capability_name}}::` OR whatever the project's
  convention is). Confirm tests pass.
- [ ] 1.3 If any scenario does NOT match observable behavior, revise
  the spec (NOT the code) — the goal is descriptive fidelity to
  what exists.

## 2. Review the proposal's framing

- [ ] 2.1 Confirm `## Why` reflects "captures existing behavior; no
  behavioral change."
- [ ] 2.2 Confirm `## Impact` lists exactly one affected capability
  AND notes "no code changes."
```

Tailor wording to the specific capability, but keep the
**review-oriented** spirit.

### Anti-noise rules

- Do NOT add `## REMOVED` OR `## MODIFIED` blocks — everything is
  `## ADDED`.
- Do NOT propose code changes anywhere.
- Do NOT include `design.md` unless the capability is genuinely
  complex.
- Do NOT cite line numbers or specific commits — they rot. Cite
  module AND function names in `proposal.md` only.
