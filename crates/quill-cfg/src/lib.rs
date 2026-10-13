use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};
use syn::parse::Parser;
use syn::punctuated::Punctuated;
use syn::{Attribute, Expr, Lit, Meta, Token};

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

    pub fn matches_meta(&self, meta: &Meta) -> bool {
        match meta {
            Meta::Path(path) => self.has_flag(&path_string(path)),
            Meta::NameValue(name_value) => {
                let Expr::Lit(expression) = &name_value.value else {
                    return false;
                };
                let Lit::Str(value) = &expression.lit else {
                    return false;
                };
                self.has_value(&path_string(&name_value.path), &value.value())
            }
            Meta::List(list) => {
                let Ok(items) =
                    Punctuated::<Meta, Token![,]>::parse_terminated.parse2(list.tokens.clone())
                else {
                    return false;
                };
                match path_string(&list.path).as_str() {
                    "all" => items.iter().all(|item| self.matches_meta(item)),
                    "any" => items.iter().any(|item| self.matches_meta(item)),
                    "not" if items.len() == 1 => !self.matches_meta(&items[0]),
                    _ => false,
                }
            }
        }
    }

    /// Whether an item's cfg/cfg_attr attributes leave it active.
    pub fn attributes_enabled(&self, attributes: &[Attribute]) -> bool {
        attributes
            .iter()
            .all(|attribute| self.attribute_enabled(attribute))
    }

    pub fn attribute_enabled(&self, attribute: &Attribute) -> bool {
        match &attribute.meta {
            Meta::List(list) if list.path.is_ident("cfg") => {
                parse_single_meta(&list.tokens).is_some_and(|meta| self.matches_meta(&meta))
            }
            Meta::List(list) if list.path.is_ident("cfg_attr") => {
                let Ok(items) =
                    Punctuated::<Meta, Token![,]>::parse_terminated.parse2(list.tokens.clone())
                else {
                    return false;
                };
                let Some(condition) = items.first() else {
                    return false;
                };
                if !self.matches_meta(condition) {
                    return true;
                }
                items
                    .iter()
                    .skip(1)
                    .filter(|meta| meta.path().is_ident("cfg") || meta.path().is_ident("cfg_attr"))
                    .all(|meta| self.meta_attribute_enabled(meta))
            }
            _ => true,
        }
    }

    /// Extract an active string-valued attribute, including `cfg_attr(...,
    /// path = "...")`. Used for Rust's module path override attribute.
    pub fn active_string_attribute(&self, attributes: &[Attribute], name: &str) -> Option<String> {
        attributes
            .iter()
            .find_map(|attribute| self.active_string_meta(&attribute.meta, name))
    }

    fn meta_attribute_enabled(&self, meta: &Meta) -> bool {
        match meta {
            Meta::List(list) if list.path.is_ident("cfg") => parse_single_meta(&list.tokens)
                .is_some_and(|condition| self.matches_meta(&condition)),
            Meta::List(list) if list.path.is_ident("cfg_attr") => {
                let Ok(items) =
                    Punctuated::<Meta, Token![,]>::parse_terminated.parse2(list.tokens.clone())
                else {
                    return false;
                };
                let Some(condition) = items.first() else {
                    return false;
                };
                !self.matches_meta(condition)
                    || items
                        .iter()
                        .skip(1)
                        .all(|meta| self.meta_attribute_enabled(meta))
            }
            _ => true,
        }
    }

    fn active_string_meta(&self, meta: &Meta, name: &str) -> Option<String> {
        match meta {
            Meta::NameValue(value) if value.path.is_ident(name) => {
                let Expr::Lit(expression) = &value.value else {
                    return None;
                };
                let Lit::Str(value) = &expression.lit else {
                    return None;
                };
                Some(value.value())
            }
            Meta::List(list) if list.path.is_ident("cfg_attr") => {
                let items = Punctuated::<Meta, Token![,]>::parse_terminated
                    .parse2(list.tokens.clone())
                    .ok()?;
                let condition = items.first()?;
                if !self.matches_meta(condition) {
                    return None;
                }
                items
                    .iter()
                    .skip(1)
                    .find_map(|nested| self.active_string_meta(nested, name))
            }
            _ => None,
        }
    }
}

fn parse_single_meta(tokens: &proc_macro2::TokenStream) -> Option<Meta> {
    syn::parse2(tokens.clone()).ok()
}

fn path_string(path: &syn::Path) -> String {
    path.segments
        .iter()
        .map(|segment| segment.ident.to_string())
        .collect::<Vec<_>>()
        .join("::")
}

fn hash_field(hasher: &mut blake3::Hasher, bytes: &[u8]) {
    hasher.update(&(bytes.len() as u64).to_le_bytes());
    hasher.update(bytes);
}

#[cfg(test)]
mod tests {
    use super::CfgSet;
    use syn::parse_quote;

    #[test]
    fn evaluates_nested_cfg_predicates_and_feature_values() {
        let mut cfg = CfgSet::default();
        cfg.insert_flag("unix");
        cfg.insert_value("target_arch", "x86_64");
        cfg.insert_value("feature", "fast-path");
        let condition: syn::Meta = parse_quote!(all(
            unix,
            target_arch = "x86_64",
            any(feature = "fast-path", feature = "other")
        ));
        assert!(cfg.matches_meta(&condition));
        let disabled: syn::Meta = parse_quote!(not(target_arch = "wasm32"));
        assert!(cfg.matches_meta(&disabled));
    }

    #[test]
    fn applies_cfg_attr_gates_and_extracts_active_path() {
        let mut cfg = CfgSet::default();
        cfg.insert_flag("unix");
        let attrs: Vec<syn::Attribute> = vec![parse_quote!(#[cfg_attr(unix, path = "unix.rs")])];
        assert!(cfg.attributes_enabled(&attrs));
        assert_eq!(
            cfg.active_string_attribute(&attrs, "path").as_deref(),
            Some("unix.rs")
        );

        let gated: syn::Attribute = parse_quote!(#[cfg_attr(unix, cfg(feature = "missing"))]);
        assert!(!cfg.attribute_enabled(&gated));
    }
}
