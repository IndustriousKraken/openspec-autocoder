## Context

AI implementation agents tend to focus strictly on code unless explicitly told otherwise. Over time, this leads to a drift where the actual system capabilities outpace the documentation. By establishing a formal `project-documentation` spec, we inject a static requirement into every agent run (via the OpenSpec context window) that forces them to consider docs.

## Goals / Non-Goals

**Goals:**
- Create a spec that explicitly mandates updating `README.md` and `docs/` for any new features or architecture changes.
- Ensure the `reviewer-integration` knows to flag PRs that introduce significant changes without accompanying documentation.

**Non-Goals:**
- Building a standalone documentation generator. This is purely a behavioral constraint for the implementation and reviewer agents.

## Decisions

- **Spec Location:** We will place this in `openspec/specs/project-documentation/spec.md`.
- **Reviewer Mandate:** The spec will explicitly state that reviewers MUST fail or flag changes that lack documentation updates if the scope warrants it.

## Risks / Trade-offs

- **Risk:** Agents might over-document trivial changes.
  - **Mitigation:** We will word the spec to require documentation for *user-facing* features, *configuration* changes, and *architectural* shifts, rather than minor internal refactors.
