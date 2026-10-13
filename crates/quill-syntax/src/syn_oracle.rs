//! Test-only copy of the Syn-based collector that `quill-syntax` used before
//! the rust-analyzer migration. Differential tests compare it with the
//! production collector; it is not compiled into the library.

use std::collections::HashSet;
use std::ops::Range;

use quill_source::SourceFile;
use syn::ext::IdentExt;
use syn::parse::Parser;
use syn::spanned::Spanned;
use syn::visit::Visit;
use syn::visit_mut::VisitMut;

use crate::*;


pub(crate) fn parse_ast_with_cfg_and_edition(
    source: &SourceFile,
    cfg: &quill_cfg::CfgSet,
    edition: &str,
) -> Parse {
    let mut result = Parse::default();
    match parse_file_for_edition(source.text(), edition) {
        Ok(file) => {
            let mut collector = AstCollector::new(source, cfg, local_trait_names(&file, cfg));
            collector.visit_file(&file);
            result.functions = collector.functions;
            result.structs = collector.structs;
            result.enums = collector.enums;
            result.modules = collector.modules;
            result.imports = collector.imports;
            result.unsupported_item_count = collector.unsupported_item_count + file.attrs.len();
        }
        Err(error) => {
            result.had_error = true;
            let location = error.span().start();
            let offset = source.byte_offset(location.line, location.column);
            result.diagnostics.push(Diagnostic {
                span: offset..offset.saturating_add(1).min(source.text().len()),
                message: format!("invalid Rust syntax: {error}"),
            });
        }
    }
    result
}

fn generic_parameters_have_unsupported_bounds(
    parameters: &syn::punctuated::Punctuated<syn::GenericParam, syn::token::Comma>,
    local_traits: &HashSet<String>,
) -> bool {
    parameters.iter().any(|parameter| match parameter {
        syn::GenericParam::Type(parameter) => {
            parameter.default.is_some()
                || parameter.bounds.iter().any(|bound| match bound {
                    syn::TypeParamBound::Trait(bound) => {
                        !is_supported_copy_bound(bound)
                            && !is_supported_as_ref_str_bound(bound)
                            && !is_supported_add_bound(bound, &parameter.ident.unraw().to_string())
                            && !is_local_trait_bound(bound, local_traits)
                    }
                    syn::TypeParamBound::Lifetime(_) => true,
                    _ => true,
                })
        }
        syn::GenericParam::Lifetime(_) | syn::GenericParam::Const(_) => true,
    })
}

fn is_local_trait_bound(bound: &syn::TraitBound, local_traits: &HashSet<String>) -> bool {
    bound.path.segments.len() == 1
        && bound
            .path
            .segments
            .first()
            .is_some_and(|segment| {
                matches!(segment.arguments, syn::PathArguments::None)
                    && local_traits.contains(&segment.ident.unraw().to_string())
            })
        && bound.lifetimes.is_none()
        && bound.maybe.is_none()
        && bound.modifiers.require_empty().is_ok()
}

fn is_supported_copy_bound(bound: &syn::TraitBound) -> bool {
    let segments: Vec<_> = bound.path.segments.iter().collect();
    let supported_path = matches!(segments.as_slice(), [copy]
        if copy.ident == "Copy" && matches!(copy.arguments, syn::PathArguments::None))
        || matches!(segments.as_slice(), [root, marker, copy]
            if (root.ident == "core" || root.ident == "std")
                && marker.ident == "marker"
                && copy.ident == "Copy"
                && matches!(root.arguments, syn::PathArguments::None)
                && matches!(marker.arguments, syn::PathArguments::None)
                && matches!(copy.arguments, syn::PathArguments::None));
    supported_path
        && bound.lifetimes.is_none()
        && bound.maybe.is_none()
        && bound.modifiers.require_empty().is_ok()
}

fn generic_copy_parameters(
    parameters: &syn::punctuated::Punctuated<syn::GenericParam, syn::token::Comma>,
) -> Vec<String> {
    parameters
        .iter()
        .filter_map(|parameter| match parameter {
            syn::GenericParam::Type(parameter)
                if parameter.bounds.iter().any(|bound| {
                    matches!(bound, syn::TypeParamBound::Trait(bound) if is_supported_copy_bound(bound))
                }) => Some(parameter.ident.unraw().to_string()),
            _ => None,
        })
        .collect()
}

fn generic_as_ref_str_parameters(
    parameters: &syn::punctuated::Punctuated<syn::GenericParam, syn::token::Comma>,
) -> Vec<String> {
    parameters
        .iter()
        .filter_map(|parameter| match parameter {
            syn::GenericParam::Type(parameter)
                if parameter.bounds.iter().any(|bound| {
                    matches!(bound, syn::TypeParamBound::Trait(bound) if is_supported_as_ref_str_bound(bound))
                }) => Some(parameter.ident.unraw().to_string()),
            _ => None,
        })
        .collect()
}

fn generic_add_parameters(
    parameters: &syn::punctuated::Punctuated<syn::GenericParam, syn::token::Comma>,
) -> Vec<String> {
    parameters
        .iter()
        .filter_map(|parameter| match parameter {
            syn::GenericParam::Type(parameter)
                if parameter.bounds.iter().any(|bound| {
                    matches!(bound, syn::TypeParamBound::Trait(bound)
                        if is_supported_add_bound(bound, &parameter.ident.unraw().to_string()))
                }) =>
            {
                Some(parameter.ident.unraw().to_string())
            }
            _ => None,
        })
        .collect()
}

fn simple_generic_trait_bounds(
    parameters: &syn::punctuated::Punctuated<syn::GenericParam, syn::token::Comma>,
) -> Vec<(String, String)> {
    parameters
        .iter()
        .filter_map(|parameter| match parameter {
            syn::GenericParam::Type(parameter) => Some(
                parameter
                    .bounds
                    .iter()
                    .filter_map(|bound| match bound {
                        syn::TypeParamBound::Trait(bound)
                            if !is_supported_copy_bound(bound)
                                && !is_supported_as_ref_str_bound(bound)
                                && !is_supported_add_bound(
                                    bound,
                                    &parameter.ident.unraw().to_string(),
                                )
                                && bound.path.segments.len() == 1
                                && bound.path.segments.first().is_some_and(|segment| {
                                    matches!(segment.arguments, syn::PathArguments::None)
                                })
                                && bound.lifetimes.is_none()
                                && bound.maybe.is_none()
                                && bound.modifiers.require_empty().is_ok() =>
                        {
                            let trait_name = bound
                                .path
                                .segments
                                .iter()
                                .map(|segment| segment.ident.unraw().to_string())
                                .collect::<Vec<_>>()
                                .join("::");
                            Some((parameter.ident.unraw().to_string(), trait_name))
                        }
                        _ => None,
                    })
                    .collect::<Vec<_>>(),
            ),
            _ => None,
        })
        .flatten()
        .collect()
}

fn simple_where_trait_bounds(
    clause: Option<&syn::WhereClause>,
) -> Vec<(String, String)> {
    clause
        .into_iter()
        .flat_map(|clause| clause.predicates.iter())
        .filter_map(|predicate| match predicate {
            syn::WherePredicate::Type(predicate) => Some(predicate),
            _ => None,
        })
        .flat_map(|predicate| {
            let generic = match &predicate.bounded_ty {
                syn::Type::Path(path) if path.qself.is_none() && path.path.segments.len() == 1 => {
                    path.path.segments.first().map(|segment| segment.ident.unraw().to_string())
                }
                _ => None,
            };
            let Some(generic) = generic else {
                return Vec::new();
            };
            predicate
                .bounds
                .iter()
                .filter_map(move |bound| match bound {
                    syn::TypeParamBound::Trait(bound)
                        if !is_supported_copy_bound(bound)
                            && !is_supported_as_ref_str_bound(bound)
                            && !is_supported_add_bound(bound, &generic)
                            && bound.path.segments.len() == 1
                            && bound.path.segments.first().is_some_and(|segment| {
                                matches!(segment.arguments, syn::PathArguments::None)
                            })
                            && bound.lifetimes.is_none()
                            && bound.maybe.is_none()
                            && bound.modifiers.require_empty().is_ok() =>
                    {
                        let trait_name = bound.path.segments.first()?.ident.unraw().to_string();
                        Some((generic.clone(), trait_name))
                    }
                    _ => None,
                })
                .collect::<Vec<_>>()
        })
        .collect()
}

fn is_supported_add_bound(bound: &syn::TraitBound, generic: &str) -> bool {
    let segments: Vec<_> = bound.path.segments.iter().collect();
    let supported_path = matches!(segments.as_slice(), [add] if add.ident == "Add")
        || matches!(segments.as_slice(), [root, ops, add]
            if (root.ident == "core" || root.ident == "std")
                && ops.ident == "ops"
                && add.ident == "Add");
    if !supported_path
        || bound.lifetimes.is_some()
        || bound.maybe.is_some()
        || bound.modifiers.require_empty().is_err()
    {
        return false;
    }
    let Some(syn::PathArguments::AngleBracketed(arguments)) =
        segments.last().map(|segment| &segment.arguments)
    else {
        return false;
    };
    matches!(arguments.args.iter().collect::<Vec<_>>().as_slice(),
        [syn::GenericArgument::AssocType(output)]
            if output.ident == "Output"
                && matches!(&output.ty, syn::Type::Path(path)
                    if path.qself.is_none()
                        && path.path.segments.len() == 1
                        && path.path.segments[0].ident == generic
                        && matches!(path.path.segments[0].arguments, syn::PathArguments::None)))
}

fn is_supported_as_ref_str_bound(bound: &syn::TraitBound) -> bool {
    let segments: Vec<_> = bound.path.segments.iter().collect();
    let supported_path = matches!(segments.as_slice(), [as_ref] if as_ref.ident == "AsRef")
        || matches!(segments.as_slice(), [root, convert, as_ref]
            if (root.ident == "core" || root.ident == "std")
                && convert.ident == "convert"
                && as_ref.ident == "AsRef");
    if !supported_path
        || bound.lifetimes.is_some()
        || bound.maybe.is_some()
        || bound.modifiers.require_empty().is_err()
    {
        return false;
    }
    let Some(syn::PathArguments::AngleBracketed(arguments)) =
        segments.last().map(|segment| &segment.arguments)
    else {
        return false;
    };
    matches!(arguments.args.iter().collect::<Vec<_>>().as_slice(),
        [syn::GenericArgument::Type(syn::Type::Path(path))]
            if path.qself.is_none()
                && path.path.segments.len() == 1
                && path.path.segments[0].ident == "str"
                && matches!(path.path.segments[0].arguments, syn::PathArguments::None))
}

