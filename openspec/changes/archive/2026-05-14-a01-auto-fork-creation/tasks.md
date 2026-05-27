## 1. Fork-creation API call

- [x] 1.1 Add `pub async fn create_fork(upstream_owner: &str, upstream_repo: &str, token: &str) -> Result<()>` in `src/github.rs` that POSTs to `https://api.github.com/repos/<owner>/<repo>/forks` with `Authorization: Bearer <token>`. Returns Ok on 2xx; Err with status + body snippet on non-2xx.
- [x] 1.2 Add `pub async fn create_fork_at(api_base, ...)` for test override (mirroring the `create_pull_request_at_for_test` pattern).
- [x] 1.3 **Verify:** `github::tests::create_fork_posts_to_forks_endpoint` using mockito; asserts URL path, Bearer header, and that 202 → Ok.
- [x] 1.4 **Verify:** `github::tests::create_fork_errors_on_non_2xx` asserting the error contains the status code.

## 2. Replace `validate_fork_existence` with `ensure_forks_exist`

- [x] 2.1 Rename `cli::run::validate_fork_existence` to `ensure_forks_exist` and make it `async`. Iterate every repo in fork-PR mode:
  1. Run `git::ls_remote_head(&fork_url)`. If Ok, that repo's fork is already there; continue.
  2. If Err, resolve the PAT for the upstream owner via `resolve_token`, then call `github::create_fork(upstream_owner, upstream_repo, &token).await`. Record any error and continue.
  3. After a successful POST, poll `git::ls_remote_head(&fork_url)` every 2s for up to 60s. Use `tokio::time::sleep` and `Instant::now() + Duration::from_secs(60)`.
  4. Aggregate failures into a single error message naming each repo's upstream URL, expected fork URL, and the failure type (POST status or polling timeout).
- [x] 2.2 Update the call site in `cli::run::execute` from `validate_fork_existence(...)` to `ensure_forks_exist(...).await`.
- [x] 2.3 Log per-action lines: `info!("creating fork for {upstream_url} → {fork_url}")` before the POST; `info!("created fork {fork_url} from upstream {upstream_url}")` after polling succeeds. Use `warn!` for failures.

## 3. Tests

- [x] 3.1 `cli::run::tests::ensure_forks_exist_skipped_in_direct_push_mode` — when `fork_owner` is None, the function returns Ok immediately without probing.
- [x] 3.2 `cli::run::tests::ensure_forks_exist_errors_on_unsupported_url_scheme` — preserved from the prior `validate_fork_existence` test; same shape, async caller.
- [x] 3.3 The live POST path is exercised by the github.rs mockito tests in section 1; integration testing the polling loop against a real fork creation requires GitHub network access and is operator-side validation.

## 4. Documentation

- [x] 4.1 README's Fork-and-PR section 7: remove step 2 ("manually fork each repo") and replace with "autocoder forks automatically on first startup if a configured fork is missing." Step 3's PAT-permission note adds `Administration: write` for fine-grained PATs that need to create forks; classic `repo` scope already covers fork creation.
- [x] 4.2 README's Quick Start prereq for fine-grained PATs: add the `Administration: write` requirement, gated by "only if using `github.fork_owner` AND your forks don't already exist."

## 5. Verification

- [x] 5.1 `cargo test` passes; test count grows by at least: 2 github + 2 cli::run + existing-tests-still-pass = ~4 new tests.
- [x] 5.2 `cargo build --release` produces a binary that, given a config with `fork_owner` set and a repo whose fork does NOT yet exist, creates the fork at startup and proceeds to normal polling.
- [x] 5.3 `openspec validate auto-fork-creation --strict` passes.
