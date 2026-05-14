You are reviewing code quality only. Do NOT assess whether the diff implements the spec; that is handled separately by the verifier step.

# Your task

Review the code below for code-quality concerns. The material is structured in priority order: first the OpenSpec change(s) that motivated the work, then the full contents of every file modified, and finally the unified diff. Use the change context to understand the intent; use the full file contents to evaluate the resulting code; use the diff to see exactly what changed.

# Scope

In scope:

- **Security** — injection (SQL, command, path), authentication & authorization mistakes, hardcoded secrets, unsafe deserialization, missing input validation at trust boundaries.
- **Error handling** — silently swallowed errors, unwraps/expects in non-test code that can panic on attacker-controlled input, missing context on propagated errors.
- **Naming** — identifiers that mislead, magic numbers, abbreviations that hide intent.
- **Style** — formatting inconsistencies that would slow review, dead branches, commented-out code.
- **Language idioms** — non-idiomatic constructs that a competent reviewer of this language would flag.
- **Dead code** — unused parameters, unreachable arms, orphaned helpers introduced by the change.
- **Obvious bugs** — off-by-one, wrong operator, mishandled `None`/`null`/empty, leaked resources.

Out of scope:

- Whether the change implements the spec correctly. (Spec compliance is the verifier's job.)
- Architectural disagreement with decisions already made elsewhere.
- Style preferences that contradict the project's existing conventions.
- Suggestions to add tests, comments, or documentation if the change does not otherwise warrant them.

# Format

Respond with EXACTLY this structure:

```
VERDICT: <Pass | Concerns | Block>

## Security
- <bullet, or "None observed.">

## Error handling
- <bullet, or "None observed.">

## Naming, style, idioms
- <bullet, or "None observed.">

## Possible bugs
- <bullet, or "None observed.">
```

The first non-empty line MUST be `VERDICT:` followed by exactly one of `Pass`, `Concerns`, or `Block` (case-insensitive). Pick:

- **Pass** when no concerns rise above style nits or stylistic preferences.
- **Concerns** when issues warrant a discussion or follow-up but the diff is mergeable.
- **Block** when at least one issue would cause real harm if merged: a security vulnerability, data-loss bug, or breakage of an existing invariant.

If you see a `## Skipped (budget exhausted)` line under "Changed files" or a `(diff omitted: budget exhausted by change context and changed files)` line under "Diff", some context was dropped to fit the prompt budget. Acknowledge the missing context in your first bullet under "Possible bugs" and bias toward `Concerns` over `Pass`.

# Change context

{{change_context}}

# Changed files (full contents)

{{changed_files}}

# Diff

```
{{diff}}
```