fn parse_file_for_edition(source: &str, edition: &str) -> syn::Result<syn::File> {
    fn rawify_async(tokens: proc_macro2::TokenStream) -> proc_macro2::TokenStream {
        use proc_macro2::{Ident, TokenTree};
        tokens
            .into_iter()
            .map(|token| match token {
                TokenTree::Ident(ident) if ident == "async" => {
                    TokenTree::Ident(Ident::new_raw("async", ident.span()))
                }
                TokenTree::Group(group) => {
                    let mut rewritten =
                        proc_macro2::Group::new(group.delimiter(), rawify_async(group.stream()));
                    rewritten.set_span(group.span());
                    TokenTree::Group(rewritten)
                }
                other => other,
            })
            .collect()
    }

    fn rawify_identifier_at(
        tokens: proc_macro2::TokenStream,
        location: proc_macro2::LineColumn,
        identifiers: &[&str],
    ) -> (proc_macro2::TokenStream, bool) {
        use proc_macro2::{Group, Ident, TokenTree};
        let mut changed = false;
        let tokens = tokens
            .into_iter()
            .map(|token| match token {
                TokenTree::Ident(ident)
                    if !changed
                        && identifiers.contains(&ident.to_string().as_str())
                        && ident.span().start() == location =>
                {
                    changed = true;
                    TokenTree::Ident(Ident::new_raw(&ident.to_string(), ident.span()))
                }
                TokenTree::Group(group) if !changed => {
                    let (stream, nested_change) =
                        rawify_identifier_at(group.stream(), location, identifiers);
                    if nested_change {
                        changed = true;
                        let mut rewritten = Group::new(group.delimiter(), stream);
                        rewritten.set_span(group.span());
                        TokenTree::Group(rewritten)
                    } else {
                        TokenTree::Group(group)
                    }
                }
                other => other,
            })
            .collect();
        (tokens, changed)
    }

    let tokens = source.parse::<proc_macro2::TokenStream>()?;
    if edition == "2015" {
        // Syn parses edition-neutral tokens. Raw identifiers let its parser
        // accept Rust 2015's `async` identifier without treating async
        // functions or blocks as Rust 2015 syntax.
        let mut tokens = rawify_async(tokens);
        loop {
            match syn::parse2(tokens.clone()) {
                Ok(file) => return Ok(file),
                Err(error) => {
                    // `await` and `dyn` are also identifiers in 2015. Only
                    // rewrite the exact token Syn rejected: `dyn Trait`
                    // remains valid trait-object syntax in this edition.
                    let location = error.span().start();
                    let (rewritten, changed) =
                        rawify_identifier_at(tokens, location, &["await", "dyn"]);
                    if !changed {
                        return Err(error);
                    }
                    tokens = rewritten;
                }
            }
        }
    }

    let file = syn::parse2(tokens)?;
    if matches!(edition, "2024" | "future") {
        let mut validator = Edition2024Validator::default();
        validator.visit_file(&file);
        if let Some(error) = validator.error {
            return Err(error);
        }
    }
    Ok(file)
}

#[derive(Default)]
struct Edition2024Validator {
    error: Option<syn::Error>,
}

impl<'ast> Visit<'ast> for Edition2024Validator {
    fn visit_ident(&mut self, ident: &'ast syn::Ident) {
        if self.error.is_none() && ident.to_string() == "gen" {
            self.error = Some(syn::Error::new(
                ident.span(),
                "`gen` is a reserved keyword in Rust 2024",
            ));
        }
    }

    fn visit_item_foreign_mod(&mut self, item: &'ast syn::ItemForeignMod) {
        if self.error.is_none() && item.unsafety.is_none() {
            self.error = Some(syn::Error::new(
                item.span(),
                "extern blocks must be unsafe in Rust 2024",
            ));
        }
        syn::visit::visit_item_foreign_mod(self, item);
    }

    fn visit_attribute(&mut self, attribute: &'ast syn::Attribute) {
        const UNSAFE_ATTRIBUTES: &[&str] = &["no_mangle", "export_name", "link_section", "naked"];
        if self.error.is_none()
            && UNSAFE_ATTRIBUTES
                .iter()
                .any(|name| attribute.path().is_ident(name))
        {
            self.error = Some(syn::Error::new(
                attribute.span(),
                "this attribute is unsafe and must be wrapped in `unsafe(...)` in Rust 2024",
            ));
        }
        syn::visit::visit_attribute(self, attribute);
    }
}

struct LocalTraitNameCollector<'a> {
    cfg: &'a quill_cfg::CfgSet,
    names: HashSet<String>,
}

impl<'ast> syn::visit::Visit<'ast> for LocalTraitNameCollector<'_> {
    fn visit_item(&mut self, item: &'ast syn::Item) {
        if attributes_enabled(&self.cfg, item_attributes(item)) {
            syn::visit::visit_item(self, item);
        }
    }

    fn visit_item_trait(&mut self, item: &'ast syn::ItemTrait) {
        self.names.insert(item.ident.unraw().to_string());
        syn::visit::visit_item_trait(self, item);
    }
}

fn local_trait_names(file: &syn::File, cfg: &quill_cfg::CfgSet) -> HashSet<String> {
    let mut collector = LocalTraitNameCollector {
        cfg,
        names: HashSet::new(),
    };
    collector.visit_file(file);
    collector.names
}

fn supported_local_trait(item: &syn::ItemTrait) -> bool {
    item.generics.params.is_empty()
        && item.generics.where_clause.is_none()
        && item.supertraits.is_empty()
        && !item.items.is_empty()
        && item.items.iter().all(|trait_item| match trait_item {
            syn::TraitItem::Fn(function) => {
                function.default.is_none()
                    && function.sig.generics.params.is_empty()
                    && function.sig.generics.where_clause.is_none()
                    && function.sig.constness.is_none()
                    && function.sig.asyncness.is_none()
                    && !matches!(function.sig.safety, syn::Safety::Unsafe(_))
                    && function.sig.abi.is_none()
                    && function.sig.inputs.iter().all(|argument| match argument {
                        syn::FnArg::Receiver(receiver) => {
                            matches!(
                                receiver.kind,
                                syn::ReceiverKind::Reference(_, None, None)
                            )
                        }
                        syn::FnArg::Typed(_) => true,
                    })
            }
            _ => false,
        })
}

fn supported_local_trait_impl(item: &syn::ItemImpl) -> bool {
    if item.trait_.is_none() {
        return true;
    }
    item.generics.params.is_empty()
        && item.generics.where_clause.is_none()
        && matches!(&*item.self_ty, syn::Type::Path(path) if path.qself.is_none() && path.path.segments.iter().all(|segment| matches!(segment.arguments, syn::PathArguments::None)))
        && item.trait_.as_ref().is_some_and(|(path, _)| {
            path.segments.len() == 1
                && path
                    .segments
                    .first()
                    .is_some_and(|segment| matches!(segment.arguments, syn::PathArguments::None))
        })
        && item.items.iter().all(|item| match item {
            syn::ImplItem::Fn(function) => {
                function.sig.generics.params.is_empty()
                    && function.sig.generics.where_clause.is_none()
                    && function.sig.constness.is_none()
                    && function.sig.asyncness.is_none()
                    && !matches!(function.sig.safety, syn::Safety::Unsafe(_))
                    && function.sig.abi.is_none()
            }
            _ => false,
        })
}

struct AstCollector<'a> {
    source: &'a SourceFile,
    cfg: quill_cfg::CfgSet,
    scope: Vec<String>,
    functions: Vec<FunctionItem>,
    structs: Vec<StructItem>,
    enums: Vec<EnumItem>,
    modules: Vec<ModuleItem>,
    imports: Vec<ImportItem>,
    macro_expansions: Vec<LocalMacroExpansion>,
    impl_generic_parameters: Vec<String>,
    impl_generic_has_bounds: bool,
    local_trait_names: HashSet<String>,
    unsupported_item_count: usize,
}

struct LocalMacroExpansion {
    name: String,
    scope: Vec<String>,
    rule: LocalMacroRule,
}

#[derive(Clone)]
enum LocalMacroRule {
    ZeroArgLiteral(syn::Expr),
    RepeatSeparatedExpr {
        capture: String,
        separator: char,
        allow_empty: bool,
        allow_trailing_separator: bool,
        transcriber: proc_macro2::TokenStream,
    },
}

impl<'a> AstCollector<'a> {
    fn new(
        source: &'a SourceFile,
        cfg: &quill_cfg::CfgSet,
        local_trait_names: HashSet<String>,
    ) -> Self {
        Self {
            source,
            cfg: cfg.clone(),
            scope: Vec::new(),
            functions: Vec::new(),
            structs: Vec::new(),
            enums: Vec::new(),
            modules: Vec::new(),
            imports: Vec::new(),
            macro_expansions: Vec::new(),
            impl_generic_parameters: Vec::new(),
            impl_generic_has_bounds: false,
            local_trait_names,
            unsupported_item_count: 0,
        }
    }

    fn add_function(
        &mut self,
        signature: &syn::Signature,
        visibility: &syn::Visibility,
        item_span: proc_macro2::Span,
        body: Option<&syn::Block>,
    ) {
        let mut calls = CallCollector::default();
        if let Some(body) = body {
            syn::visit::Visit::visit_block(&mut calls, body);
        }
        let parameters = signature
            .inputs
            .iter()
            .map(|argument| match argument {
                syn::FnArg::Receiver(receiver) => ParameterItem {
                    name: Some("self".to_owned()),
                    mutable: matches!(receiver.kind, syn::ReceiverKind::Value)
                        && receiver.mutability.is_some(),
                    type_text: match &receiver.kind {
                        syn::ReceiverKind::Value => {
                            if receiver.mutability.is_some() {
                                "mut Self".to_owned()
                            } else {
                                "Self".to_owned()
                            }
                        }
                        syn::ReceiverKind::Reference(_, lifetime, mutability) => format!(
                            "&{}{}Self",
                            lifetime
                                .as_ref()
                                // Oracle correction: the pre-migration collector printed
                                // `lifetime.ident`, dropping the apostrophe (`&a Self`).
                                .map(|lifetime| format!("{lifetime} "))
                                .unwrap_or_default(),
                            if mutability.is_some() { "mut " } else { "" }
                        ),
                        syn::ReceiverKind::Typed(_, ty) => self
                            .source
                            .slice(span_range(self.source, ty.span()))
                            .to_owned(),
                        _ => "Self".to_owned(),
                    },
                    span: span_range(self.source, receiver.span()),
                },
                syn::FnArg::Typed(argument) => ParameterItem {
                    name: match &*argument.pat {
                        syn::Pat::Ident(pattern) => Some(pattern.ident.unraw().to_string()),
                        _ => None,
                    },
                    mutable: matches!(&*argument.pat, syn::Pat::Ident(pattern) if pattern.mutability.is_some()),
                    type_text: self
                        .source
                        .slice(span_range(self.source, argument.ty.span()))
                        .to_owned(),
                    span: span_range(self.source, argument.span()),
                },
            })
            .collect();
        let return_type = match &signature.output {
            syn::ReturnType::Default => None,
            syn::ReturnType::Type(_, ty) => Some(
                self.source
                    .slice(span_range(self.source, ty.span()))
                    .to_owned(),
            ),
        };
        let mut generic_parameters: Vec<_> = signature
            .generics
            .params
            .iter()
            .filter_map(|parameter| match parameter {
                syn::GenericParam::Type(parameter) => Some(parameter.ident.unraw().to_string()),
                syn::GenericParam::Lifetime(_) | syn::GenericParam::Const(_) => None,
            })
            .collect();
        for parameter in &self.impl_generic_parameters {
            if !generic_parameters.contains(parameter) {
                generic_parameters.push(parameter.clone());
            }
        }
        let generic_has_bounds = self.impl_generic_has_bounds
            || signature.generics.where_clause.is_some()
            || generic_parameters_have_unsupported_bounds(
                &signature.generics.params,
                &self.local_trait_names,
            );
        self.functions.push(FunctionItem {
            name: signature.ident.unraw().to_string(),
            scope: self.scope.clone(),
            calls: calls.calls,
            qualified_calls: calls.qualified_calls,
            name_span: span_range(self.source, signature.ident.span()),
            span: span_range(self.source, item_span),
            body_span: body.map(|block| span_range(self.source, block.span())),
            parameters,
            return_type,
            body: body.map(|block| lower_block(block, self.source)),
            is_public: matches!(visibility, syn::Visibility::Public(_)),
            is_generic: !generic_parameters.is_empty(),
            generic_parameters,
            generic_copy_parameters: generic_copy_parameters(&signature.generics.params),
            generic_as_ref_str_parameters: generic_as_ref_str_parameters(
                &signature.generics.params,
            ),
            generic_add_parameters: generic_add_parameters(&signature.generics.params),
            generic_trait_bounds: simple_generic_trait_bounds(&signature.generics.params)
                .into_iter()
                .chain(simple_where_trait_bounds(signature.generics.where_clause.as_ref()))
                .collect(),
            generic_has_bounds,
            is_async: signature.asyncness.is_some(),
            is_const: signature.constness.is_some(),
            is_unsafe: matches!(signature.safety, syn::Safety::Unsafe(_)),
            has_abi: signature.abi.is_some(),
        });
    }
}

