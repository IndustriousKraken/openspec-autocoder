## 1. GitHub helpers

- [ ] 1.1 In `autocoder/src/github.rs`, add `pub async fn list_open_prs_by_author(owner, repo, author_logins: &[&str], token: &str) -> Result<Vec<PullRequestSummary>>` where `PullRequestSummary { number, html_url, author_login, title }`. Hits `GET /repos/{owner}/{repo}/pulls?state=open` and filters client-side by author.
- [ ] 1.2 `pub async fn fetch_pr_diff(owner, repo, number, token) -> Result<String>` — `GET /repos/.../pulls/{n}` with `Accept: application/vnd.github.v3.diff`.
- [ ] 1.3 `pub async fn list_pr_reviews(owner, repo, number, token) -> Result<Vec<PullRequestReview>>` where `PullRequestReview { user_login, state }`. Used to detect prior approval.
- [ ] 1.4 `pub async fn approve_pr(owner, repo, number, body, token) -> Result<()>` — `POST /repos/.../pulls/{n}/reviews` with `{"event": "APPROVE", "body": ...}`.
- [ ] 1.5 Tests in `github::tests` using mockito for each helper. Especially:
  - `list_open_prs_filters_by_author`
  - `fetch_pr_diff_accepts_diff_media_type`
  - `approve_pr_posts_correct_payload`
  - `list_pr_reviews_returns_user_logins`

## 2. Safe-shape filter

- [ ] 2.1 New module `autocoder/src/audits/dependency_update.rs`. Constants: `KNOWN_MANIFEST_FILES: &[&str]` (the list from the spec — `Cargo.toml`, `package.json`, etc.).
- [ ] 2.2 `fn classify_diff(diff: &str) -> Classification` where `Classification` is:
  ```rust
  pub enum Classification {
      Safe,
      NewDependencyEntry { path: String, entry: String },
      ScriptHookAdded { path: String, hook: String },
      SourceUrlChange { path: String, field: String },
      NonManifestFiles { paths: Vec<String> },
      DiffParseError(String),
  }
  ```
- [ ] 2.3 Parse the unified diff (a small ad-hoc parser is sufficient — split on `diff --git`, extract `+++` / `---` paths, scan `+` lines).
- [ ] 2.4 For each modified manifest file:
  - Reject if path is not in `KNOWN_MANIFEST_FILES`.
  - For `package.json`: parse the added/removed JSON fragments; reject if the diff adds keys under `dependencies`/`devDependencies` that didn't exist, or modifies anything under `scripts` (`postinstall`/`preinstall`/`prepublish`/etc).
  - For `Cargo.toml`: reject any new top-level dependency entry; reject any `build = "..."` field changes; reject any `registry = "..."` field changes.
  - For lockfiles (`*.lock`): allow only version + hash field changes per dependency entry.
  - For language-specific manifests (`requirements.txt`, `pyproject.toml`, `*.csproj`, `go.mod`, `Gemfile`, etc.): start with the same "no new top-level entries, no script-equivalent fields" check.
- [ ] 2.5 Tests `dependency_update::tests`:
  - `safe_classification_for_version_bump_only_diff`
  - `new_dependency_entry_in_package_json_rejected`
  - `new_postinstall_script_in_package_json_rejected`
  - `new_build_field_in_cargo_toml_rejected`
  - `non_manifest_file_in_diff_rejected`
  - `lockfile_only_version_hash_changes_allowed`
  - `registry_url_change_rejected`

## 3. Audit implementation

- [ ] 3.1 `pub struct DependencyUpdateAudit { settings: ..., max_approvals_per_run: u32, fork_remote_name: String }` implementing `Audit`.
- [ ] 3.2 `audit_type() -> "dependency_update_triage"`, `requires_head_change() -> false`, `write_policy() -> WritePolicy::None`.
- [ ] 3.3 `run(&self, ctx) -> Result<AuditOutcome>`:
  1. Determine target repo: if `github.fork_owner` is set, target is `<fork_owner>/<repo_name>`. Else `<upstream_owner>/<repo_name>`.
  2. Resolve the GitHub token via the existing `github_credentials::resolve_token`.
  3. Call `list_open_prs_by_author(target_owner, repo_name, &["dependabot[bot]", "dependabot-preview[bot]"], &token)`.
  4. For each PR (up to a global hard cap of 100 — defense against an exploded list):
     - Skip if `list_pr_reviews` shows our bot user has already approved.
     - Fetch diff via `fetch_pr_diff`.
     - Classify via `classify_diff`.
     - If `Safe` and we're under `max_approvals_per_run`: call `approve_pr`. Increment counter.
     - Otherwise: add a `Finding` to the outcome.
  5. After the loop, if the safe-but-deferred count is > 0, add a `Finding` listing the deferred PR numbers.
  6. Return `AuditOutcome::Reported(findings)`.
- [ ] 3.4 Registration in `cli/run.rs::build_audit_registry`: append `Arc::new(DependencyUpdateAudit::new(&audit_settings, github_cfg.clone()))`.
- [ ] 3.5 Tests `dependency_update::audit_tests`:
  - `run_approves_safe_prs_up_to_cap`
  - `run_skips_already_approved_prs`
  - `run_reports_unsafe_prs_via_findings`
  - `run_reports_deferred_safe_prs_when_cap_hit`
  - `run_handles_list_api_failure_returning_err`
  - `run_continues_when_individual_diff_fetch_fails`

## 4. Documentation

- [ ] 4.1 README "Periodic audits" — add `dependency_update_triage` to the list of registered audits with its semantics and config knobs.
- [ ] 4.2 README "Config reference" — under `audits.dependency_update_triage`, document `max_approvals_per_run` (default `5`) and `fork_remote_name` (default `"fork"`).

## 5. Verification

- [ ] 5.1 `cargo test` passes.
- [ ] 5.2 `openspec validate dependency-update-audit --strict` passes.
