## 1. Config schema

- [x] 1.1 Extend `GithubConfig` in `src/config.rs` with `pub owner_tokens: Option<std::collections::HashMap<String, String>>`. Apply `#[serde(default)]` so an absent block parses to `None`.
- [x] 1.2 Keep `token_env: String` with its current default (`"GITHUB_TOKEN"`) for backward compatibility. The semantics of "is GITHUB_TOKEN required in the environment" become conditional on whether `owner_tokens` covers every configured repo (see section 3).
- [x] 1.3 Update `config.example.yaml` to show `owner_tokens` as a commented-out optional block underneath `token_env`, with an explanatory comment pointing at the README section.
- [x] 1.4 **Verify:** `cargo test config::tests::loads_with_owner_tokens` parses an example using the new block; `cargo test config::tests::owner_tokens_optional` confirms that an absent block leaves `owner_tokens` as `None`.

## 2. Token resolution

- [x] 2.1 Create `pub fn resolve_token(github_cfg: &GithubConfig, owner: &str) -> Result<String>` in a new `src/github_credentials.rs` module (kept separate from `src/github.rs` so the HTTP client and the credential resolver stay independently testable). The function MUST:
    - Lookup `owner` in `github_cfg.owner_tokens` (if `Some`) using case-insensitive key matching.
    - If matched: `std::env::var(env_name)`; on `Err`, return `Err(anyhow!("owner-token env var `{env_name}` for owner `{owner}` is not set"))`.
    - Otherwise: read `std::env::var(&github_cfg.token_env)`; on `Err`, return `Err(anyhow!("github token env var `{token_env}` is not set; no `owner_tokens` route for owner `{owner}`"))`.
- [x] 2.2 Unit tests in `src/github_credentials.rs` covering: owner match → returns that env's value; owner not matched → falls back to `token_env`; neither set → error message names both fallbacks; case-insensitive owner match (`My-Org` config key matches `my-org` URL owner).

## 3. Startup validation

- [x] 3.1 In `cli::run::execute`, after `workspace::detect_collisions(&cfg.repositories)?` and before spawning any polling task, iterate every repo. For each: parse the owner via `github::parse_repo_url`; call `resolve_token(&cfg.github, &owner)`. Collect failures into a single error message that lists every repo whose token cannot be resolved, then return that error (so the process exits non-zero before launching the daemon).
- [x] 3.2 For each successfully-resolved repo, emit one info-level log line of the form `repository `{url}` will use GitHub token from env var `{env_var_name}`` — names the env var, NOT the token value.
- [x] 3.3 **Verify:** add a test `cli::run::tests::startup_fails_when_no_token_route` that constructs a 2-repo config where one repo has no matching `owner_tokens` entry AND `token_env`'s named env var is unset. Confirm `repo_passes_startup_check` (or an equivalent helper) returns false / error with a message naming the unmappable owner.

## 4. PR-creation wiring

- [x] 4.1 In `polling_loop::open_pull_request`, replace the direct `std::env::var(&github_cfg.token_env)` call with `github_credentials::resolve_token(github_cfg, &owner)`. The `owner` is already parsed there from `github::parse_repo_url`.
- [x] 4.2 The existing 2 mockito-tested call sites in `github.rs` (PR creation + label fallback) are unaffected: they accept a `token: &str` parameter and don't care where it came from. No change to `github::create_pull_request` signature.
- [x] 4.3 **Verify:** `cargo test polling_loop::tests::askuser_on_pending_escalates_to_chatops` and the other existing pass-through tests continue to pass. Add a new test `polling_loop::tests::pr_creation_uses_owner_specific_token` that runs a fixture pass with `owner_tokens: { fixture-owner: SPECIFIC_TOKEN }` set and asserts the GitHub mockito server received the request with that specific token in the Authorization header. The current `github::tests::create_pull_request_posts_expected_request` already pins the `Bearer <token>` shape; this new test pins the routing.

## 5. Documentation

- [x] 5.1 Update README's Quick Start prerequisites: when `owner_tokens` exists, the operator can configure one PAT per owner. Re-word the current "fine-grained PATs are scoped to a single owner" caveat to point at the multi-token feature.
- [x] 5.2 Add a new README section "Multiple GitHub Tokens" between Configuration Reference and Architecture, walking through the personal + 2-org case end-to-end: how the owner is parsed, how `owner_tokens:` is configured, what the env vars must be exported as, what the startup log lines look like, and the recommendation to use SSH URLs for the git side so the same token logic doesn't have to be re-implemented in git-credential-helper land.
- [x] 5.3 Update the example deployment systemd unit's `EnvironmentFile=/etc/autocoder.env` example to show multiple PAT env vars commented in/out.

## 6. Verification

- [x] 6.1 `cargo test` passes with no regressions (137 baseline tests + the new tests from sections 1, 2, 3, 4).
- [x] 6.2 `cargo build --release` produces a binary that, given a multi-token `config.yaml` and the matching env vars set, opens a PR against a sandbox repo in each of two different owners using the correctly-routed token. (Live-service exercise is per-operator dev/staging per project convention; no automated cross-owner smoke.)
- [x] 6.3 `openspec validate multi-token-github-credentials --strict` passes.