impl<'ast> syn::visit::Visit<'ast> for AstCollector<'_> {
    fn visit_item(&mut self, item: &'ast syn::Item) {
        if !attributes_enabled(&self.cfg, item_attributes(item)) {
            return;
        }
        let supported_literal_macro = matches!(item, syn::Item::Macro(macro_item)
            if macro_item.mac.path.is_ident("macro_rules")
                && macro_item.ident.is_some()
                && supported_macro_rule(macro_item).is_some());
        if !matches!(
            item,
            syn::Item::Fn(_)
                | syn::Item::Struct(_)
                | syn::Item::Enum(_)
                | syn::Item::Impl(_)
                | syn::Item::Trait(_)
        ) && !supported_literal_macro
        {
            self.unsupported_item_count += 1;
        }
        if let syn::Item::Fn(function) = item {
            self.unsupported_item_count += unsupported_attribute_count(&function.attrs);
        } else if let syn::Item::Struct(item) = item {
            self.unsupported_item_count += unsupported_attribute_count(&item.attrs);
        } else if let syn::Item::Enum(item) = item {
            self.unsupported_item_count += unsupported_attribute_count(&item.attrs);
        } else if let syn::Item::Impl(item) = item {
            self.unsupported_item_count += unsupported_attribute_count(&item.attrs);
        } else if let syn::Item::Trait(item) = item {
            self.unsupported_item_count += unsupported_attribute_count(&item.attrs);
        }
        syn::visit::visit_item(self, item);
    }

    fn visit_item_fn(&mut self, item: &'ast syn::ItemFn) {
        if self.macro_expansions.is_empty() {
            self.add_function(&item.sig, &item.vis, item.span(), Some(&item.block));
            self.scope.push(format!("fn:{}", item.sig.ident));
            syn::visit::visit_block(self, &item.block);
            self.scope.pop();
            return;
        }
        let mut block = (*item.block).clone();
        expand_syn_macros(&mut block, &self.macro_expansions, &self.scope);
        self.add_function(&item.sig, &item.vis, item.span(), Some(&block));
        self.scope.push(format!("fn:{}", item.sig.ident));
        syn::visit::visit_block(self, &block);
        self.scope.pop();
    }

    fn visit_item_macro(&mut self, item: &'ast syn::ItemMacro) {
        self.unsupported_item_count += unsupported_attribute_count(&item.attrs);
        if item.mac.path.is_ident("macro_rules")
            && let Some(name) = &item.ident
            && let Some(rule) = supported_macro_rule(item)
        {
            self.macro_expansions.push(LocalMacroExpansion {
                name: name.to_string(),
                scope: self.scope.clone(),
                rule,
            });
        }
    }

    fn visit_item_struct(&mut self, item: &'ast syn::ItemStruct) {
        let derived_traits = supported_derives(&item.attrs).unwrap_or_default();
        for attribute in item
            .attrs
            .iter()
            .filter(|attribute| attribute.path().is_ident("derive"))
        {
            if supported_derives(std::slice::from_ref(attribute)).is_some()
                && derived_traits.iter().any(|trait_name| {
                    item.fields
                        .iter()
                        .any(|field| !field_supports_derive(&field.ty, trait_name))
                })
            {
                self.unsupported_item_count += 1;
            }
        }
        self.unsupported_item_count += item
            .fields
            .iter()
            .map(|field| unsupported_attribute_count(&field.attrs))
            .sum::<usize>();
        let generic_parameters = item
            .generics
            .params
            .iter()
            .filter_map(|parameter| match parameter {
                syn::GenericParam::Type(parameter) => Some(parameter.ident.unraw().to_string()),
                syn::GenericParam::Lifetime(_) | syn::GenericParam::Const(_) => None,
            })
            .collect();
        let fields = item
            .fields
            .iter()
            .map(|field| StructFieldItem {
                name: field.ident.as_ref().map(ToString::to_string),
                type_text: self
                    .source
                    .slice(span_range(self.source, field.ty.span()))
                    .to_owned(),
                span: span_range(self.source, field.span()),
                is_public: matches!(field.vis, syn::Visibility::Public(_)),
            })
            .collect();
        self.structs.push(StructItem {
            name: item.ident.unraw().to_string(),
            scope: self.scope.clone(),
            fields,
            generic_parameters,
            derived_traits,
            repr_c: item.attrs.iter().any(|attribute| {
                attribute.path().is_ident("repr")
                    && attribute
                        .meta
                        .require_list()
                        .is_ok_and(|list| list.tokens.to_string().trim() == "C")
            }),
            repr_layout_supported: item.attrs.iter().all(|attribute| {
                !attribute.path().is_ident("repr")
                    || attribute
                        .meta
                        .require_list()
                        .is_ok_and(|list| matches!(list.tokens.to_string().trim(), "" | "C"))
            }),
            span: span_range(self.source, item.span()),
            is_public: matches!(item.vis, syn::Visibility::Public(_)),
        });
    }

    fn visit_item_enum(&mut self, item: &'ast syn::ItemEnum) {
        self.unsupported_item_count += unsupported_attribute_count(&item.attrs);
        let mut next_discriminant = Some(0i32);
        let mut variants = Vec::with_capacity(item.variants.len());
        let mut variant_names = std::collections::HashSet::new();
        let mut discriminants = std::collections::HashSet::new();
        for variant in &item.variants {
            self.unsupported_item_count += unsupported_attribute_count(&variant.attrs);
            if !matches!(variant.fields, syn::Fields::Unit) {
                self.unsupported_item_count += 1;
            }
            let discriminant = variant
                .discriminant
                .as_ref()
                .and_then(|(_, expression)| parse_i32_discriminant(expression))
                .or_else(|| {
                    variant
                        .discriminant
                        .is_none()
                        .then_some(next_discriminant)
                        .flatten()
                });
            let Some(discriminant) = discriminant else {
                self.unsupported_item_count += 1;
                continue;
            };
            if !variant_names.insert(variant.ident.unraw().to_string())
                || !discriminants.insert(discriminant)
            {
                self.unsupported_item_count += 1;
            }
            variants.push(EnumVariantItem {
                name: variant.ident.unraw().to_string(),
                discriminant,
                span: span_range(self.source, variant.span()),
            });
            next_discriminant = discriminant.checked_add(1);
        }
        let repr_supported = item.attrs.iter().all(|attribute| {
            !attribute.path().is_ident("repr")
                || attribute
                    .meta
                    .require_list()
                    .is_ok_and(|list| matches!(list.tokens.to_string().trim(), "i32"))
        });
        if !repr_supported {
            self.unsupported_item_count += 1;
        }
        self.enums.push(EnumItem {
            name: item.ident.unraw().to_string(),
            scope: self.scope.clone(),
            variants,
            derived_traits: supported_derives(&item.attrs).unwrap_or_default(),
            span: span_range(self.source, item.span()),
            is_public: matches!(item.vis, syn::Visibility::Public(_)),
        });
    }

    fn visit_item_mod(&mut self, item: &'ast syn::ItemMod) {
        self.modules.push(ModuleItem {
            name: item.ident.unraw().to_string(),
            parent_scope: self.scope.clone(),
            span: span_range(self.source, item.span()),
            body_span: item.content.as_ref().map(|(brace, _)| {
                let open = brace.span.open();
                let whole = open.join(brace.span.close()).unwrap_or(open);
                span_range(self.source, whole)
            }),
            path_override: active_string_attribute(&self.cfg, &item.attrs, "path"),
            is_public: matches!(item.vis, syn::Visibility::Public(_)),
        });
        if let Some((_, items)) = &item.content {
            self.scope.push(format!("mod:{}", item.ident));
            for nested in items {
                self.visit_item(nested);
            }
            self.scope.pop();
        }
    }

    fn visit_item_impl(&mut self, item: &'ast syn::ItemImpl) {
        if !supported_local_trait_impl(item) {
            self.unsupported_item_count += 1;
        }
        let name = match &*item.self_ty {
            syn::Type::Path(path) => path
                .path
                .segments
                .last()
                .map(|segment| segment.ident.unraw().to_string())
                .unwrap_or_else(|| "_".to_owned()),
            _ => "_".to_owned(),
        };
        let previous_parameter_count = self.impl_generic_parameters.len();
        let previous_has_bounds = self.impl_generic_has_bounds;
        self.impl_generic_parameters
            .extend(
                item.generics
                    .params
                    .iter()
                    .filter_map(|parameter| match parameter {
                        syn::GenericParam::Type(parameter) => {
                            Some(parameter.ident.unraw().to_string())
                        }
                        syn::GenericParam::Lifetime(_) | syn::GenericParam::Const(_) => None,
                    }),
            );
        self.impl_generic_has_bounds = item.generics.where_clause.is_some()
            || generic_parameters_have_unsupported_bounds(
                &item.generics.params,
                &HashSet::new(),
            )
            // Trait-bound metadata is currently carried only from function
            // parameters, so bounded impl parameters stay outside the subset.
            || !generic_copy_parameters(&item.generics.params).is_empty()
            || !generic_as_ref_str_parameters(&item.generics.params).is_empty();
        self.scope.push(format!("impl:{name}"));
        if let Some((trait_path, _)) = &item.trait_ {
            self.scope.push(format!(
                "traitimpl:{}",
                trait_path
                    .segments
                    .iter()
                    .map(|segment| segment.ident.unraw().to_string())
                    .collect::<Vec<_>>()
                    .join("::")
            ));
        }
        syn::visit::visit_item_impl(self, item);
        if item.trait_.is_some() {
            self.scope.pop();
        }
        self.scope.pop();
        self.impl_generic_parameters
            .truncate(previous_parameter_count);
        self.impl_generic_has_bounds = previous_has_bounds;
    }

    fn visit_item_trait(&mut self, item: &'ast syn::ItemTrait) {
        if !supported_local_trait(item) {
            self.unsupported_item_count += 1;
        }
        self.scope.push(format!("trait:{}", item.ident));
        syn::visit::visit_item_trait(self, item);
        self.scope.pop();
    }

    fn visit_impl_item_fn(&mut self, item: &'ast syn::ImplItemFn) {
        if !attributes_enabled(&self.cfg, &item.attrs) {
            return;
        }
        self.unsupported_item_count += unsupported_attribute_count(&item.attrs);
        self.add_function(&item.sig, &item.vis, item.span(), Some(&item.block));
        self.scope.push(format!("fn:{}", item.sig.ident));
        syn::visit::visit_block(self, &item.block);
        self.scope.pop();
    }

    fn visit_trait_item_fn(&mut self, item: &'ast syn::TraitItemFn) {
        if !attributes_enabled(&self.cfg, &item.attrs) {
            return;
        }
        self.add_function(
            &item.sig,
            &syn::Visibility::Inherited,
            item.span(),
            item.default.as_ref(),
        );
        if let Some(body) = &item.default {
            self.scope.push(format!("fn:{}", item.sig.ident));
            syn::visit::visit_block(self, body);
            self.scope.pop();
        }
    }

    fn visit_foreign_item_fn(&mut self, item: &'ast syn::ForeignItemFn) {
        if !attributes_enabled(&self.cfg, &item.attrs) {
            return;
        }
        self.add_function(&item.sig, &item.vis, item.span(), None);
    }

    fn visit_item_use(&mut self, item: &'ast syn::ItemUse) {
        let span = span_range(self.source, item.span());
        flatten_ast_use(
            &item.tree,
            &[],
            &self.scope,
            &span,
            matches!(item.vis, syn::Visibility::Public(_)),
            &mut self.imports,
        );
    }
}

