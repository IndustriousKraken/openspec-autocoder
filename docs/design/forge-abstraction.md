# Forge abstraction (GitHub / GitLab / GHE) — design

**Status:** converged design, not yet broken into change proposals. Captures the decisions for making the forge (GitHub today) a swappable provider so self-hosted GitLab — and GitHub Enterprise — become first-class.

## Motivation

A user already runs autocoder against a private GitLab instance, and self-hosted GitLab is concentrated where it matters for this tool: security/pentest shops whose tooling gets flagged by GitHub scanners, air-gapped or compliance-controlled infra, and operators who keep their own code on their own servers. The core loop already works there *by accident* — the git half (clone / fetch / branch / commit / push) uses the raw URL and the `origin` remote, so it is host-neutral. What does NOT work is everything routed through `github.rs`: `parse_repo_url` literally rejects non-GitHub URLs (test `parse_url_rejects_non_github`), and the REST calls are GitHub-shaped (`/repos/{owner}/{repo}/pulls`). So a GitLab user today gets autonomous implementation + a pushed agent branch (especially under `auto_submit_pr: false`), and opens the MR by hand.

Making the forge swappable turns that into first-class support — and is the same trait-with-providers shape already used for `CliStrategy` (claude / opencode / gemini).

## The abstraction

A `Forge` trait with provider implementations, selected per-repo. **The git half does not change** — only the API layer (`github.rs` and its call sites) moves behind the trait.

```
                          Forge (trait)
   parse_repo(url) -> (host, project)        open_pr / list_open_prs / find_pr_by_head
   set_pr_draft (+ label fallback)           list_comments_since / post_comment
   post_review(verdict)                      create_fork        branch_url(...)
   authorize(commenter) -> AuthLevel         (later) issues: list / read / comment
            ▲                                                ▲
   ┌──────────────────────┐                        ┌──────────────────────┐
   │     GithubForge      │                        │     GitlabForge      │
   │ /repos/{o}/{r}/pulls │                        │ /projects/:id/merge_requests
   │ author_association   │                        │ member access_level  │
   │ (today's github.rs)  │                        │ (new)                │
   └──────────────────────┘                        └──────────────────────┘
       selected per-repo by URL host (+ a forge config: host / api_base / token)
       git operations (clone/fetch/push) are unchanged — already host-neutral
```

### Trait surface (everything GitHub-coupled today)

- **`parse_repo(url) -> (host, project)`** — replaces the github-only `parse_repo_url`; host-generic (extracts the host and the owner/group/project path).
- **PR/MR lifecycle** — `open_pr`, `list_open_prs`, `find_pr_by_head`, `set_pr_draft` (with the `do-not-merge` label fallback on hosts that reject drafts).
- **Comments** — `list_comments_since`, `post_comment`. The revision loop, the reviewer's posted comments, and the `@<bot> revise` / `code-review` triggers all ride this.
- **Reviews** — `post_review(verdict)` (approve / request-changes / comment).
- **Fork** — `create_fork` (fork-PR mode).
- **`authorize(commenter)`** — the a000 gate. GitHub `author_association` (OWNER / MEMBER / COLLABORATOR) → GitLab member `access_level` (Owner / Maintainer / Developer).
- **`branch_url(...)`** — for the push-only / "branch pushed, no PR" chatops message; host-specific.
- **(later) issues** — list / read / comment, for the issues-fix lane's ingestion side.

## Phasing

The surface is broad, so phase it rather than boil the ocean:

1. **Extract `GithubForge` behind the trait** — a behavior-preserving refactor: every current GitHub call moves behind `Forge`, no GitLab yet, verified by the existing tests. This is the load-bearing, get-it-right step; nothing user-visible changes.
2. **`GitlabForge` for the daily loop** — MR create/list/find, comments, reviews, and the `authorize` mapping. Makes GitLab first-class for auto-MR + reviewer + revision loop + authorized triggers.
3. **Fork mode + the issues-lane forge side** — later, when fork-PR-on-GitLab or the issues lane needs it.

## Selection and config

- **Auto-detect by URL host:** `github.com` → `GithubForge`; a configured GitLab host → `GitlabForge`. Existing GitHub configs need no change.
- **Per-repo `forge` config** carrying host / API base / token, so a self-hosted GitLab (`gitlab.company.com`) is reachable. The same configurable API base **also unlocks GitHub Enterprise** (self-hosted GHE, GitHub shape) for free.
- The push-only manual hint becomes forge-specific: `gh pr create` → `glab mr create` (or a plain web URL).

## Interactions

- **a000 (external-trigger authorization):** the `authorize(commenter)` method is where the author_association gate generalizes to GitLab access levels. a000's GitHub-only gate becomes the GithubForge implementation; GitlabForge supplies the access-level equivalent.
- **Issues-fix lane:** the issues ingestion (hybrid triage of public issues) reads the forge's issues API — Phase 3 gives it a GitLab implementation through the same trait.
- **`CliStrategy` parallel:** same trait-with-providers pattern already in use; the forge is the second axis of swappability (which CLI drives the agent × which forge hosts the code).

## Open decisions (settle before Phase 2)

- **Authorize mapping:** which GitLab access levels count as authorized for the a000 gate — Owner + Maintainer only, or also Developer?
- **v1 scope:** Phase 1 + 2 (the daily loop); defer fork mode and issues to Phase 3.
- **Config shape:** a per-repo `forge:` block (kind / host / api_base / token route) vs. inferring everything from the URL host plus the existing token routing. Lean: infer kind+host from the URL, carry only api_base/token-route in config when non-default.
- **Draft/label semantics on GitLab:** GitLab uses "Draft:" MR title prefix / WIP rather than a draft flag — map `set_pr_draft` onto that.
