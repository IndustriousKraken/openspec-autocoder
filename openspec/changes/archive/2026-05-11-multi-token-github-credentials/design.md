## Context

Fine-grained GitHub PATs (the security-recommended PAT form since 2022) are scoped to a single account or organization at creation time — you cannot make a fine-grained PAT that covers repos in `your-account` and `some-org` and `another-org` simultaneously. Each of those owners must mint their own fine-grained PAT (or grant you access to one). Multi-account contributors hit this constraint immediately: even a moderately active engineer typically has a personal account plus access to one or two work orgs.

autocoder today reads a single token via `github.token_env` (default `GITHUB_TOKEN`) and uses it for every PR-creation call regardless of which repository the PR is for. This works only when the operator either (a) uses a classic broad-scope PAT — discouraged on security grounds — or (b) constrains their `config.yaml` to repos under a single owner. Both are workarounds the operator has to think about; the design should accommodate the common case directly.

## Goals / Non-Goals

**Goals:**
- A multi-account operator can configure one fine-grained PAT per owner in their `config.yaml` and run a single autocoder instance against repos under all those owners.
- Token selection is automatic: parse the URL, look up the owner, pick the matching token. No per-repo annotation required.
- Fail-fast on misconfiguration: at startup, before any polling task spawns, every configured repo must have a routable token.
- Backward compatibility: existing single-token configs (just `token_env`, no `owner_tokens`) work unchanged.
- The PR-creation HTTP code path is unchanged — only the token argument's value changes.

**Non-Goals:**
- **Per-repository token override.** An operator who wants two different tokens for two repos under the same owner is an unusual case. If it arises, a follow-on change can add per-repo `github_token_env` (parallel to `slack_channel_id`). Not in this change.
- **Git operations.** autocoder uses `git` directly (not the GitHub API) for clone/fetch/push. Multi-owner git authentication is the user's responsibility via SSH keys or a git credential helper that maps URLs to tokens. This change does NOT teach git about multiple tokens — that's outside scope. README guidance steers users toward SSH URLs for multi-owner setups.
- **Token validation at startup beyond presence.** Verifying that each token actually has the required scopes on the named repos would require an API round-trip per repo at startup, which is slow and noisy. We validate that the env var is *set*; if it's wrong/expired, the failure surfaces at the first PR-creation call with a clear GitHub API error.
- **Renaming `token_env`.** It stays as the global default. The new field is additive.

## Decisions

- **Schema:**

  ```yaml
  github:
    token_env: GITHUB_TOKEN              # optional, used as fallback when no owner match
    owner_tokens:                        # optional, map owner name → env var name
      rabbeverly: PERSONAL_GH_TOKEN
      my-org-a: ORG_A_GH_TOKEN
      my-org-b: ORG_B_GH_TOKEN
  ```

  The map key is the GitHub owner (the segment before the repo name in the URL — `owner/repo`). Case-insensitive matching at lookup time (GitHub names are case-insensitive).

- **Resolution function:**

  ```rust
  pub fn resolve_token(
      cfg: &GithubConfig,
      owner: &str,
  ) -> Result<String>
  ```

  1. If `cfg.owner_tokens` contains an entry whose key matches `owner` case-insensitively, read that env var. If the env var is unset, return `Err` naming both the owner and the env var.
  2. Else if `cfg.token_env` is `Some`, read that env var. If unset, return `Err` naming the env var.
  3. Else (no entry, no fallback): return `Err` naming the owner and listing the configured `owner_tokens` keys + whether `token_env` is set.

- **`token_env` becomes optional:** in `GithubConfig`, `token_env: String` changes to `token_env: Option<String>`. The default-value annotation (`default = "default_github_token_env"`) is dropped; if the operator wants the previous behavior, they explicitly write `token_env: GITHUB_TOKEN`. Hmm — actually that's a backward-incompatible change. Reconsider:

  - **Better:** keep `token_env: String` with the same default `"GITHUB_TOKEN"`. The semantics: `token_env` always names an env var; whether that var must be set depends on whether `owner_tokens` covers every configured repo. This way an existing config keeps working with no edits.
  - The startup validator decides whether the env var must be present. If every repo's owner has an `owner_tokens` route, `GITHUB_TOKEN` (the default name) does not need to be set in the environment. Otherwise, it does.

  Going with the second form to preserve backward compatibility.

- **Startup validation:** in `cli::run::execute`, after `workspace::detect_collisions`, iterate every configured repo and call `resolve_token(&cfg.github, &owner_of(repo.url))`. Any `Err` aborts startup with a clear message listing the missing owner→env mapping. This catches typos at boot time, not 5 minutes later when the polling iteration first tries to open a PR.

- **Owner extraction:** reuse `github::parse_repo_url`, which already returns `(owner, repo)` and handles SSH/HTTPS variants. Case-fold for matching.

- **`apply_label` uses the same resolved token** for the do-not-merge fallback POST. That's automatic because `apply_label` is called from `create_pull_request` with the same `token` argument.

## Risks / Trade-offs

- **Risk:** operator typos in `owner_tokens` keys (e.g. `my-org` when GitHub knows the org as `my-org-inc`) silently fall through to `token_env`, which may have wrong scopes, producing a confusing 404/403 at PR time.
  - **Mitigation:** the startup validator only checks that *some* token resolves for each repo. To catch typos, the operator must verify their `owner_tokens` keys match the actual URL owners. Document this explicitly in the README; a future enhancement could warn at startup if any `owner_tokens` key never matches any configured repo (unused key).

- **Risk:** PAT in env var leaks via process listings (`ps -E`) or `EnvironmentFile=` files readable by the wrong users.
  - **Mitigation:** unchanged from today — the deployment guide already recommends `EnvironmentFile=/etc/autocoder.env` with mode `0600` owned by root. Adding more tokens doesn't change the threat model.

- **Risk:** users get confused about which token is being used for which repo.
  - **Mitigation:** emit a per-repo startup log line that names the env var (NOT the token value) used for that repo's PR creation. Plus README example walkthrough.

- **Risk:** `owner_tokens` keys are case-sensitive in YAML but GitHub names are case-insensitive.
  - **Mitigation:** comparison is case-insensitive. The validator + resolver both lowercase the key and the URL owner before comparing.