fn supported_macro_rule(item: &syn::ItemMacro) -> Option<LocalMacroRule> {
    let mut tokens = item.mac.tokens.clone().into_iter();
    let proc_macro2::TokenTree::Group(pattern) = tokens.next()? else {
        return None;
    };
    if pattern.delimiter() != proc_macro2::Delimiter::Parenthesis {
        return None;
    }
    for expected in ['=', '>'] {
        let proc_macro2::TokenTree::Punct(punctuation) = tokens.next()? else {
            return None;
        };
        if punctuation.as_char() != expected {
            return None;
        }
    }
    let proc_macro2::TokenTree::Group(body) = tokens.next()? else {
        return None;
    };
    if body.delimiter() != proc_macro2::Delimiter::Brace {
        return None;
    }
    if let Some(token) = tokens.next()
        && !matches!(token, proc_macro2::TokenTree::Punct(punctuation) if punctuation.as_char() == ';')
    {
        return None;
    }
    if tokens.next().is_some() {
        return None;
    }
    if pattern.stream().is_empty() {
        let expression = syn::parse2::<syn::Expr>(body.stream()).ok()?;
        return matches!(expression, syn::Expr::Lit(_))
            .then_some(LocalMacroRule::ZeroArgLiteral(expression));
    }

    let pattern_tokens = pattern.stream().into_iter().collect::<Vec<_>>();
    let [
        capture_dollar,
        capture_group,
        separator_token,
        repetition,
        optional_tail @ ..,
    ] = pattern_tokens.as_slice()
    else {
        return None;
    };
    if !is_macro_punct(capture_dollar, '$') {
        return None;
    }
    let proc_macro2::TokenTree::Punct(separator) = separator_token else {
        return None;
    };
    if !matches!(separator.as_char(), ',' | ';') {
        return None;
    }
    let proc_macro2::TokenTree::Punct(repetition) = repetition else {
        return None;
    };
    let allow_empty = match repetition.as_char() {
        '+' => false,
        '*' => true,
        _ => return None,
    };
    let allow_trailing_separator = match optional_tail {
        [] => false,
        [optional_dollar, optional_group, question]
            if is_macro_punct(optional_dollar, '$') && is_macro_punct(question, '?') =>
        {
            let proc_macro2::TokenTree::Group(optional_group) = optional_group else {
                return None;
            };
            let optional_tokens = optional_group.stream().into_iter().collect::<Vec<_>>();
            optional_tokens.len() == 1 && is_macro_punct(&optional_tokens[0], separator.as_char())
        }
        _ => false,
    };
    if !allow_trailing_separator && !optional_tail.is_empty() {
        return None;
    }
    let proc_macro2::TokenTree::Group(capture_group) = capture_group else {
        return None;
    };
    let capture_tokens = capture_group.stream().into_iter().collect::<Vec<_>>();
    let [dollar, ident, colon, expr] = capture_tokens.as_slice() else {
        return None;
    };
    if !is_macro_punct(dollar, '$')
        || !is_macro_punct(colon, ':')
        || !matches!(expr, proc_macro2::TokenTree::Ident(kind) if kind == "expr")
    {
        return None;
    }
    let proc_macro2::TokenTree::Ident(capture) = ident else {
        return None;
    };
    Some(LocalMacroRule::RepeatSeparatedExpr {
        capture: capture.to_string(),
        separator: separator.as_char(),
        allow_empty,
        allow_trailing_separator,
        transcriber: body.stream(),
    })
}

fn is_macro_punct(token: &proc_macro2::TokenTree, expected: char) -> bool {
    matches!(token, proc_macro2::TokenTree::Punct(punctuation) if punctuation.as_char() == expected)
}

fn expand_syn_macros(
    block: &mut syn::Block,
    definitions: &[LocalMacroExpansion],
    scope: &[String],
) {
    struct Expander<'a> {
        definitions: &'a [LocalMacroExpansion],
        scope: &'a [String],
    }

    impl syn::visit_mut::VisitMut for Expander<'_> {
        fn visit_expr_mut(&mut self, expression: &mut syn::Expr) {
            let replacement = if let syn::Expr::Macro(invocation) = expression {
                let path = &invocation.mac.path;
                let name = if path.segments.len() == 1
                    && matches!(path.segments[0].arguments, syn::PathArguments::None)
                {
                    Some(path.segments[0].ident.unraw().to_string())
                } else {
                    None
                };
                name.and_then(|name| {
                    self.definitions
                        .iter()
                        .rev()
                        .find(|definition| {
                            definition.name == name && definition.scope == self.scope
                        })
                        .and_then(|definition| {
                            expand_local_macro(&definition.rule, &invocation.mac.tokens)
                        })
                })
            } else {
                None
            };
            if let Some(replacement) = replacement {
                *expression = replacement;
            }
            syn::visit_mut::visit_expr_mut(self, expression);
        }
    }

    Expander { definitions, scope }.visit_block_mut(block);
}

fn expand_local_macro(
    rule: &LocalMacroRule,
    arguments: &proc_macro2::TokenStream,
) -> Option<syn::Expr> {
    match rule {
        LocalMacroRule::ZeroArgLiteral(expression) if arguments.is_empty() => {
            Some(expression.clone())
        }
        LocalMacroRule::RepeatSeparatedExpr {
            capture,
            separator,
            allow_empty,
            allow_trailing_separator,
            transcriber,
        } => {
            let mut groups = Vec::<proc_macro2::TokenStream>::new();
            let mut current = proc_macro2::TokenStream::new();
            for token in arguments.clone() {
                if is_macro_punct(&token, *separator) {
                    if current.is_empty() {
                        return None;
                    }
                    syn::parse2::<syn::Expr>(current.clone()).ok()?;
                    groups.push(std::mem::take(&mut current));
                } else {
                    current.extend([token]);
                }
            }
            if !current.is_empty() {
                syn::parse2::<syn::Expr>(current.clone()).ok()?;
                groups.push(current);
            } else if !groups.is_empty() && !allow_trailing_separator {
                return None;
            }
            if groups.is_empty() && !allow_empty {
                return None;
            }

            let expanded = expand_macro_transcriber(transcriber, capture, &groups);
            syn::parse2::<syn::Expr>(expanded).ok()
        }
        _ => None,
    }
}

fn expand_macro_transcriber(
    transcriber: &proc_macro2::TokenStream,
    capture: &str,
    values: &[proc_macro2::TokenStream],
) -> proc_macro2::TokenStream {
    use proc_macro2::{Group, TokenTree};

    let tokens = transcriber.clone().into_iter().collect::<Vec<_>>();
    let mut output = proc_macro2::TokenStream::new();
    let mut index = 0;
    while index < tokens.len() {
        if is_macro_punct(&tokens[index], '$')
            && let Some(TokenTree::Group(repetition)) = tokens.get(index + 1)
            && let Some(TokenTree::Punct(operator)) = tokens.get(index + 2)
            && matches!(operator.as_char(), '+' | '*')
        {
            for value in values {
                output.extend(substitute_macro_capture(
                    &repetition.stream(),
                    capture,
                    value,
                ));
            }
            index += 3;
            continue;
        }
        if let TokenTree::Group(group) = &tokens[index] {
            let mut nested = Group::new(
                group.delimiter(),
                expand_macro_transcriber(&group.stream(), capture, values),
            );
            nested.set_span(group.span());
            output.extend([TokenTree::Group(nested)]);
        } else {
            output.extend([tokens[index].clone()]);
        }
        index += 1;
    }
    output
}

fn substitute_macro_capture(
    body: &proc_macro2::TokenStream,
    capture: &str,
    value: &proc_macro2::TokenStream,
) -> proc_macro2::TokenStream {
    let tokens = body.clone().into_iter().collect::<Vec<_>>();
    let mut output = proc_macro2::TokenStream::new();
    let mut index = 0;
    while index < tokens.len() {
        if is_macro_punct(&tokens[index], '$')
            && matches!(tokens.get(index + 1), Some(proc_macro2::TokenTree::Ident(name)) if name == capture)
        {
            output.extend(value.clone());
            index += 2;
        } else {
            output.extend([tokens[index].clone()]);
            index += 1;
        }
    }
    output
}

