use std::collections::BTreeSet;

use ra_ap_syntax::{AstNode, AstToken, Edition, SourceFile, ast};
use serde::{Deserialize, Serialize};

/// Compiler-visible cfg facts for one Cargo package/target.
#[derive(Clone, Debug, Default, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
pub struct CfgSet {
    flags: BTreeSet<String>,
    values: BTreeSet<(String, String)>,
}

impl CfgSet {
    /// Add the facts from another configuration set.
    pub fn extend(&mut self, other: Self) {
        self.flags.extend(other.flags);
        self.values.extend(other.values);
    }

    pub fn insert_flag(&mut self, flag: impl Into<String>) {
        self.flags.insert(flag.into());
    }

    pub fn insert_value(&mut self, key: impl Into<String>, value: impl Into<String>) {
        self.values.insert((key.into(), value.into()));
    }

    pub fn has_flag(&self, flag: &str) -> bool {
        self.flags.contains(flag)
    }

    pub fn has_value(&self, key: &str, value: &str) -> bool {
        self.values.contains(&(key.to_owned(), value.to_owned()))
    }

    /// Stable fingerprint suitable for configuration-sensitive query keys.
    pub fn fingerprint(&self) -> [u8; 32] {
        let mut hasher = blake3::Hasher::new();
        for flag in &self.flags {
            hash_field(&mut hasher, b"flag");
            hash_field(&mut hasher, flag.as_bytes());
        }
        for (key, value) in &self.values {
            hash_field(&mut hasher, b"value");
            hash_field(&mut hasher, key.as_bytes());
            hash_field(&mut hasher, value.as_bytes());
        }
        *hasher.finalize().as_bytes()
    }

    /// Parse the line format emitted by `rustc --print cfg`.
    pub fn parse_rustc_cfg(output: &str) -> Self {
        let mut cfg = Self::default();
        for line in output
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
        {
            if let Some((key, value)) = line.split_once('=') {
                let value = value.trim().trim_matches('"');
                cfg.insert_value(key.trim(), value);
            } else {
                cfg.insert_flag(line);
            }
        }
        cfg
    }

    // Frontend migration API: attribute_enabled(&ast::Attr),
    // attributes_enabled(&[ast::Attr]), active_string_attribute(&[ast::Attr], &str),
    // matches_meta(&ast::Meta), and matches_predicate(&ast::CfgPredicate).
    // Callers pass nodes from their RA parse; no Syn/token-stream bridge is needed.

    /// Evaluate a meta-shaped predicate such as `all(unix, feature = "fast")`.
    pub fn matches_meta(&self, meta: &ast::Meta) -> bool {
        parse_predicate(&meta.syntax().text().to_string())
            .is_some_and(|predicate| self.evaluate_predicate(&predicate))
    }

    /// Evaluate a predicate from an RA cfg node, rejecting parser recovery.
    pub fn matches_predicate(&self, predicate: &ast::CfgPredicate) -> bool {
        parse_predicate(&predicate.syntax().text().to_string())
            .is_some_and(|predicate| self.evaluate_predicate(&predicate))
    }

    fn evaluate_predicate(&self, predicate: &ast::CfgPredicate) -> bool {
        match predicate {
            ast::CfgPredicate::CfgAtom(atom) => {
                // Syn treated `true` and `false` as ordinary cfg paths. Keep
                // that behavior instead of interpreting them as booleans.
                if let Some(token) = atom.true_token().or_else(|| atom.false_token()) {
                    return self.has_flag(token.text());
                }
                let Some(key) = atom.ident_token() else {
                    return false;
                };
                if atom.eq_token().is_some() {
                    atom.string_token()
                        .and_then(ast::String::cast)
                        .and_then(|value| value.value().ok().map(|value| value.into_owned()))
                        .is_some_and(|value| self.has_value(key.text(), &value))
                } else {
                    self.has_flag(key.text())
                }
            }
            ast::CfgPredicate::CfgComposite(composite) => {
                let Some(keyword) = composite.keyword() else {
                    return false;
                };
                let mut predicates = composite.cfg_predicates();
                match keyword.text() {
                    "all" => predicates.all(|predicate| self.evaluate_predicate(&predicate)),
                    "any" => predicates.any(|predicate| self.evaluate_predicate(&predicate)),
                    "not" => {
                        let Some(predicate) = predicates.next() else {
                            return false;
                        };
                        predicates.next().is_none() && !self.evaluate_predicate(&predicate)
                    }
                    _ => false,
                }
            }
        }
    }

    /// Whether an item's cfg/cfg_attr attributes leave it active.
    pub fn attributes_enabled(&self, attributes: &[ast::Attr]) -> bool {
        attributes
            .iter()
            .all(|attribute| self.attribute_enabled(attribute))
    }

