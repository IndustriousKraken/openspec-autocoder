//! Uniform prompt-template loader (a24). Every embedded prompt in
//! `prompts/*.md` is loaded through [`PromptLoader::load`] so the
//! precedence is identical across audit, executor, reviewer, and
//! brownfield consumers: per-workspace nested override → per-workspace
//! flat-legacy override → daemon-level flat-legacy override → embedded
//! default. Missing-override paths log a one-shot WARN naming the
//! `(PromptId, path)` pair.

pub mod loader;

pub use loader::{PromptId, PromptLoader};

/// Render a `{{placeholder}}` template in a SINGLE pass (a002).
///
/// The template is scanned exactly once. Each recognized `{{key}}` token
/// (where `key` matches a `vars` pair) is replaced with that pair's
/// value; the scan then resumes immediately AFTER the token, so injected
/// values are NEVER re-scanned. Consequences:
///
/// - A `{{...}}` token appearing inside a substituted VALUE (a README, a
///   diff, a changed file's contents, operator guidance, …) is emitted
///   verbatim — it is not expanded. This is the defect the helper fixes:
///   naive chained `String::replace` re-scans already-substituted content
///   and re-expands such tokens, corrupting AND multiplying the prompt.
/// - Rendering is linear in `template.len() + Σ value.len()`; it cannot
///   multiply.
/// - Unrecognized `{{tokens}}` in the template are left verbatim (the
///   same behavior chained `.replace` had — it only touched the specific
///   keys it was given).
///
/// `vars` is a slice of `(key, value)` pairs where `key` is the bare
/// placeholder name (e.g. `"diff"` for the `{{diff}}` token). On a
/// duplicate key the first matching pair wins.
///
/// For inputs whose values contain no placeholder tokens, the output is
/// byte-identical to the prior chained `template.replace("{{a}}", a)
/// .replace("{{b}}", b)…` rendering: no two distinct `{{key}}` tokens can
/// overlap, and with no value-injected tokens the result is independent
/// of substitution order.
pub fn render_template(template: &str, vars: &[(&str, &str)]) -> String {
    let mut out = String::with_capacity(template.len());
    let mut rest = template;
    while let Some(open) = rest.find("{{") {
        // Everything before the `{{` is copied verbatim.
        out.push_str(&rest[..open]);
        let after_open = &rest[open + 2..];
        if let Some(close) = after_open.find("}}") {
            let key = &after_open[..close];
            if let Some((_, value)) = vars.iter().find(|(k, _)| *k == key) {
                out.push_str(value);
                // Resume AFTER the closing `}}`; the value is never
                // re-scanned, so any `{{...}}` it carries stays literal.
                rest = &after_open[close + 2..];
                continue;
            }
        }
        // No closing `}}`, or the key is not recognized: emit the literal
        // `{{` and resume right after it, so a later (or nested) `{{key}}`
        // can still match — mirroring `String::replace`'s leftmost-match
        // behavior.
        out.push_str("{{");
        rest = after_open;
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod render_template_tests {
    use super::render_template;

    /// 3.1 — a value carrying another placeholder token renders that
    /// token verbatim; the template's own placeholder is substituted
    /// exactly once, regardless of pair order.
    #[test]
    fn value_borne_placeholder_is_not_re_expanded() {
        let template = "{{readme}}\n---\n{{symbols_overview}}";
        let readme = "intro mentioning {{symbols_overview}} literally";
        let symbols = "SYMBOLS_VALUE";

        for vars in [
            &[("readme", readme), ("symbols_overview", symbols)][..],
            // Reverse order must produce the same result.
            &[("symbols_overview", symbols), ("readme", readme)][..],
        ] {
            let out = render_template(template, vars);
            assert_eq!(
                out,
                "intro mentioning {{symbols_overview}} literally\n---\nSYMBOLS_VALUE"
            );
            // The literal carried in the README survives…
            assert!(out.contains("mentioning {{symbols_overview}} literally"));
            // …and the real placeholder is substituted exactly once.
            assert_eq!(out.matches("SYMBOLS_VALUE").count(), 1);
        }
    }

    /// 3.2 — linear growth: a value with K copies of `{{x}}` grows the
    /// output by `K × len("{{x}}")`, NOT `K × len(value_of_x)`.
    #[test]
    fn rendering_is_linear_not_multiplicative() {
        let token = "{{x}}";
        let k = 1000usize;
        let big_value = token.repeat(k);
        let x_value = "X".repeat(50_000); // far larger than the token

        let template = "{{readme}} {{x}}";
        let out = render_template(
            template,
            &[("readme", big_value.as_str()), ("x", x_value.as_str())],
        );

        // Output = big_value (K literal tokens, unexpanded) + " " + x_value
        // (the template's own single {{x}}). It must NOT contain x_value K+1
        // times — that would be the multiplicative blowup.
        assert_eq!(out.matches(&x_value).count(), 1);
        assert_eq!(out.matches(token).count(), k);
        assert_eq!(out.len(), big_value.len() + 1 + x_value.len());
    }

    /// 3.3 — normal inputs (no placeholder tokens in values) render
    /// byte-identically to the old chained `String::replace`.
    #[test]
    fn normal_inputs_match_chained_replace() {
        let template =
            "head {{a}} mid {{b}} {{a}} tail {{unknown}} {{c}}";
        let a = "AAA";
        let b = "BBB";
        let c = "CCC";

        let chained = template
            .replace("{{a}}", a)
            .replace("{{b}}", b)
            .replace("{{c}}", c);
        let single = render_template(template, &[("a", a), ("b", b), ("c", c)]);

        assert_eq!(single, chained);
        // Unrecognized placeholder left verbatim.
        assert!(single.contains("{{unknown}}"));
    }

    #[test]
    fn unrecognized_and_unterminated_tokens_are_left_verbatim() {
        // Unterminated `{{` and an unknown key both survive untouched.
        let template = "before {{ broken and {{diff}} after {{nope}}";
        let out = render_template(template, &[("diff", "DIFF")]);
        assert_eq!(out, "before {{ broken and DIFF after {{nope}}");
    }

    #[test]
    fn empty_vars_returns_template_unchanged() {
        let template = "nothing {{here}} changes";
        assert_eq!(render_template(template, &[]), template);
    }
}