fn unsupported_attribute_count(attributes: &[syn::Attribute]) -> usize {
    attributes
        .iter()
        .filter(|attribute| {
            if attribute.path().is_ident("cfg")
                || attribute.path().is_ident("cfg_attr")
                || attribute.path().is_ident("repr")
            {
                false
            } else if attribute.path().is_ident("derive") {
                supported_derives(std::slice::from_ref(attribute)).is_none()
            } else {
                true
            }
        })
        .count()
}

fn supported_derives(attributes: &[syn::Attribute]) -> Option<Vec<String>> {
    let mut derives = Vec::new();
    for attribute in attributes
        .iter()
        .filter(|attribute| attribute.path().is_ident("derive"))
    {
        let paths = attribute
            .parse_args_with(
                syn::punctuated::Punctuated::<syn::Path, syn::Token![,]>::parse_terminated,
            )
            .ok()?;
        for path in paths {
            if path.leading_colon.is_some() || path.segments.len() != 1 {
                return None;
            }
            let name = path.segments.last()?.ident.unraw().to_string();
            if !matches!(
                name.as_str(),
                "Copy" | "Clone" | "Debug" | "PartialEq" | "Eq"
            ) {
                return None;
            }
            if !derives.contains(&name) {
                derives.push(name);
            }
        }
    }
    Some(derives)
}

fn field_supports_derive(ty: &syn::Type, trait_name: &str) -> bool {
    if trait_name == "Copy" {
        return match ty {
            syn::Type::Reference(reference) => reference.mutability.is_none(),
            syn::Type::Ptr(_) => true,
            syn::Type::Array(array) => field_supports_derive(&array.elem, trait_name),
            syn::Type::Tuple(tuple) => tuple
                .elems
                .iter()
                .all(|element| field_supports_derive(element, trait_name)),
            syn::Type::Path(path) if path.qself.is_none() => {
                path.path.segments.last().is_some_and(|segment| {
                    matches!(
                        segment.ident.unraw().to_string().as_str(),
                        "bool"
                            | "char"
                            | "u8"
                            | "u16"
                            | "u32"
                            | "u64"
                            | "u128"
                            | "usize"
                            | "i8"
                            | "i16"
                            | "i32"
                            | "i64"
                            | "i128"
                            | "isize"
                            | "f32"
                            | "f64"
                    ) && matches!(segment.arguments, syn::PathArguments::None)
                })
            }
            _ => false,
        };
    }
    match ty {
        syn::Type::Reference(reference) => field_supports_derive(&reference.elem, trait_name),
        syn::Type::Array(array) => field_supports_derive(&array.elem, trait_name),
        syn::Type::Tuple(tuple) => tuple
            .elems
            .iter()
            .all(|element| field_supports_derive(element, trait_name)),
        syn::Type::Path(path) if path.qself.is_none() => {
            let Some(segment) = path.path.segments.last() else {
                return false;
            };
            let name = segment.ident.unraw().to_string();
            match name.as_str() {
                "bool" | "char" | "u8" | "u16" | "u32" | "u64" | "u128" | "usize" | "i8"
                | "i16" | "i32" | "i64" | "i128" | "isize" | "String" => true,
                "f32" | "f64" => trait_name != "Eq",
                "Vec" | "VecDeque" | "BTreeSet" | "HashSet" => {
                    let syn::PathArguments::AngleBracketed(arguments) = &segment.arguments else {
                        return false;
                    };
                    arguments.args.iter().all(|argument| match argument {
                        syn::GenericArgument::Type(ty) => field_supports_derive(ty, trait_name),
                        syn::GenericArgument::Lifetime(_) => true,
                        _ => false,
                    })
                }
                "BTreeMap" | "HashMap" => {
                    let syn::PathArguments::AngleBracketed(arguments) = &segment.arguments else {
                        return false;
                    };
                    let types: Vec<_> = arguments
                        .args
                        .iter()
                        .filter_map(|argument| match argument {
                            syn::GenericArgument::Type(ty) => Some(ty),
                            _ => None,
                        })
                        .collect();
                    types.len() == 2 && types.iter().all(|ty| field_supports_derive(ty, trait_name))
                }
                _ => false,
            }
        }
        _ => false,
    }
}

#[derive(Default)]
struct CallCollector {
    calls: Vec<String>,
    qualified_calls: Vec<String>,
}

impl<'ast> syn::visit::Visit<'ast> for CallCollector {
    fn visit_expr_call(&mut self, call: &'ast syn::ExprCall) {
        if let syn::Expr::Path(path) = &*call.func
            && path.qself.is_none()
            && path.path.leading_colon.is_none()
            && path
                .path
                .segments
                .iter()
                .all(|segment| matches!(segment.arguments, syn::PathArguments::None))
        {
            let path = path
                .path
                .segments
                .iter()
                .map(|segment| segment.ident.unraw().to_string())
                .collect::<Vec<_>>();
            if path.len() == 1 {
                self.calls.push(path[0].clone());
            } else {
                self.qualified_calls.push(path.join("::"));
            }
        }
        syn::visit::visit_expr_call(self, call);
    }
}

fn flatten_ast_use(
    tree: &syn::UseTree,
    prefix: &[String],
    scope: &[String],
    span: &Range<usize>,
    is_public: bool,
    output: &mut Vec<ImportItem>,
) {
    match tree {
        syn::UseTree::Path(path) => {
            let mut path_prefix = prefix.to_vec();
            path_prefix.push(path.ident.unraw().to_string());
            flatten_ast_use(&path.tree, &path_prefix, scope, span, is_public, output);
        }
        syn::UseTree::Name(name) => {
            let mut path = prefix.to_vec();
            let self_import = name.ident == "self" && !path.is_empty();
            if !self_import {
                path.push(name.ident.unraw().to_string());
            }
            output.push(ImportItem {
                scope: scope.to_vec(),
                path,
                alias: None,
                is_glob: false,
                span: span.clone(),
                is_public,
            });
        }
        syn::UseTree::Rename(rename) => {
            let mut path = prefix.to_vec();
            let self_import = rename.ident == "self" && !path.is_empty();
            if !self_import {
                path.push(rename.ident.unraw().to_string());
            }
            output.push(ImportItem {
                scope: scope.to_vec(),
                path,
                alias: Some(rename.rename.to_string()),
                is_glob: false,
                span: span.clone(),
                is_public,
            });
        }
        syn::UseTree::Glob(_) => output.push(ImportItem {
            scope: scope.to_vec(),
            path: prefix.to_vec(),
            alias: None,
            is_glob: true,
            span: span.clone(),
            is_public,
        }),
        syn::UseTree::Group(group) => {
            for nested in &group.items {
                flatten_ast_use(nested, prefix, scope, span, is_public, output);
            }
        }
    }
}

fn item_attributes(item: &syn::Item) -> &[syn::Attribute] {
    match item {
        syn::Item::Const(x) => &x.attrs,
        syn::Item::Enum(x) => &x.attrs,
        syn::Item::ExternCrate(x) => &x.attrs,
        syn::Item::Fn(x) => &x.attrs,
        syn::Item::ForeignMod(x) => &x.attrs,
        syn::Item::Impl(x) => &x.attrs,
        syn::Item::Macro(x) => &x.attrs,
        syn::Item::Mod(x) => &x.attrs,
        syn::Item::Static(x) => &x.attrs,
        syn::Item::Struct(x) => &x.attrs,
        syn::Item::Trait(x) => &x.attrs,
        syn::Item::TraitAlias(x) => &x.attrs,
        syn::Item::Type(x) => &x.attrs,
        syn::Item::Union(x) => &x.attrs,
        syn::Item::Use(x) => &x.attrs,
        syn::Item::Verbatim(_) => &[],
        _ => &[],
    }
}

// Oracle correction: proc-macro2 columns count characters, while
// `SourceFile::byte_offset` takes byte columns. The pre-migration collector
// passed character columns through unchanged, which misplaced every span
// after a multibyte character on the same line. Convert properly so the
// oracle is a correct reference for rust-analyzer's byte spans.
fn span_range(source: &SourceFile, span: proc_macro2::Span) -> Range<usize> {
    fn offset(source: &SourceFile, location: proc_macro2::LineColumn) -> usize {
        let line_start = source.byte_offset(location.line, 0);
        let line = &source.text()[line_start..];
        line_start
            + line
                .char_indices()
                .nth(location.column)
                .map_or(line.len(), |(offset, _)| offset)
    }
    offset(source, span.start())..offset(source, span.end())
}

fn parse_i32_discriminant(expression: &syn::Expr) -> Option<i32> {
    match expression {
        syn::Expr::Lit(literal) => match &literal.lit {
            syn::Lit::Int(value) => value.base10_parse().ok(),
            _ => None,
        },
        syn::Expr::Unary(unary) if matches!(unary.op, syn::UnOp::Neg(_)) => {
            let syn::Expr::Lit(literal) = &*unary.expr else {
                return None;
            };
            let syn::Lit::Int(value) = &literal.lit else {
                return None;
            };
            value.base10_parse::<i32>().ok()?.checked_neg()
        }
        _ => None,
    }
}