    pub fn attribute_enabled(&self, attribute: &ast::Attr) -> bool {
        // Unrelated attributes, including opaque macro token trees, do not gate items.
        if !attribute
            .simple_name()
            .is_some_and(|name| name == "cfg" || name == "cfg_attr")
        {
            return true;
        }
        checked_attribute_meta(attribute).is_some_and(|meta| self.meta_attribute_enabled(&meta))
    }

    /// Extract the first active string-valued attribute, including nested
    /// `cfg_attr(..., path = "...")`. Used for Rust's module path override.
    pub fn active_string_attribute(&self, attributes: &[ast::Attr], name: &str) -> Option<String> {
        attributes.iter().find_map(|attribute| {
            if !attribute.simple_name().is_some_and(|attribute_name| {
                attribute_name == name || attribute_name == "cfg_attr"
            }) {
                return None;
            }
            self.active_string_meta(&checked_attribute_meta(attribute)?, name)
        })
    }

    fn meta_attribute_enabled(&self, meta: &ast::Meta) -> bool {
        match meta {
            ast::Meta::CfgMeta(meta) => {
                // The former single-Meta parser rejected a trailing comma at
                // the top level (`cfg(unix,)`) while allowing commas in
                // nested all/any/cfg_attr lists. Preserve that distinction.
                !has_direct_comma(meta.syntax())
                    && meta
                        .cfg_predicate()
                        .is_some_and(|predicate| self.evaluate_predicate(&predicate))
            }
            ast::Meta::CfgAttrMeta(meta) => {
                let Some(condition) = meta.cfg_predicate() else {
                    return false;
                };
                !self.evaluate_predicate(&condition)
                    || meta.metas().all(|meta| self.meta_attribute_enabled(&meta))
            }
            // RA only recognizes the parenthesized forms as cfg nodes.
            ast::Meta::TokenTreeMeta(_)
                if meta
                    .simple_name()
                    .is_some_and(|name| name == "cfg" || name == "cfg_attr") =>
            {
                false
            }
            _ => true,
        }
    }

    fn active_string_meta(&self, meta: &ast::Meta, name: &str) -> Option<String> {
        match meta {
            ast::Meta::KeyValueMeta(value) if meta.simple_name().as_deref() == Some(name) => {
                let ast::Expr::Literal(literal) = value.expr()? else {
                    return None;
                };
                let value = ast::String::cast(literal.token())?;
                value.value().ok().map(|value| value.into_owned())
            }
            ast::Meta::CfgAttrMeta(meta) => {
                if !self.evaluate_predicate(&meta.cfg_predicate()?) {
                    return None;
                }
                meta.metas()
                    .find_map(|nested| self.active_string_meta(&nested, name))
            }
            _ => None,
        }
    }
}

fn has_direct_comma(node: &ra_ap_syntax::SyntaxNode) -> bool {
    node.children_with_tokens()
        .filter_map(|element| element.into_token())
        .any(|token| token.text() == ",")
}

fn parse_predicate(text: &str) -> Option<ast::CfgPredicate> {
    let parse = SourceFile::parse(&format!("#![cfg({text})]"), Edition::CURRENT);
    if !parse.errors().is_empty() {
        return None;
    }
    parse
        .tree()
        .syntax()
        .descendants()
        .find_map(ast::CfgMeta::cast)?
        .cfg_predicate()
}

fn checked_attribute_meta(attribute: &ast::Attr) -> Option<ast::Meta> {
    // An AST node alone does not retain its Parse diagnostics. Revalidate this
    // attribute, rather than interpreting a recovered tree (e.g. any(unix, @))
    // as an active configuration. Errors elsewhere in the file are irrelevant.
    let text = format!("{}\nfn __quill_cfg_probe() {{}}", attribute.syntax().text());
    let parse = SourceFile::parse(&text, Edition::CURRENT);
    if !parse.errors().is_empty() {
        return None;
    }
    parse
        .tree()
        .syntax()
        .descendants()
        .find_map(ast::Attr::cast)?
        .meta()
}

fn hash_field(hasher: &mut blake3::Hasher, bytes: &[u8]) {
    hasher.update(&(bytes.len() as u64).to_le_bytes());
    hasher.update(bytes);
}

#[cfg(test)]
mod tests {
    use super::CfgSet;
    use ra_ap_syntax::{AstNode, Edition, SourceFile, ast};

    fn attributes(text: &str) -> Vec<ast::Attr> {
        let parse = SourceFile::parse(&format!("{text}\nfn probe() {{}}"), Edition::CURRENT);
        assert!(parse.errors().is_empty(), "{text}: {:?}", parse.errors());
        parse
            .tree()
            .syntax()
            .descendants()
            .filter_map(ast::Attr::cast)
            .collect()
    }

