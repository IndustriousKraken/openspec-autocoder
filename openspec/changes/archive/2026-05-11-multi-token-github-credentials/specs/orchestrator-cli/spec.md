## ADDED Requirements

### Requirement: Per-owner GitHub token routing
autocoder SHALL resolve the GitHub PAT used for each PR-creation call by parsing the repository URL's owner segment and consulting an optional `owner_tokens` map in the `github:` config block. When no owner-specific entry matches, autocoder SHALL fall back to the global `github.token_env`. When neither route resolves, autocoder SHALL fail at startup before any polling task is spawned.

#### Scenario: Owner-specific token used when configured
- **WHEN** `config.yaml`'s `github.owner_tokens` map contains an entry whose key matches the URL owner of a configured repository (case-insensitive)
- **THEN** the PR-creation HTTP call for that repository uses the value of the environment variable named by `owner_tokens[<matched-key>]`
- **AND** if that environment variable is unset at startup, autocoder exits non-zero with stderr naming both the owner and the missing env var

#### Scenario: Fallback to global token when no owner match
- **WHEN** `config.yaml`'s `github.owner_tokens` map either is absent OR has no entry matching a given repository's URL owner
- **THEN** the PR-creation HTTP call for that repository uses the value of the environment variable named by `github.token_env`
- **AND** if `github.token_env`'s named environment variable is unset at startup, autocoder exits non-zero with stderr naming the missing env var AND the repository whose owner has no `owner_tokens` route

#### Scenario: Startup logs name the env var per repository
- **WHEN** autocoder starts and successfully resolves a token route for every configured repository
- **THEN** for each repository, autocoder emits an info-level log line of the form `repository <url> will use GitHub token from env var <env-var-name>`
- **AND** the log line names ONLY the environment variable name, never the token value itself

#### Scenario: Case-insensitive owner matching
- **WHEN** `owner_tokens` contains a key like `My-Org` AND a repository URL has owner `my-org`
- **THEN** the entry matches and the corresponding env var is used
- **AND** the same applies in reverse (config key `my-org`, URL owner `My-Org`)

#### Scenario: Backward compatibility — config with only `token_env`
- **WHEN** `config.yaml` has a `github:` block with `token_env` set AND no `owner_tokens` key
- **THEN** every repository uses the env var named by `token_env`, with no behavior change from the prior single-token implementation