fn lower_block(block: &syn::Block, source: &SourceFile) -> BodyExpr {
    let mut statements = Vec::with_capacity(block.stmts.len());
    let mut tail = None;
    for (index, statement) in block.stmts.iter().enumerate() {
        match statement {
            syn::Stmt::Local(local) => {
                if let (syn::Pat::Tuple(pattern), Some(initializer)) =
                    (&local.pat, local.init.as_ref())
                {
                    let names = pattern
                        .elems
                        .iter()
                        .map(|element| match element {
                            syn::Pat::Ident(binding)
                                if binding.mutability.is_none()
                                    && binding.by_ref.is_none()
                                    && binding.subpat.is_none() =>
                            {
                                Some(binding.ident.unraw().to_string())
                            }
                            _ => None,
                        })
                        .collect::<Option<Vec<_>>>();
                    if let Some(names) = names {
                        statements.push(BodyStatement::LetTuple {
                            names,
                            initializer: lower_expr(&initializer.expr, source),
                            span: span_range(source, local.span()),
                        });
                        continue;
                    }
                }
                let (name, type_text, mutable) = match &local.pat {
                    syn::Pat::Ident(pattern) => (
                        Some(pattern.ident.unraw().to_string()),
                        None,
                        pattern.mutability.is_some(),
                    ),
                    syn::Pat::Type(pattern) => {
                        let (name, mutable) = match &*pattern.pat {
                            syn::Pat::Ident(ident) => (
                                Some(ident.ident.unraw().to_string()),
                                ident.mutability.is_some(),
                            ),
                            _ => (None, false),
                        };
                        (
                            name,
                            Some(
                                source
                                    .slice(span_range(source, pattern.ty.span()))
                                    .to_owned(),
                            ),
                            mutable,
                        )
                    }
                    _ => (None, None, false),
                };
                statements.push(BodyStatement::Let {
                    name,
                    type_text,
                    mutable,
                    initializer: local
                        .init
                        .as_ref()
                        .map(|initializer| lower_expr(&initializer.expr, source)),
                    span: span_range(source, local.span()),
                });
            }
            syn::Stmt::Expr(expression, semicolon) => match expression {
                syn::Expr::Break(expr) if expr.expr.is_none() => statements
                    .push(BodyStatement::Break {
                        label: expr.label.as_ref().map(|label| {
                            source
                                .slice(span_range(source, label.span()))
                                .to_owned()
                        }),
                        span: span_range(source, expr.span()),
                    }),
                syn::Expr::Continue(expr) => {
                    statements.push(BodyStatement::Continue {
                        label: expr.label.as_ref().map(|label| {
                            source
                                .slice(span_range(source, label.span()))
                                .to_owned()
                        }),
                        span: span_range(source, expr.span()),
                    })
                }
                syn::Expr::Break(_) => {
                    statements.push(BodyStatement::Unsupported)
                }
                syn::Expr::Assign(assign) => {
                    let target = match &*assign.left {
                        syn::Expr::Path(path)
                            if path.qself.is_none() && path.path.segments.len() == 1 =>
                        {
                            Some(path.path.segments[0].ident.unraw().to_string())
                        }
                        // Mirrors the post-migration extension of assignment
                        // targets: field, index and dereference places keep
                        // their source text.
                        syn::Expr::Field(_) | syn::Expr::Index(_) => Some(
                            source
                                .slice(span_range(source, assign.left.span()))
                                .to_owned(),
                        ),
                        syn::Expr::Unary(unary) if matches!(unary.op, syn::UnOp::Deref(_)) => Some(
                            source
                                .slice(span_range(source, assign.left.span()))
                                .to_owned(),
                        ),
                        _ => None,
                    };
                    if let Some(name) = target {
                        statements.push(BodyStatement::Assign {
                            name,
                            value: lower_expr(&assign.right, source),
                            span: span_range(source, assign.span()),
                        });
                        continue;
                    }
                    statements.push(BodyStatement::Unsupported);
                }
                syn::Expr::While(while_expr) => statements.push(BodyStatement::While {
                    label: while_expr.label.as_ref().map(|label| {
                        source
                            .slice(span_range(source, label.name.span()))
                            .to_owned()
                    }),
                    condition: lower_expr(&while_expr.cond, source),
                    body: lower_block(&while_expr.body, source),
                    span: span_range(source, while_expr.span()),
                }),
                syn::Expr::Loop(loop_expr) => {
                    statements.push(BodyStatement::Loop {
                        label: loop_expr.label.as_ref().map(|label| {
                            source
                                .slice(span_range(source, label.name.span()))
                                .to_owned()
                        }),
                        body: lower_block(&loop_expr.body, source),
                        span: span_range(source, loop_expr.span()),
                    });
                }
                syn::Expr::Return(_) => statements.push(BodyStatement::Expression {
                    expression: lower_expr(expression, source),
                    has_semicolon: semicolon.is_some(),
                }),
                syn::Expr::ForLoop(for_expr) => {
                    let name = match &*for_expr.pat {
                        syn::Pat::Ident(pattern) if pattern.subpat.is_none() => {
                            Some(pattern.ident.unraw().to_string())
                        }
                        _ => None,
                    };
                    let range = match &*for_expr.expr {
                        syn::Expr::Range(range) => Some(range),
                        _ => None,
                    };
                    if let (Some(name), Some(range), Some(start), Some(end)) = (
                        name,
                        range,
                        range.and_then(|range| range.start.as_deref()),
                        range.and_then(|range| range.end.as_deref()),
                    ) {
                        statements.push(BodyStatement::ForRange {
                            label: for_expr.label.as_ref().map(|label| {
                                source
                                    .slice(span_range(source, label.name.span()))
                                    .to_owned()
                            }),
                            name,
                            start: lower_expr(start, source),
                            end: lower_expr(end, source),
                            inclusive: matches!(range.limits, syn::RangeLimits::Closed(_)),
                            body: lower_block(&for_expr.body, source),
                            span: span_range(source, for_expr.span()),
                        });
                    } else {
                        let names = match &*for_expr.pat {
                            syn::Pat::Ident(pattern)
                                if pattern.by_ref.is_none() && pattern.subpat.is_none() =>
                            {
                                Some(vec![pattern.ident.unraw().to_string()])
                            }
                            syn::Pat::Tuple(pattern) => pattern
                                .elems
                                .iter()
                                .map(|element| match element {
                                    syn::Pat::Ident(pattern)
                                        if pattern.by_ref.is_none() && pattern.subpat.is_none() =>
                                    {
                                        Some(pattern.ident.unraw().to_string())
                                    }
                                    _ => None,
                                })
                                .collect(),
                            _ => None,
                        };
                        if let Some(names) = names {
                            statements.push(BodyStatement::ForEach {
                                label: for_expr.label.as_ref().map(|label| {
                                    source
                                        .slice(span_range(source, label.name.span()))
                                        .to_owned()
                                }),
                                names,
                                iterator: lower_expr(&for_expr.expr, source),
                                body: lower_block(&for_expr.body, source),
                                span: span_range(source, for_expr.span()),
                            });
                        } else {
                            statements.push(BodyStatement::Unsupported);
                        }
                    }
                }
                _ => {
                    let lowered = lower_expr(expression, source);
                    let is_statement_if = matches!(expression, syn::Expr::If(if_expr) if if_expr.else_branch.is_none());
                    if semicolon.is_none() && index + 1 == block.stmts.len() && !is_statement_if {
                        tail = Some(Box::new(lowered));
                    } else {
                        statements.push(BodyStatement::Expression {
                            expression: lowered,
                            has_semicolon: semicolon.is_some(),
                        });
                    }
                }
            },
            syn::Stmt::Macro(statement_macro) => {
                let macro_expression = lower_macro_expression(&statement_macro.mac, source);
                let span = span_range(source, statement_macro.span());
                let (line, column) = source.line_column(span.start);
                statements.push(BodyStatement::MacroInvocation {
                    path: macro_expression.path,
                    tokens: macro_expression.tokens,
                    format_string: macro_expression.format_string,
                    arguments: macro_expression.arguments,
                    span,
                    source_path: source.path().to_string_lossy().into_owned(),
                    line,
                    column,
                });
            }
            syn::Stmt::Item(_) => statements.push(BodyStatement::Unsupported),
        }
    }
    BodyExpr {
        kind: BodyExprKind::Block { statements, tail },
        span: span_range(source, block.span()),
    }
}

