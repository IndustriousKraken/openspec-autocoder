## Why

Fine-grained GitHub PATs can only be created by the **owner** of a resource — the user themselves for personal repos, or an org admin for org repos. An operator running autocoder against repos spread across multiple accounts (personal + one or more orgs) cannot use a single fine-grained PAT for all of them. The current single-token-at-global-scope model forces them to either fall back to a classic broad-scope PAT (security regression) or run multiple autocoder instances with different configs (defeats the multi-repo daemon design). This change adds per-owner token routing so a normal multi-account user can run one daemon with one config.

## What Changes

- Extend the `github:` config block with an optional `owner_tokens:` map from GitHub owner name → environment variable holding that owner's PAT.
- At PR-creation time, autocoder parses the owner from the repo URL and resolves the token via this lookup order: `owner_tokens[owner]` if present, else `github.token_env`, else error.
- `github.token_env` becomes optional when `owner_tokens` is present and covers every configured repo.
- At startup, autocoder validates that every configured repository has a routable token (either an explicit `owner_tokens` entry OR a global `token_env`); missing routes exit non-zero before any polling task spawns.

## Capabilities

### Modified Capabilities
- `orchestrator-cli`: GitHub credential resolution gains per-owner routing with a global fallback. The PR-creation HTTP path is unchanged; only the token selection changes.

## Impact

Operators with repos across multiple GitHub accounts can configure one fine-grained PAT per scope (personal, org-a, org-b, etc.) and run a single autocoder instance against all of them. The change is fully backward-compatible: a config with only `token_env` and no `owner_tokens` works exactly as before.
