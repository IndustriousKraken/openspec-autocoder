use anyhow::{Context, Result};

use crate::config::{GithubConfig, SecretSource};

/// Resolve the GitHub PAT to use for an HTTP API call against a repository
/// owned by `owner`.
///
/// Lookup order:
/// 1. If `cfg.owner_tokens` contains a key matching `owner` case-insensitively,
///    resolve that entry's `SecretSource` (env-var name OR inline value).
/// 2. Else if `cfg.token` is set, resolve that inline/env-var source.
/// 3. Otherwise read the env var named by `cfg.token_env`.
///
/// On miss, the returned error names the originating config field and (for
/// env-var sources) the env var name.
pub fn resolve_token(cfg: &GithubConfig, owner: &str) -> Result<String> {
    resolve_token_with_source(cfg, owner).map(|(value, _src)| value)
}

/// Like `resolve_token` but also returns a non-secret description of the
/// source for startup logging (e.g. `"env var GITHUB_TOKEN"` or
/// `"inline (github.token)"`).
pub fn resolve_token_with_source(
    cfg: &GithubConfig,
    owner: &str,
) -> Result<(String, String)> {
    if let Some(map) = cfg.owner_tokens.as_ref() {
        if let Some((matched_key, source)) = map
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(owner))
        {
            let field = format!("github.owner_tokens[{matched_key}]");
            let value = source.resolve(&field)?;
            return Ok((value, source.describe(&field)));
        }
    }
    if let Some(source) = cfg.token.as_ref() {
        let field = "github.token";
        let value = source
            .resolve(field)
            .with_context(|| format!("no `owner_tokens` route for owner `{owner}`"))?;
        return Ok((value, source.describe(field)));
    }
    let fallback = SecretSource::EnvVar(cfg.token_env.clone());
    let field = format!("github.token_env={}", cfg.token_env);
    let value = fallback
        .resolve(&field)
        .with_context(|| format!("no `owner_tokens` route for owner `{owner}`"))?;
    Ok((value, fallback.describe(&field)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::sync::Mutex;

    /// Env mutation is process-global, so all tests in this module share one mutex.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    fn cfg_with(
        token_env: &str,
        token: Option<SecretSource>,
        owner_tokens: Option<HashMap<String, SecretSource>>,
    ) -> GithubConfig {
        GithubConfig {
            token_env: token_env.into(),
            token,
            owner_tokens,
        }
    }

    #[test]
    fn owner_match_returns_specific_env_value() {
        let _g = ENV_LOCK.lock().unwrap();
        let var = "AUTOCODER_TEST_OWNER_TOKEN_A";
        // SAFETY: env mutation is gated by ENV_LOCK above.
        unsafe { std::env::set_var(var, "owner-specific-secret") };
        let mut map = HashMap::new();
        map.insert("my-org".into(), SecretSource::EnvVar(var.into()));
        let cfg = cfg_with("FALLBACK_VAR", None, Some(map));
        let got = resolve_token(&cfg, "my-org").unwrap();
        assert_eq!(got, "owner-specific-secret");
        unsafe { std::env::remove_var(var) };
    }

    #[test]
    fn no_owner_match_falls_back_to_token_env() {
        let _g = ENV_LOCK.lock().unwrap();
        let fallback = "AUTOCODER_TEST_FALLBACK_TOKEN";
        unsafe { std::env::set_var(fallback, "fallback-secret") };
        let mut map = HashMap::new();
        map.insert("some-other-org".into(), SecretSource::EnvVar("OTHER_VAR".into()));
        let cfg = cfg_with(fallback, None, Some(map));
        let got = resolve_token(&cfg, "uncovered-owner").unwrap();
        assert_eq!(got, "fallback-secret");
        unsafe { std::env::remove_var(fallback) };
    }

    #[test]
    fn case_insensitive_owner_match() {
        let _g = ENV_LOCK.lock().unwrap();
        let var = "AUTOCODER_TEST_CASE_TOKEN";
        unsafe { std::env::set_var(var, "case-secret") };
        let mut map = HashMap::new();
        map.insert("My-Org".into(), SecretSource::EnvVar(var.into()));
        let cfg = cfg_with("UNUSED", None, Some(map));
        let got = resolve_token(&cfg, "my-org").unwrap();
        assert_eq!(got, "case-secret");
        let mut map2 = HashMap::new();
        map2.insert("my-org".into(), SecretSource::EnvVar(var.into()));
        let cfg2 = cfg_with("UNUSED", None, Some(map2));
        let got2 = resolve_token(&cfg2, "My-Org").unwrap();
        assert_eq!(got2, "case-secret");
        unsafe { std::env::remove_var(var) };
    }

    #[test]
    fn owner_matched_but_env_var_unset_errors_with_both_names() {
        let _g = ENV_LOCK.lock().unwrap();
        let var = "AUTOCODER_TEST_DEFINITELY_UNSET_OWNER_VAR";
        unsafe { std::env::remove_var(var) };
        let mut map = HashMap::new();
        map.insert("acme".into(), SecretSource::EnvVar(var.into()));
        let cfg = cfg_with("ALSO_UNSET", None, Some(map));
        let err = resolve_token(&cfg, "acme").unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains(var), "error must name the env var; got: {msg}");
        assert!(
            msg.contains("acme"),
            "error must name the owner via the field path; got: {msg}"
        );
    }

    #[test]
    fn no_route_and_token_env_unset_names_both() {
        let _g = ENV_LOCK.lock().unwrap();
        let fallback = "AUTOCODER_TEST_UNSET_FALLBACK_VAR_XYZ";
        unsafe { std::env::remove_var(fallback) };
        let cfg = cfg_with(fallback, None, None);
        let err = resolve_token(&cfg, "lonely-owner").unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains(fallback),
            "error must name the fallback env var; got: {msg}"
        );
        // Field-label path includes `github.token_env=<name>`.
        assert!(
            msg.contains("github.token_env="),
            "error must name the field path; got: {msg}"
        );
    }

    #[test]
    fn owner_tokens_none_uses_token_env() {
        let _g = ENV_LOCK.lock().unwrap();
        let fallback = "AUTOCODER_TEST_NONE_MAP_FALLBACK";
        unsafe { std::env::set_var(fallback, "only-fallback") };
        let cfg = cfg_with(fallback, None, None);
        let got = resolve_token(&cfg, "any-owner").unwrap();
        assert_eq!(got, "only-fallback");
        unsafe { std::env::remove_var(fallback) };
    }

    #[test]
    fn inline_token_resolves_without_env() {
        let _g = ENV_LOCK.lock().unwrap();
        let cfg = cfg_with(
            "AUTOCODER_TEST_INLINE_FALLBACK_DEFINITELY_UNSET",
            Some(SecretSource::Inline {
                value: "inline-secret-value".into(),
            }),
            None,
        );
        let got = resolve_token(&cfg, "any-owner").unwrap();
        assert_eq!(got, "inline-secret-value");
    }

    #[test]
    fn inline_owner_token_resolves_without_env() {
        let _g = ENV_LOCK.lock().unwrap();
        let mut map = HashMap::new();
        map.insert(
            "personal".into(),
            SecretSource::Inline {
                value: "personal-inline-pat".into(),
            },
        );
        let cfg = cfg_with("UNSET_FALLBACK", None, Some(map));
        let got = resolve_token(&cfg, "personal").unwrap();
        assert_eq!(got, "personal-inline-pat");
    }

    #[test]
    fn inline_token_takes_precedence_over_token_env() {
        let _g = ENV_LOCK.lock().unwrap();
        let fallback = "AUTOCODER_TEST_PRECEDENCE_FALLBACK";
        unsafe { std::env::set_var(fallback, "env-value-should-be-ignored") };
        let cfg = cfg_with(
            fallback,
            Some(SecretSource::Inline {
                value: "inline-wins".into(),
            }),
            None,
        );
        let got = resolve_token(&cfg, "any-owner").unwrap();
        assert_eq!(got, "inline-wins");
        unsafe { std::env::remove_var(fallback) };
    }

    #[test]
    fn resolve_with_source_returns_inline_description() {
        let _g = ENV_LOCK.lock().unwrap();
        let cfg = cfg_with(
            "UNSET_VAR_XYZ",
            Some(SecretSource::Inline {
                value: "secret".into(),
            }),
            None,
        );
        let (value, src) = resolve_token_with_source(&cfg, "any").unwrap();
        assert_eq!(value, "secret");
        assert_eq!(src, "inline (github.token)");
    }

    #[test]
    fn resolve_with_source_returns_env_description() {
        let _g = ENV_LOCK.lock().unwrap();
        let var = "AUTOCODER_TEST_SOURCE_DESC_VAR";
        unsafe { std::env::set_var(var, "x") };
        let cfg = cfg_with(var, None, None);
        let (_value, src) = resolve_token_with_source(&cfg, "any").unwrap();
        assert_eq!(src, format!("env var {var}"));
        unsafe { std::env::remove_var(var) };
    }
}