fn lower_expr(expression: &syn::Expr, source: &SourceFile) -> BodyExpr {
    let span = span_range(source, expression.span());
    let kind = match expression {
        syn::Expr::Lit(literal) => match &literal.lit {
            syn::Lit::Int(value) => BodyExprKind::Literal(LiteralKind::Integer {
                text: value.base10_digits().to_owned(),
                suffix: value.suffix().to_owned(),
            }),
            syn::Lit::Float(value) => BodyExprKind::Literal(LiteralKind::Float {
                text: value.base10_digits().to_owned(),
                suffix: value.suffix().to_owned(),
            }),
            syn::Lit::Bool(value) => BodyExprKind::Literal(LiteralKind::Boolean(value.value)),
            syn::Lit::Char(value) => BodyExprKind::Literal(LiteralKind::Character(value.value())),
            syn::Lit::Byte(_) => BodyExprKind::Unsupported,
            syn::Lit::Str(value) => BodyExprKind::Literal(LiteralKind::String {
                value: value.value(),
            }),
            syn::Lit::ByteStr(_) | syn::Lit::CStr(_) => BodyExprKind::Unsupported,
            _ => BodyExprKind::Unsupported,
        },
        syn::Expr::Path(path) if path.qself.is_none() => BodyExprKind::Path(
            path.path
                .segments
                .iter()
                .map(|segment| segment.ident.unraw().to_string())
                .collect::<Vec<_>>()
                .join("::"),
        ),
        syn::Expr::Call(call) => {
            let callee = match &*call.func {
                syn::Expr::Path(path)
                    if path.qself.is_none()
                        && path.path.segments.iter().all(|segment| {
                            matches!(segment.arguments, syn::PathArguments::None)
                                || (std::ptr::eq(
                                    segment,
                                    path.path.segments.last().expect("path has a last segment"),
                                ) && matches!(
                                    segment.arguments,
                                    syn::PathArguments::AngleBracketed(_)
                                ))
                        }) =>
                {
                    path.path
                        .segments
                        .iter()
                        .map(|segment| segment.ident.unraw().to_string())
                        .collect::<Vec<_>>()
                        .join("::")
                }
                _ => String::new(),
            };
            let generic_arguments = match &*call.func {
                syn::Expr::Path(path) if path.qself.is_none() => path
                    .path
                    .segments
                    .last()
                    .and_then(|segment| match &segment.arguments {
                        syn::PathArguments::AngleBracketed(arguments) => Some(
                            arguments
                                .args
                                .iter()
                                .map(|argument| match argument {
                                    syn::GenericArgument::Type(syn::Type::Path(path))
                                        if path.qself.is_none()
                                            && path.path.segments.iter().all(|segment| {
                                                matches!(
                                                    segment.arguments,
                                                    syn::PathArguments::None
                                                )
                                            }) =>
                                    {
                                        path.path
                                            .segments
                                            .iter()
                                            .map(|segment| segment.ident.unraw().to_string())
                                            .collect::<Vec<_>>()
                                            .join("::")
                                    }
                                    _ => String::new(),
                                })
                                .collect(),
                        ),
                        _ => None,
                    })
                    .unwrap_or_default(),
                _ => Vec::new(),
            };
            BodyExprKind::Call {
                callee,
                generic_arguments,
                arguments: call
                    .args
                    .iter()
                    .map(|argument| lower_expr(argument, source))
                    .collect(),
            }
        }
        syn::Expr::MethodCall(call) => BodyExprKind::MethodCall {
            receiver: Box::new(lower_expr(&call.receiver, source)),
            method: call.method.to_string(),
            arguments: call
                .args
                .iter()
                .map(|argument| lower_expr(argument, source))
                .collect(),
        },
        syn::Expr::Closure(closure) => {
            let parameters: Option<Vec<_>> = closure
                .inputs
                .iter()
                .map(|pattern| match pattern {
                    syn::Pat::Ident(ident) if ident.by_ref.is_none() && ident.subpat.is_none() => {
                        Some(ClosureParameter::Identifier(
                            ident.ident.unraw().to_string(),
                        ))
                    }
                    syn::Pat::Tuple(tuple) => tuple
                        .elems
                        .iter()
                        .map(|element| match element {
                            syn::Pat::Ident(ident)
                                if ident.by_ref.is_none() && ident.subpat.is_none() =>
                            {
                                Some(ident.ident.unraw().to_string())
                            }
                            _ => None,
                        })
                        .collect::<Option<Vec<_>>>()
                        .map(ClosureParameter::Tuple),
                    _ => None,
                })
                .collect();
            match parameters {
                Some(parameters) => BodyExprKind::Closure {
                    parameters,
                    body: Box::new(lower_expr(&closure.body, source)),
                    move_capture: closure.capture.is_some(),
                },
                None => BodyExprKind::Unsupported,
            }
        }
        syn::Expr::Struct(structure) if structure.qself.is_none() && structure.rest.is_none() => {
            BodyExprKind::StructLiteral {
                path: structure
                    .path
                    .segments
                    .iter()
                    .map(|segment| segment.ident.unraw().to_string())
                    .collect::<Vec<_>>()
                    .join("::"),
                fields: structure
                    .fields
                    .iter()
                    .filter_map(|field| match &field.member {
                        syn::Member::Named(name) => {
                            Some((name.to_string(), lower_expr(&field.expr, source)))
                        }
                        syn::Member::Unnamed(_) => None,
                    })
                    .collect(),
            }
        }
        syn::Expr::Field(field) => match &field.member {
            syn::Member::Named(name) => BodyExprKind::FieldAccess {
                base: Box::new(lower_expr(&field.base, source)),
                field: name.to_string(),
            },
            syn::Member::Unnamed(index) => BodyExprKind::FieldAccess {
                base: Box::new(lower_expr(&field.base, source)),
                field: index.index.to_string(),
            },
        },
        syn::Expr::Index(index) => {
            let span = span_range(source, index.span());
            let base_span = span_range(source, index.expr.span());
            let (line, column) = source.line_column(span.start);
            let (base_line, base_column) = source.line_column(base_span.start);
            BodyExprKind::Index {
                base: Box::new(lower_expr(&index.expr, source)),
                index: Box::new(lower_expr(&index.index, source)),
                source_path: source.path().to_string_lossy().into_owned(),
                line,
                column,
                base_line,
                base_column,
            }
        }
        syn::Expr::Array(array) => BodyExprKind::FixedArrayLiteral(
            array
                .elems
                .iter()
                .map(|element| lower_expr(element, source))
                .collect(),
        ),
        syn::Expr::Repeat(repeat) => BodyExprKind::ArrayRepeat {
            element: Box::new(lower_expr(&repeat.expr, source)),
            length: match &*repeat.len {
                syn::Expr::Lit(literal) => match &literal.lit {
                    syn::Lit::Int(length) => length.base10_parse::<usize>().ok(),
                    _ => None,
                },
                _ => None,
            },
        },
        syn::Expr::Binary(binary) => BodyExprKind::Binary {
            operator: lower_binary_operator(&binary.op),
            left: Box::new(lower_expr(&binary.left, source)),
            right: Box::new(lower_expr(&binary.right, source)),
        },
        syn::Expr::Unary(unary) if matches!(unary.op, syn::UnOp::Neg(_)) => {
            BodyExprKind::Negate(Box::new(lower_expr(&unary.expr, source)))
        }
        syn::Expr::Unary(unary) if matches!(unary.op, syn::UnOp::Not(_)) => {
            BodyExprKind::Not(Box::new(lower_expr(&unary.expr, source)))
        }
        syn::Expr::Unary(unary) if matches!(unary.op, syn::UnOp::Deref(_)) => {
            BodyExprKind::Dereference(Box::new(lower_expr(&unary.expr, source)))
        }
        syn::Expr::Try(try_expression) => {
            BodyExprKind::Try(Box::new(lower_expr(&try_expression.expr, source)))
        }
        syn::Expr::Block(block) => lower_block(&block.block, source).kind,
        syn::Expr::Paren(paren) => {
            BodyExprKind::Parenthesized(Box::new(lower_expr(&paren.expr, source)))
        }
        syn::Expr::If(branch) => {
            let condition = match &*branch.cond {
                syn::Expr::Let(let_expression)
                    if repeatable_if_let_scrutinee(&let_expression.expr) =>
                {
                    lower_if_let_condition(&let_expression.pat, &let_expression.expr, source)
                        .unwrap_or_else(|| BodyExpr {
                            kind: BodyExprKind::Unsupported,
                            span: span_range(source, branch.cond.span()),
                        })
                }
                syn::Expr::Let(_) => BodyExpr {
                    kind: BodyExprKind::Unsupported,
                    span: span_range(source, branch.cond.span()),
                },
                _ => lower_expr(&branch.cond, source),
            };
            BodyExprKind::If {
                condition: Box::new(condition),
                then_branch: Box::new(lower_block(&branch.then_branch, source)),
                else_branch: branch
                    .else_branch
                    .as_ref()
                    .map(|(_, otherwise)| Box::new(lower_expr(otherwise, source))),
            }
        }
        syn::Expr::Match(branch) => BodyExprKind::Match {
            scrutinee: Box::new(lower_expr(&branch.expr, source)),
            arms: branch
                .arms
                .iter()
                .map(|arm| MatchArm {
                    pattern: lower_match_pattern(&arm.pat),
                    body: lower_expr(&arm.body, source),
                    span: span_range(source, arm.span()),
                })
                .collect(),
        },
        syn::Expr::Return(returned) => BodyExprKind::Return(
            returned
                .expr
                .as_ref()
                .map(|value| Box::new(lower_expr(value, source))),
        ),
        syn::Expr::Macro(macro_expression) => {
            BodyExprKind::MacroInvocation(lower_macro_expression(&macro_expression.mac, source))
        }
        syn::Expr::Reference(reference) if matches!(&*reference.expr, syn::Expr::Array(array) if array.elems.is_empty()) => {
            BodyExprKind::EmptySliceReference
        }
        syn::Expr::Reference(reference) if let syn::Expr::Array(array) = &*reference.expr => {
            BodyExprKind::ArrayLiteral(
                array
                    .elems
                    .iter()
                    .map(|element| lower_expr(element, source))
                    .collect(),
            )
        }
        syn::Expr::Reference(reference) => BodyExprKind::Reference {
            mutable: reference.mutability.is_some(),
            expression: Box::new(lower_expr(&reference.expr, source)),
        },
        syn::Expr::Cast(cast) => BodyExprKind::Cast {
            expression: Box::new(lower_expr(&cast.expr, source)),
            type_name: source.slice(span_range(source, cast.ty.span())).to_owned(),
        },
        syn::Expr::Tuple(tuple) if tuple.elems.is_empty() => BodyExprKind::Unit,
        syn::Expr::Tuple(tuple) => BodyExprKind::Tuple(
            tuple
                .elems
                .iter()
                .map(|element| lower_expr(element, source))
                .collect(),
        ),
        syn::Expr::Group(group) => {
            BodyExprKind::Parenthesized(Box::new(lower_expr(&group.expr, source)))
        }
        _ => BodyExprKind::Unsupported,
    };
    BodyExpr { kind, span }
}

fn lower_match_pattern(pattern: &syn::Pat) -> MatchPattern {
    match pattern {
        syn::Pat::Or(patterns) => {
            MatchPattern::Or(patterns.cases.iter().map(lower_match_pattern).collect())
        }
        syn::Pat::Guard(_) => MatchPattern::Unsupported,
        syn::Pat::Range(range) => match (
            range
                .start
                .as_deref()
                .and_then(lower_pattern_range_endpoint),
            range.end.as_deref().and_then(lower_pattern_range_endpoint),
        ) {
            (Some(start), Some(end)) => MatchPattern::Range {
                start,
                end,
                inclusive: matches!(range.limits, syn::RangeLimits::Closed(_)),
            },
            _ => MatchPattern::Unsupported,
        },
        syn::Pat::Wild(_) => MatchPattern::Wildcard,
        syn::Pat::Ident(pattern)
            if pattern.by_ref.is_none()
                && pattern.mutability.is_none()
                && pattern.subpat.is_none() =>
        {
            MatchPattern::Binding(pattern.ident.unraw().to_string())
        }
        syn::Pat::Path(path) if path.qself.is_none() => MatchPattern::Path(
            path.path
                .segments
                .iter()
                .map(|segment| segment.ident.unraw().to_string())
                .collect::<Vec<_>>()
                .join("::"),
        ),
        syn::Pat::TupleStruct(pattern) if pattern.qself.is_none() => MatchPattern::Destructure {
            path: pattern
                .path
                .segments
                .iter()
                .map(|segment| segment.ident.unraw().to_string())
                .collect::<Vec<_>>()
                .join("::"),
            bindings: pattern.elems.iter().flat_map(pattern_binding_names).collect(),
        },
        syn::Pat::Struct(pattern) if pattern.qself.is_none() => MatchPattern::Destructure {
            path: pattern
                .path
                .segments
                .iter()
                .map(|segment| segment.ident.unraw().to_string())
                .collect::<Vec<_>>()
                .join("::"),
            bindings: pattern
                .fields
                .iter()
                .flat_map(|field| pattern_binding_names(&field.pat))
                .collect(),
        },
        syn::Pat::Lit(literal) => match &literal.lit {
            syn::Lit::Int(value) => MatchPattern::Literal(LiteralKind::Integer {
                text: value.base10_digits().to_owned(),
                suffix: value.suffix().to_owned(),
            }),
            syn::Lit::Bool(value) => MatchPattern::Literal(LiteralKind::Boolean(value.value)),
            syn::Lit::Char(value) => MatchPattern::Literal(LiteralKind::Character(value.value())),
            _ => MatchPattern::Unsupported,
        },
        _ => MatchPattern::Unsupported,
    }
}

fn pattern_binding_names(pattern: &syn::Pat) -> Vec<String> {
    match pattern {
        syn::Pat::Ident(binding)
            if binding.by_ref.is_none()
                && binding.mutability.is_none()
                && binding.subpat.is_none() =>
        {
            vec![binding.ident.unraw().to_string()]
        }
        syn::Pat::Tuple(tuple) => tuple.elems.iter().flat_map(pattern_binding_names).collect(),
        syn::Pat::TupleStruct(tuple) => tuple.elems.iter().flat_map(pattern_binding_names).collect(),
        syn::Pat::Struct(structure) => structure
            .fields
            .iter()
            .flat_map(|field| pattern_binding_names(&field.pat))
            .collect(),
        _ => Vec::new(),
    }
}

fn lower_pattern_range_endpoint(expression: &syn::Expr) -> Option<LiteralKind> {
    match expression {
        syn::Expr::Lit(literal) => match &literal.lit {
            syn::Lit::Int(value) => Some(LiteralKind::Integer {
                text: value.base10_digits().to_owned(),
                suffix: value.suffix().to_owned(),
            }),
            syn::Lit::Char(value) => Some(LiteralKind::Character(value.value())),
            _ => None,
        },
        syn::Expr::Unary(unary) if matches!(unary.op, syn::UnOp::Neg(_)) => {
            let syn::Expr::Lit(literal) = &*unary.expr else {
                return None;
            };
            let syn::Lit::Int(value) = &literal.lit else {
                return None;
            };
            Some(LiteralKind::Integer {
                text: format!("-{}", value.base10_digits()),
                suffix: value.suffix().to_owned(),
            })
        }
        _ => None,
    }
}

