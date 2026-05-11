use anyhow::{anyhow, Result};

use crate::config::GithubConfig;

/// Resolve the GitHub PAT to use for an HTTP API call against a repository
/// owned by `owner`.
///
/// Lookup order:
/// 1. If `cfg.owner_tokens` contains a key matching `owner` case-insensitively,
///    read the env var named by that entry.
/// 2. Otherwise, read the env var named by `cfg.token_env`.
///
/// On any miss (env var unset), the returned error names both the env var
/// and the owner so the operator can fix the config or the environment.
pub fn resolve_token(cfg: &GithubConfig, owner: &str) -> Result<String> {
    if let Some(map) = cfg.owner_tokens.as_ref() {
        if let Some((_matched_key, env_name)) = map
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(owner))
        {
            return std::env::var(env_name).map_err(|_| {
                anyhow!(
                    "owner-token env var `{env_name}` for owner `{owner}` is not set"
                )
            });
        }
    }
    let token_env = &cfg.token_env;
    std::env::var(token_env).map_err(|_| {
        anyhow!(
            "github token env var `{token_env}` is not set; no `owner_tokens` route for owner `{owner}`"
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::sync::Mutex;

    /// Env mutation is process-global, so all tests in this module share one mutex.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    fn cfg_with(token_env: &str, owner_tokens: Option<HashMap<String, String>>) -> GithubConfig {
        GithubConfig {
            token_env: token_env.into(),
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
        map.insert("my-org".into(), var.into());
        let cfg = cfg_with("FALLBACK_VAR", Some(map));
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
        map.insert("some-other-org".into(), "OTHER_VAR".into());
        let cfg = cfg_with(fallback, Some(map));
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
        map.insert("My-Org".into(), var.into());
        let cfg = cfg_with("UNUSED", Some(map));
        // URL owner lowercase, config key mixed-case — must still match.
        let got = resolve_token(&cfg, "my-org").unwrap();
        assert_eq!(got, "case-secret");
        // Reverse direction.
        let mut map2 = HashMap::new();
        map2.insert("my-org".into(), var.into());
        let cfg2 = cfg_with("UNUSED", Some(map2));
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
        map.insert("acme".into(), var.into());
        let cfg = cfg_with("ALSO_UNSET", Some(map));
        let err = resolve_token(&cfg, "acme").unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains(var), "error must name the env var; got: {msg}");
        assert!(msg.contains("acme"), "error must name the owner; got: {msg}");
    }

    #[test]
    fn no_route_and_token_env_unset_names_both() {
        let _g = ENV_LOCK.lock().unwrap();
        let fallback = "AUTOCODER_TEST_UNSET_FALLBACK_VAR_XYZ";
        unsafe { std::env::remove_var(fallback) };
        let cfg = cfg_with(fallback, None);
        let err = resolve_token(&cfg, "lonely-owner").unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains(fallback), "error must name the fallback env var; got: {msg}");
        assert!(msg.contains("lonely-owner"), "error must name the owner; got: {msg}");
    }

    #[test]
    fn owner_tokens_none_uses_token_env() {
        let _g = ENV_LOCK.lock().unwrap();
        let fallback = "AUTOCODER_TEST_NONE_MAP_FALLBACK";
        unsafe { std::env::set_var(fallback, "only-fallback") };
        let cfg = cfg_with(fallback, None);
        let got = resolve_token(&cfg, "any-owner").unwrap();
        assert_eq!(got, "only-fallback");
        unsafe { std::env::remove_var(fallback) };
    }
}