    fn recovered_attribute(text: &str) -> ast::Attr {
        let parse = SourceFile::parse(&format!("{text}\nfn probe() {{}}"), Edition::CURRENT);
        assert!(!parse.errors().is_empty(), "expected recovery: {text}");
        parse
            .tree()
            .syntax()
            .descendants()
            .find_map(ast::Attr::cast)
            .unwrap()
    }

    fn predicate(cfg: &CfgSet, text: &str) -> bool {
        let attrs = attributes(&format!("#[{text}]"));
        cfg.matches_meta(&attrs[0].meta().unwrap())
    }

    #[test]
    fn evaluates_nested_cfg_predicates_and_feature_values() {
        let mut cfg = CfgSet::default();
        cfg.insert_flag("unix");
        cfg.insert_value("target_arch", "x86_64");
        cfg.insert_value("feature", "fast-path");
        assert!(predicate(
            &cfg,
            r#"all(
            unix,
            target_arch = "x86_64",
            any(feature = "fast-path", feature = "other")
        )"#
        ));
        assert!(predicate(&cfg, r#"not(target_arch = "wasm32")"#));
        assert!(!predicate(&cfg, r#"feature = "missing""#));
        assert!(!predicate(&cfg, "windows"));

        let attrs = attributes("#[cfg(all(unix, not(windows)))]");
        let ast::Meta::CfgMeta(meta) = attrs[0].meta().unwrap() else {
            panic!()
        };
        assert!(cfg.matches_predicate(&meta.cfg_predicate().unwrap()));
    }

    #[test]
    fn applies_cfg_attr_gates_and_extracts_active_path() {
        let mut cfg = CfgSet::default();
        cfg.insert_flag("unix");
        let attrs = attributes(r#"#[cfg_attr(unix, path = "unix.rs")]"#);
        assert!(cfg.attributes_enabled(&attrs));
        assert_eq!(
            cfg.active_string_attribute(&attrs, "path").as_deref(),
            Some("unix.rs")
        );
        let gated = attributes(r#"#[cfg_attr(unix, cfg(feature = "missing"))]"#);
        assert!(!cfg.attribute_enabled(&gated[0]));
        assert!(CfgSet::default().attribute_enabled(&gated[0]));
    }

    #[test]
    fn evaluates_empty_lists_trailing_commas_and_not_arity() {
        let mut cfg = CfgSet::default();
        cfg.insert_flag("unix");
        for (text, expected) in [
            ("all()", true),
            ("any()", false),
            ("not()", false),
            ("not(unix, windows)", false),
            ("not(windows,)", true),
            ("all(unix,)", true),
            ("any(windows, unix,)", true),
            ("unsupported(unix)", false),
        ] {
            assert_eq!(predicate(&cfg, text), expected, "{text}");
        }
        assert!(!cfg.attributes_enabled(&attributes("#[cfg(unix,)]")));
        let true_attribute = attributes("#[cfg(true)]");
        assert!(!cfg.attributes_enabled(&true_attribute));
        cfg.insert_flag("true");
        assert!(cfg.attributes_enabled(&true_attribute));
    }

    #[test]
    fn applies_all_attributes_and_nested_cfg_attr_gates() {
        let mut cfg = CfgSet::default();
        cfg.insert_flag("unix");
        cfg.insert_value("feature", "enabled");
        assert!(cfg.attributes_enabled(&attributes(
            r#"
            #[cfg(unix)]
            #[cfg_attr(unix, cfg_attr(feature = "enabled", cfg(not(windows))),
                derive(Debug), allow(dead_code))]
        "#
        )));
        assert!(!cfg.attributes_enabled(&attributes(
            r#"
            #[cfg(unix)]
            #[cfg_attr(unix, cfg_attr(feature = "enabled", cfg(windows)))]
        "#
        )));
        assert!(!cfg.attributes_enabled(&attributes("#[cfg(unix)] #[cfg(windows)]")));
        assert!(
            cfg.attributes_enabled(&attributes("#[cfg_attr(windows, cfg(unix), cfg(windows))]"))
        );
        assert!(cfg.attributes_enabled(&[]));
    }

    #[test]
    fn extracts_first_active_nested_path_and_ignores_non_strings() {
        let mut cfg = CfgSet::default();
        cfg.insert_flag("unix");
        cfg.insert_flag("debug_assertions");
        let attrs = attributes(
            r#"
            #[cfg_attr(windows, path = "windows.rs")]
            #[path = 42]
            #[path = concat!("computed", ".rs")]
            #[cfg_attr(unix, cfg_attr(debug_assertions, path = "nested.rs"), path = "later.rs")]
            #[path = "fallback.rs"]
        "#,
        );
        assert_eq!(
            cfg.active_string_attribute(&attrs, "path").as_deref(),
            Some("nested.rs")
        );
        assert_eq!(
            CfgSet::default()
                .active_string_attribute(&attrs, "path")
                .as_deref(),
            Some("fallback.rs")
        );
        assert_eq!(cfg.active_string_attribute(&attrs, "other"), None);
        assert_eq!(cfg.active_string_attribute(&[], "path"), None);
    }

    #[test]
    fn decodes_raw_escaped_unicode_and_continued_strings() {
        let mut cfg = CfgSet::default();
        cfg.insert_value("feature", "a, \"quoted\" (feature)");
        cfg.insert_value("feature", "fast-path");
        cfg.insert_value("feature", "日本語");
        assert!(cfg.attributes_enabled(&attributes(
            r###"#[cfg(feature = r##"a, "quoted" (feature)"##)]"###
        )));
        assert!(cfg.attributes_enabled(&attributes(r#"#[cfg(feature = "fast\x2dpath")]"#)));
        assert!(cfg.attributes_enabled(&attributes(r#"#[cfg(feature = "日本\u{8a9e}")]"#)));
        assert!(cfg.attributes_enabled(&attributes("#[cfg(feature = \"fast-\\\n    path\")]")));
        let attrs = attributes(r###"#[path = r##"dir\quoted,"file".rs"##]"###);
        assert_eq!(
            cfg.active_string_attribute(&attrs, "path").as_deref(),
            Some(r#"dir\quoted,"file".rs"#)
        );
        let attrs = attributes(r#"#[path = "dir\x2ffile\u{2e}rs"]"#);
        assert_eq!(
            cfg.active_string_attribute(&attrs, "path").as_deref(),
            Some("dir/file.rs")
        );
    }

    #[test]
    fn ignores_unrelated_attributes_and_supports_inner_cfg() {
        let mut cfg = CfgSet::default();
        cfg.insert_flag("unix");
        let attrs = attributes(
            r#"
            #![cfg(unix)]
            #[some_macro(a => b, [tokens; 1], { arbitrary + payload })]
            #[tool::cfg(windows)]
            #[tool::path = "other.rs"]
            #[doc = "example"]
        "#,
        );
        assert!(cfg.attributes_enabled(&attrs));
        assert_eq!(cfg.active_string_attribute(&attrs, "path"), None);
        assert_eq!(
            cfg.active_string_attribute(&attrs, "doc").as_deref(),
            Some("example")
        );
    }

    #[test]
    fn rejects_recovered_cfg_instead_of_enabling_partial_trees() {
        let mut cfg = CfgSet::default();
        cfg.insert_flag("unix");
        for text in [
            "#[cfg()]",
            "#[cfg(any(unix, @))]",
            "#[cfg(all(unix windows))]",
            "#[cfg_attr()]",
            "#[cfg_attr(unix)]",
            "#[cfg_attr(unix, cfg(any(unix, @)))]",
            "#[cfg(feature = 42)]",
            "#[cfg(unix)] garbage ]",
        ] {
            // Only errors inside the attribute affect cfg evaluation.
            if text == "#[cfg(unix)] garbage ]" {
                assert!(cfg.attribute_enabled(&recovered_attribute(text)));
            } else {
                assert!(!cfg.attribute_enabled(&recovered_attribute(text)), "{text}");
            }
        }
        let attr = recovered_attribute(r#"#[cfg_attr(unix, path = "\q")]"#);
        assert_eq!(cfg.active_string_attribute(&[attr], "path"), None);
    }

    #[test]
    fn matches_predicate_rejects_recovery_under_negation() {
        let cfg = CfgSet::default();
        let attr = recovered_attribute("#[cfg(not(feature = ))]");
        let ast::Meta::CfgMeta(meta) = attr.meta().unwrap() else {
            panic!()
        };
        assert!(!cfg.matches_predicate(&meta.cfg_predicate().unwrap()));
    }

    #[test]
    fn preserves_cfg_facts_and_order_independent_fingerprints() {
        let mut cfg = CfgSet::parse_rustc_cfg("unix\ntarget_arch=\"x86_64\"\n\n");
        let mut extra = CfgSet::default();
        extra.insert_value("feature", "enabled");
        cfg.extend(extra);
        let mut reordered = CfgSet::default();
        reordered.insert_value("feature", "enabled");
        reordered.insert_value("target_arch", "x86_64");
        reordered.insert_flag("unix");
        reordered.insert_flag("unix");
        assert_eq!(cfg, reordered);
        assert_eq!(cfg.fingerprint(), reordered.fingerprint());
        reordered.insert_flag("windows");
        assert_ne!(cfg.fingerprint(), reordered.fingerprint());
    }
}