fn repeatable_if_let_scrutinee(expression: &syn::Expr) -> bool {
    match expression {
        syn::Expr::Path(path) => path.qself.is_none(),
        syn::Expr::Lit(_) => true,
        syn::Expr::Paren(paren) => repeatable_if_let_scrutinee(&paren.expr),
        _ => false,
    }
}

fn lower_if_let_condition(
    pattern: &syn::Pat,
    scrutinee: &syn::Expr,
    source: &SourceFile,
) -> Option<BodyExpr> {
    let scrutinee = lower_expr(scrutinee, source);
    lower_if_let_pattern_condition(pattern, &scrutinee, source)
}

fn lower_if_let_pattern_condition(
    pattern: &syn::Pat,
    scrutinee: &BodyExpr,
    source: &SourceFile,
) -> Option<BodyExpr> {
    let span = span_range(source, pattern.span());
    let equality = |right| BodyExpr {
        kind: BodyExprKind::Binary {
            operator: BinaryOperator::PatternEqual,
            left: Box::new(scrutinee.clone()),
            right: Box::new(right),
        },
        span: span.clone(),
    };
    match lower_match_pattern(pattern) {
        MatchPattern::Literal(literal) => Some(equality(BodyExpr {
            kind: BodyExprKind::Literal(literal),
            span: span.clone(),
        })),
        MatchPattern::Path(path) => Some(equality(BodyExpr {
            kind: BodyExprKind::Path(path),
            span: span.clone(),
        })),
        MatchPattern::Or(_) => {
            let syn::Pat::Or(patterns) = pattern else {
                unreachable!("only syn or-patterns lower to an or-pattern")
            };
            let mut conditions = patterns
                .cases
                .iter()
                .map(|pattern| lower_if_let_pattern_condition(pattern, scrutinee, source));
            let first = conditions.next()??;
            conditions.try_fold(first, |left, right| {
                Some(BodyExpr {
                    kind: BodyExprKind::Binary {
                        operator: BinaryOperator::Or,
                        left: Box::new(left),
                        right: Box::new(right?),
                    },
                    span: span.clone(),
                })
            })
        }
        MatchPattern::Range {
            start,
            end,
            inclusive,
        } => {
            let lower = BodyExpr {
                kind: BodyExprKind::Binary {
                    operator: BinaryOperator::GreaterEqual,
                    left: Box::new(scrutinee.clone()),
                    right: Box::new(BodyExpr {
                        kind: BodyExprKind::Literal(start),
                        span: span.clone(),
                    }),
                },
                span: span.clone(),
            };
            let upper = BodyExpr {
                kind: BodyExprKind::Binary {
                    operator: if inclusive {
                        BinaryOperator::LessEqual
                    } else {
                        BinaryOperator::Less
                    },
                    left: Box::new(scrutinee.clone()),
                    right: Box::new(BodyExpr {
                        kind: BodyExprKind::Literal(end),
                        span: span.clone(),
                    }),
                },
                span: span.clone(),
            };
            Some(BodyExpr {
                kind: BodyExprKind::Binary {
                    operator: BinaryOperator::And,
                    left: Box::new(lower),
                    right: Box::new(upper),
                },
                span,
            })
        }
        MatchPattern::Wildcard
        | MatchPattern::Binding(_)
        | MatchPattern::EnumVariant { .. }
        | MatchPattern::Destructure { .. }
        | MatchPattern::Unsupported => None,
    }
}

fn lower_macro_expression(mac: &syn::Macro, source: &SourceFile) -> MacroExpression {
    let parsed_arguments =
        syn::punctuated::Punctuated::<syn::Expr, syn::Token![,]>::parse_terminated
            .parse2(mac.tokens.clone())
            .ok();
    let format_string = parsed_arguments.as_ref().and_then(|arguments| {
        arguments.first().and_then(|argument| match argument {
            syn::Expr::Lit(literal) => match &literal.lit {
                syn::Lit::Str(value) => Some(value.value()),
                _ => None,
            },
            _ => None,
        })
    });
    let arguments = parsed_arguments
        .as_ref()
        .map(|arguments| {
            if mac.path.is_ident("stringify") {
                return Vec::new();
            }
            arguments
                .iter()
                .skip(1)
                .map(|argument| lower_expr(argument, source))
                .collect()
        })
        .unwrap_or_default();
    let path = mac
        .path
        .segments
        .iter()
        .map(|segment| segment.ident.unraw().to_string())
        .collect::<Vec<_>>()
        .join("::");
    let const_expansion = match path.as_str() {
        "concat" => parsed_arguments.as_ref().and_then(|arguments| {
            arguments
                .iter()
                .map(|argument| match argument {
                    syn::Expr::Lit(expression) => match &expression.lit {
                        syn::Lit::Str(value) => Some(value.value()),
                        syn::Lit::Char(value) => Some(value.value().to_string()),
                        syn::Lit::Int(value) => Some(value.base10_digits().to_owned()),
                        syn::Lit::Float(value) => Some(value.base10_digits().to_owned()),
                        syn::Lit::Bool(value) => Some(value.value.to_string()),
                        _ => None,
                    },
                    _ => None,
                })
                .collect::<Option<Vec<_>>>()
                .map(|parts| parts.concat())
        }),
        "stringify" => source.text().get(mac.tokens.span().byte_range()).and_then(|text| crate::stringify_tokens(text, source)),
        _ => None,
    };
    MacroExpression {
        path,
        tokens: mac.tokens.to_string(),
        format_string,
        arguments,
        const_expansion,
    }
}

fn lower_binary_operator(operator: &syn::BinOp) -> BinaryOperator {
    match operator {
        syn::BinOp::Add(_) => BinaryOperator::Add,
        syn::BinOp::Sub(_) => BinaryOperator::Subtract,
        syn::BinOp::Mul(_) => BinaryOperator::Multiply,
        syn::BinOp::Div(_) => BinaryOperator::Divide,
        syn::BinOp::Rem(_) => BinaryOperator::Remainder,
        syn::BinOp::Shl(_) => BinaryOperator::ShiftLeft,
        syn::BinOp::Shr(_) => BinaryOperator::ShiftRight,
        syn::BinOp::BitXor(_) => BinaryOperator::BitXor,
        syn::BinOp::BitOr(_) => BinaryOperator::BitOr,
        syn::BinOp::BitAnd(_) => BinaryOperator::BitAnd,
        syn::BinOp::Eq(_) => BinaryOperator::Equal,
        syn::BinOp::Ne(_) => BinaryOperator::NotEqual,
        syn::BinOp::Lt(_) => BinaryOperator::Less,
        syn::BinOp::Le(_) => BinaryOperator::LessEqual,
        syn::BinOp::Gt(_) => BinaryOperator::Greater,
        syn::BinOp::Ge(_) => BinaryOperator::GreaterEqual,
        syn::BinOp::And(_) => BinaryOperator::And,
        syn::BinOp::Or(_) => BinaryOperator::Or,
        _ => BinaryOperator::Unsupported,
    }
}

// Syn-based cfg evaluation copied from `quill-cfg` before its migration, so
// the oracle stays independent of that crate's attribute API.
fn matches_meta(cfg: &quill_cfg::CfgSet, meta: &syn::Meta) -> bool {
    match meta {
        syn::Meta::Path(path) => cfg.has_flag(&cfg_path_string(path)),
        syn::Meta::NameValue(name_value) => {
            let syn::Expr::Lit(expression) = &name_value.value else {
                return false;
            };
            let syn::Lit::Str(value) = &expression.lit else {
                return false;
            };
            cfg.has_value(&cfg_path_string(&name_value.path), &value.value())
        }
        syn::Meta::List(list) => {
            let Ok(items) = syn::punctuated::Punctuated::<syn::Meta, syn::Token![,]>::parse_terminated
                .parse2(list.tokens.clone())
            else {
                return false;
            };
            match cfg_path_string(&list.path).as_str() {
                "all" => items.iter().all(|item| matches_meta(cfg, item)),
                "any" => items.iter().any(|item| matches_meta(cfg, item)),
                "not" if items.len() == 1 => !matches_meta(cfg, &items[0]),
                _ => false,
            }
        }
    }
}

fn attributes_enabled(cfg: &quill_cfg::CfgSet, attributes: &[syn::Attribute]) -> bool {
    attributes
        .iter()
        .all(|attribute| meta_attribute_enabled(cfg, &attribute.meta, true))
}

fn meta_attribute_enabled(cfg: &quill_cfg::CfgSet, meta: &syn::Meta, top_level: bool) -> bool {
    match meta {
        syn::Meta::List(list) if list.path.is_ident("cfg") => {
            syn::parse2::<syn::Meta>(list.tokens.clone())
                .is_ok_and(|condition| matches_meta(cfg, &condition))
        }
        syn::Meta::List(list) if list.path.is_ident("cfg_attr") => {
            let Ok(items) = syn::punctuated::Punctuated::<syn::Meta, syn::Token![,]>::parse_terminated
                .parse2(list.tokens.clone())
            else {
                return false;
            };
            let Some(condition) = items.first() else {
                return false;
            };
            !matches_meta(cfg, condition)
                || items
                    .iter()
                    .skip(1)
                    .filter(|meta| {
                        !top_level || meta.path().is_ident("cfg") || meta.path().is_ident("cfg_attr")
                    })
                    .all(|meta| meta_attribute_enabled(cfg, meta, false))
        }
        _ => true,
    }
}

fn active_string_attribute(
    cfg: &quill_cfg::CfgSet,
    attributes: &[syn::Attribute],
    name: &str,
) -> Option<String> {
    attributes
        .iter()
        .find_map(|attribute| active_string_meta(cfg, &attribute.meta, name))
}

fn active_string_meta(cfg: &quill_cfg::CfgSet, meta: &syn::Meta, name: &str) -> Option<String> {
    match meta {
        syn::Meta::NameValue(value) if value.path.is_ident(name) => {
            let syn::Expr::Lit(expression) = &value.value else {
                return None;
            };
            let syn::Lit::Str(value) = &expression.lit else {
                return None;
            };
            Some(value.value())
        }
        syn::Meta::List(list) if list.path.is_ident("cfg_attr") => {
            let items = syn::punctuated::Punctuated::<syn::Meta, syn::Token![,]>::parse_terminated
                .parse2(list.tokens.clone())
                .ok()?;
            let condition = items.first()?;
            if !matches_meta(cfg, condition) {
                return None;
            }
            items
                .iter()
                .skip(1)
                .find_map(|nested| active_string_meta(cfg, nested, name))
        }
        _ => None,
    }
}

fn cfg_path_string(path: &syn::Path) -> String {
    path.segments
        .iter()
        .map(|segment| segment.ident.to_string())
        .collect::<Vec<_>>()
        .join("::")
}
