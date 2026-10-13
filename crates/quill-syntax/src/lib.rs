use std::ops::Range;
use std::os::unix::fs::MetadataExt;
use std::sync::OnceLock;

use quill_source::SourceFile;
use serde::{Deserialize, Serialize};
use syn::ext::IdentExt;
use syn::parse::Parser;
use syn::spanned::Spanned;
use syn::visit::Visit;
use syn::visit_mut::VisitMut;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TokenKind {
    Identifier,
    Lifetime,
    Number,
    String,
    Character,
    Punctuation,
    DocComment,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Token {
    pub kind: TokenKind,
    pub span: Range<usize>,
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
pub struct Diagnostic {
    pub span: Range<usize>,
    pub message: String,
}

#[derive(Clone, Debug, Default)]
pub struct Lexed {
    pub tokens: Vec<Token>,
    pub diagnostics: Vec<Diagnostic>,
    pub had_error: bool,
}

pub fn lex(source: &SourceFile) -> Lexed {
    Lexer::new(source.text()).run()
}

pub fn lex_files(pool: &quill_parallel::WorkPool, sources: &[SourceFile]) -> Vec<Lexed> {
    pool.map(sources, lex)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Delimiter {
    Parenthesis,
    Brace,
    Bracket,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TokenTree {
    Token(Token),
    Group {
        delimiter: Delimiter,
        span: Range<usize>,
        children: Vec<TokenTree>,
    },
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct Parse {
    #[serde(skip)]
    pub items: Vec<TokenTree>,
    pub functions: Vec<FunctionItem>,
    pub structs: Vec<StructItem>,
    pub enums: Vec<EnumItem>,
    pub modules: Vec<ModuleItem>,
    pub imports: Vec<ImportItem>,
    pub unsupported_item_count: usize,
    pub diagnostics: Vec<Diagnostic>,
    pub had_error: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
pub struct EnumItem {
    pub name: String,
    pub scope: Vec<String>,
    pub variants: Vec<EnumVariantItem>,
    pub derived_traits: Vec<String>,
    pub span: Range<usize>,
    pub is_public: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
pub struct EnumVariantItem {
    pub name: String,
    pub discriminant: i32,
    pub span: Range<usize>,
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
pub struct StructItem {
    pub name: String,
    pub scope: Vec<String>,
    pub fields: Vec<StructFieldItem>,
    pub generic_parameters: Vec<String>,
    pub derived_traits: Vec<String>,
    pub repr_c: bool,
    pub repr_layout_supported: bool,
    pub span: Range<usize>,
    pub is_public: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
pub struct StructFieldItem {
    pub name: Option<String>,
    pub type_text: String,
    pub span: Range<usize>,
    pub is_public: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
pub struct ModuleItem {
    pub name: String,
    pub parent_scope: Vec<String>,
    pub span: Range<usize>,
    pub body_span: Option<Range<usize>>,
    pub path_override: Option<String>,
    pub is_public: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
pub struct ImportItem {
    pub scope: Vec<String>,
    pub path: Vec<String>,
    pub alias: Option<String>,
    pub is_glob: bool,
    pub span: Range<usize>,
    pub is_public: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
pub struct FunctionItem {
    pub name: String,
    pub scope: Vec<String>,
    pub calls: Vec<String>,
    /// Qualified direct function-call paths, preserving all path segments.
    pub qualified_calls: Vec<String>,
    pub name_span: Range<usize>,
    pub span: Range<usize>,
    pub body_span: Option<Range<usize>>,
    pub parameters: Vec<ParameterItem>,
    pub return_type: Option<String>,
    pub body: Option<BodyExpr>,
    pub is_public: bool,
    pub is_generic: bool,
    pub generic_parameters: Vec<String>,
    /// Type parameters with the recognized built-in `Copy` bound.
    pub generic_copy_parameters: Vec<String>,
    /// Type parameters constrained by the supported `AsRef<str>` subset.
    pub generic_as_ref_str_parameters: Vec<String>,
    /// Type parameters constrained by the supported `Add<Output = T>` subset.
    pub generic_add_parameters: Vec<String>,
    /// True when this generic definition uses constraints outside the
    /// compiler's recognized built-in bound subsets.
    pub generic_has_bounds: bool,
    pub is_async: bool,
    pub is_const: bool,
    pub is_unsafe: bool,
    pub has_abi: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
pub struct ParameterItem {
    pub name: Option<String>,
    pub type_text: String,
    pub span: Range<usize>,
}

fn generic_parameters_have_unsupported_bounds(
    parameters: &syn::punctuated::Punctuated<syn::GenericParam, syn::token::Comma>,
) -> bool {
    parameters.iter().any(|parameter| match parameter {
        syn::GenericParam::Type(parameter) => {
            parameter.default.is_some()
                || parameter.bounds.iter().any(|bound| match bound {
                    syn::TypeParamBound::Trait(bound) => {
                        !is_supported_copy_bound(bound)
                            && !is_supported_as_ref_str_bound(bound)
                            && !is_supported_add_bound(bound, &parameter.ident.unraw().to_string())
                    }
                    syn::TypeParamBound::Lifetime(_) => true,
                    _ => true,
                })
        }
        syn::GenericParam::Lifetime(_) | syn::GenericParam::Const(_) => true,
    })
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

#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
pub struct BodyExpr {
    pub kind: BodyExprKind,
    pub span: Range<usize>,
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
pub struct MacroExpression {
    pub path: String,
    pub tokens: String,
    pub format_string: Option<String>,
    pub arguments: Vec<BodyExpr>,
    /// Compile-time expansion for built-in macros whose inputs are literals.
    pub const_expansion: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
pub struct MatchArm {
    pub pattern: MatchPattern,
    pub body: BodyExpr,
    pub span: Range<usize>,
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
pub enum MatchPattern {
    Wildcard,
    Literal(LiteralKind),
    Binding(String),
    Range {
        start: LiteralKind,
        end: LiteralKind,
        inclusive: bool,
    },
    Path(String),
    Or(Vec<MatchPattern>),
    EnumVariant {
        path: String,
        discriminant: i32,
        variant_count: usize,
    },
    Unsupported,
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
pub enum BodyExprKind {
    Unit,
    Literal(LiteralKind),
    EmptySliceReference,
    Reference {
        mutable: bool,
        expression: Box<BodyExpr>,
    },
    Cast {
        expression: Box<BodyExpr>,
        type_name: String,
    },
    Dereference(Box<BodyExpr>),
    MacroInvocation(MacroExpression),
    Path(String),
    Call {
        callee: String,
        generic_arguments: Vec<String>,
        arguments: Vec<BodyExpr>,
    },
    MethodCall {
        receiver: Box<BodyExpr>,
        method: String,
        arguments: Vec<BodyExpr>,
    },
    Closure {
        parameters: Vec<ClosureParameter>,
        body: Box<BodyExpr>,
    },
    Tuple(Vec<BodyExpr>),
    ArrayLiteral(Vec<BodyExpr>),
    FixedArrayLiteral(Vec<BodyExpr>),
    StructLiteral {
        path: String,
        fields: Vec<(String, BodyExpr)>,
    },
    FieldAccess {
        base: Box<BodyExpr>,
        field: String,
    },
    Index {
        base: Box<BodyExpr>,
        index: Box<BodyExpr>,
    },
    Binary {
        operator: BinaryOperator,
        left: Box<BodyExpr>,
        right: Box<BodyExpr>,
    },
    Negate(Box<BodyExpr>),
    Not(Box<BodyExpr>),
    Block {
        statements: Vec<BodyStatement>,
        tail: Option<Box<BodyExpr>>,
    },
    If {
        condition: Box<BodyExpr>,
        then_branch: Box<BodyExpr>,
        else_branch: Option<Box<BodyExpr>>,
    },
    Match {
        scrutinee: Box<BodyExpr>,
        arms: Vec<MatchArm>,
    },
    Return(Option<Box<BodyExpr>>),
    Try(Box<BodyExpr>),
    Parenthesized(Box<BodyExpr>),
    Unsupported,
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
pub enum ClosureParameter {
    Identifier(String),
    Tuple(Vec<String>),
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
pub enum LiteralKind {
    Integer { text: String, suffix: String },
    Float { text: String, suffix: String },
    Boolean(bool),
    Character(char),
    String { value: String },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize)]
pub enum BinaryOperator {
    Add,
    Subtract,
    Multiply,
    Divide,
    Remainder,
    BitXor,
    Equal,
    PatternEqual,
    NotEqual,
    Less,
    LessEqual,
    Greater,
    GreaterEqual,
    And,
    Or,
    Unsupported,
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
pub enum BodyStatement {
    Let {
        name: Option<String>,
        type_text: Option<String>,
        mutable: bool,
        initializer: Option<BodyExpr>,
        span: Range<usize>,
    },
    LetTuple {
        names: Vec<String>,
        initializer: BodyExpr,
        span: Range<usize>,
    },
    Assign {
        name: String,
        value: BodyExpr,
        span: Range<usize>,
    },
    While {
        condition: BodyExpr,
        body: BodyExpr,
        span: Range<usize>,
    },
    Loop {
        body: BodyExpr,
        span: Range<usize>,
    },
    ForRange {
        name: String,
        start: BodyExpr,
        end: BodyExpr,
        inclusive: bool,
        body: BodyExpr,
        span: Range<usize>,
    },
    ForEach {
        names: Vec<String>,
        iterator: BodyExpr,
        body: BodyExpr,
        span: Range<usize>,
    },
    Break {
        span: Range<usize>,
    },
    Continue {
        span: Range<usize>,
    },
    MacroInvocation {
        path: String,
        tokens: String,
        format_string: Option<String>,
        arguments: Vec<BodyExpr>,
        span: Range<usize>,
    },
    Expression {
        expression: BodyExpr,
        has_semicolon: bool,
    },
    Unsupported,
}

/// Build nested delimiter groups. This is a parser bootstrap, not Rust grammar parsing.
pub fn parse(source: &SourceFile) -> Parse {
    let lexed = lex(source);
    let mut result = Parse {
        items: Vec::new(),
        functions: Vec::new(),
        structs: Vec::new(),
        enums: Vec::new(),
        modules: Vec::new(),
        imports: Vec::new(),
        unsupported_item_count: 0,
        diagnostics: lexed.diagnostics,
        had_error: lexed.had_error,
    };
    let mut stack: Vec<GroupFrame> = Vec::new();
    for token in lexed.tokens {
        let text = source.slice(token.span.clone());
        if let Some(delimiter) = opening(text) {
            stack.push(GroupFrame {
                delimiter,
                start: token.span.start,
                children: Vec::new(),
            });
        } else if let Some(delimiter) = closing(text) {
            if stack
                .last()
                .is_some_and(|frame| frame.delimiter == delimiter)
            {
                let frame = stack.pop().expect("top frame checked");
                append_tree(
                    &mut result.items,
                    &mut stack,
                    TokenTree::Group {
                        delimiter,
                        span: frame.start..token.span.end,
                        children: frame.children,
                    },
                );
            } else {
                result.had_error = true;
                result.diagnostics.push(Diagnostic {
                    span: token.span,
                    message: "unmatched closing delimiter".to_owned(),
                });
            }
        } else {
            append_tree(&mut result.items, &mut stack, TokenTree::Token(token));
        }
    }
    for frame in stack.into_iter().rev() {
        result.had_error = true;
        result.diagnostics.push(Diagnostic {
            span: frame.start..frame.start + 1,
            message: "unclosed delimiter".to_owned(),
        });
    }
    collect_functions(&result.items, source, &[], &mut result.functions);
    collect_modules_and_imports(
        &result.items,
        source,
        &[],
        &mut result.modules,
        &mut result.imports,
    );
    match syn::parse_file(source.text()) {
        Ok(file) => {
            result.functions.clear();
            result.structs.clear();
            result.enums.clear();
            result.modules.clear();
            result.imports.clear();
            let mut collector = AstCollector::new(source, &quill_cfg::CfgSet::default());
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
            if result.diagnostics.is_empty() {
                let location = error.span().start();
                let offset = source.byte_offset(location.line, location.column);
                result.diagnostics.push(Diagnostic {
                    span: offset..offset.saturating_add(1).min(source.text().len()),
                    message: format!("invalid Rust syntax: {error}"),
                });
            }
        }
    }
    result
}

pub fn parse_files(pool: &quill_parallel::WorkPool, sources: &[SourceFile]) -> Vec<Parse> {
    // The compiler driver needs semantic items, not the lossless bootstrap
    // token tree. Avoid lexing and grouping every byte before Syn parses it.
    pool.map(sources, parse_ast)
}

/// Parse source for compiler phases that consume Rust items and bodies but do
/// not need the standalone lexer's token stream or the bootstrap token tree.
pub fn parse_ast(source: &SourceFile) -> Parse {
    parse_ast_with_cfg_and_edition(source, &quill_cfg::CfgSet::default(), "2024")
}

pub fn parse_ast_with_cfg(source: &SourceFile, cfg: &quill_cfg::CfgSet) -> Parse {
    parse_ast_with_cfg_and_edition(source, cfg, "2024")
}

/// Parse source using its Cargo edition, including edition-specific keywords
/// and foreign-item safety rules.
pub fn parse_ast_with_cfg_and_edition(
    source: &SourceFile,
    cfg: &quill_cfg::CfgSet,
    edition: &str,
) -> Parse {
    if let Some(parse) = load_parse_cache(source, cfg, edition) {
        return parse;
    }
    let mut result = Parse::default();
    match parse_file_for_edition(source.text(), edition) {
        Ok(file) => {
            let mut collector = AstCollector::new(source, cfg);
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
    store_parse_cache(source, cfg, edition, &result);
    result
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

fn parse_cache_path(
    source: &SourceFile,
    cfg: &quill_cfg::CfgSet,
    edition: &str,
) -> Option<std::path::PathBuf> {
    let cache_root = std::env::var_os("QUILL_CACHE_DIR").map(std::path::PathBuf::from)?;
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"quill-parse-cache-v1");
    hasher.update(&parse_compiler_fingerprint());
    hasher.update(source.text().as_bytes());
    hasher.update(format!("{cfg:?}").as_bytes());
    hasher.update(edition.as_bytes());
    Some(
        cache_root
            .join("parse-ast")
            .join(format!("{}.json", hasher.finalize().to_hex())),
    )
}

fn parse_compiler_fingerprint() -> [u8; 32] {
    static FINGERPRINT: OnceLock<[u8; 32]> = OnceLock::new();
    *FINGERPRINT.get_or_init(|| {
        let mut hasher = blake3::Hasher::new();
        if let Ok(executable) = std::env::current_exe() {
            hasher.update(executable.as_os_str().as_encoded_bytes());
            if let Ok(metadata) = std::fs::metadata(executable) {
                hasher.update(&metadata.dev().to_le_bytes());
                hasher.update(&metadata.ino().to_le_bytes());
                hasher.update(&metadata.len().to_le_bytes());
                hasher.update(&metadata.mtime().to_le_bytes());
                hasher.update(&metadata.mtime_nsec().to_le_bytes());
                hasher.update(&metadata.ctime().to_le_bytes());
                hasher.update(&metadata.ctime_nsec().to_le_bytes());
            }
        }
        *hasher.finalize().as_bytes()
    })
}

fn load_parse_cache(source: &SourceFile, cfg: &quill_cfg::CfgSet, edition: &str) -> Option<Parse> {
    let bytes = std::fs::read(parse_cache_path(source, cfg, edition)?).ok()?;
    serde_json::from_slice(&bytes).ok()
}

fn store_parse_cache(source: &SourceFile, cfg: &quill_cfg::CfgSet, edition: &str, parse: &Parse) {
    let Some(path) = parse_cache_path(source, cfg, edition) else {
        return;
    };
    let Some(parent) = path.parent() else {
        return;
    };
    if std::fs::create_dir_all(parent).is_err() {
        return;
    }
    let Ok(bytes) = serde_json::to_vec(parse) else {
        return;
    };
    let temporary = path.with_extension(format!("{}.tmp", std::process::id()));
    if std::fs::write(&temporary, bytes).is_ok() {
        let _ = std::fs::rename(temporary, path);
    }
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
    fn new(source: &'a SourceFile, cfg: &quill_cfg::CfgSet) -> Self {
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
                                .map(|lifetime| format!("{} ", lifetime.ident))
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
            || generic_parameters_have_unsupported_bounds(&signature.generics.params);
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
        if !self.cfg.attributes_enabled(item_attributes(item)) {
            return;
        }
        let supported_literal_macro = matches!(item, syn::Item::Macro(macro_item)
            if macro_item.mac.path.is_ident("macro_rules")
                && macro_item.ident.is_some()
                && supported_macro_rule(macro_item).is_some());
        if !matches!(
            item,
            syn::Item::Fn(_) | syn::Item::Struct(_) | syn::Item::Enum(_) | syn::Item::Impl(_)
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
            path_override: self.cfg.active_string_attribute(&item.attrs, "path"),
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
            || generic_parameters_have_unsupported_bounds(&item.generics.params)
            // Trait-bound metadata is currently carried only from function
            // parameters, so bounded impl parameters stay outside the subset.
            || !generic_copy_parameters(&item.generics.params).is_empty()
            || !generic_as_ref_str_parameters(&item.generics.params).is_empty();
        self.scope.push(format!("impl:{name}"));
        syn::visit::visit_item_impl(self, item);
        self.scope.pop();
        self.impl_generic_parameters
            .truncate(previous_parameter_count);
        self.impl_generic_has_bounds = previous_has_bounds;
    }

    fn visit_item_trait(&mut self, item: &'ast syn::ItemTrait) {
        self.scope.push(format!("trait:{}", item.ident));
        syn::visit::visit_item_trait(self, item);
        self.scope.pop();
    }

    fn visit_impl_item_fn(&mut self, item: &'ast syn::ImplItemFn) {
        if !self.cfg.attributes_enabled(&item.attrs) {
            return;
        }
        self.unsupported_item_count += unsupported_attribute_count(&item.attrs);
        self.add_function(&item.sig, &item.vis, item.span(), Some(&item.block));
        self.scope.push(format!("fn:{}", item.sig.ident));
        syn::visit::visit_block(self, &item.block);
        self.scope.pop();
    }

    fn visit_trait_item_fn(&mut self, item: &'ast syn::TraitItemFn) {
        if !self.cfg.attributes_enabled(&item.attrs) {
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
        if !self.cfg.attributes_enabled(&item.attrs) {
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

fn span_range(source: &SourceFile, span: proc_macro2::Span) -> Range<usize> {
    let start = span.start();
    let end = span.end();
    source.byte_offset(start.line, start.column)..source.byte_offset(end.line, end.column)
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
                syn::Expr::Break(expr) if expr.label.is_none() && expr.expr.is_none() => statements
                    .push(BodyStatement::Break {
                        span: span_range(source, expr.span()),
                    }),
                syn::Expr::Continue(expr) if expr.label.is_none() => {
                    statements.push(BodyStatement::Continue {
                        span: span_range(source, expr.span()),
                    })
                }
                syn::Expr::Break(_) | syn::Expr::Continue(_) => {
                    statements.push(BodyStatement::Unsupported)
                }
                syn::Expr::Assign(assign) => {
                    let target = match &*assign.left {
                        syn::Expr::Path(path)
                            if path.qself.is_none() && path.path.segments.len() == 1 =>
                        {
                            Some(path.path.segments[0].ident.unraw().to_string())
                        }
                        syn::Expr::Field(field) => {
                            let syn::Expr::Path(path) = &*field.base else {
                                statements.push(BodyStatement::Unsupported);
                                continue;
                            };
                            (path.qself.is_none() && path.path.segments.len() == 1).then(|| {
                                format!(
                                    "{}.{}",
                                    path.path.segments[0].ident,
                                    match &field.member {
                                        syn::Member::Named(name) => name.to_string(),
                                        syn::Member::Unnamed(index) => index.index.to_string(),
                                    }
                                )
                            })
                        }
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
                    condition: lower_expr(&while_expr.cond, source),
                    body: lower_block(&while_expr.body, source),
                    span: span_range(source, while_expr.span()),
                }),
                syn::Expr::Loop(loop_expr) if loop_expr.label.is_none() => {
                    statements.push(BodyStatement::Loop {
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
                statements.push(BodyStatement::MacroInvocation {
                    path: macro_expression.path,
                    tokens: macro_expression.tokens,
                    format_string: macro_expression.format_string,
                    arguments: macro_expression.arguments,
                    span: span_range(source, statement_macro.span()),
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
        syn::Expr::Index(index) => BodyExprKind::Index {
            base: Box::new(lower_expr(&index.expr, source)),
            index: Box::new(lower_expr(&index.index, source)),
        },
        syn::Expr::Array(array) => BodyExprKind::FixedArrayLiteral(
            array
                .elems
                .iter()
                .map(|element| lower_expr(element, source))
                .collect(),
        ),
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
    let const_expansion = (path == "concat")
        .then(|| parsed_arguments.as_ref())
        .flatten()
        .and_then(|arguments| {
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
        });
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
        syn::BinOp::BitXor(_) => BinaryOperator::BitXor,
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

struct GroupFrame {
    delimiter: Delimiter,
    start: usize,
    children: Vec<TokenTree>,
}

fn append_tree(items: &mut Vec<TokenTree>, stack: &mut [GroupFrame], tree: TokenTree) {
    if let Some(parent) = stack.last_mut() {
        parent.children.push(tree);
    } else {
        items.push(tree);
    }
}

fn opening(text: &str) -> Option<Delimiter> {
    match text {
        "(" => Some(Delimiter::Parenthesis),
        "{" => Some(Delimiter::Brace),
        "[" => Some(Delimiter::Bracket),
        _ => None,
    }
}

fn closing(text: &str) -> Option<Delimiter> {
    match text {
        ")" => Some(Delimiter::Parenthesis),
        "}" => Some(Delimiter::Brace),
        "]" => Some(Delimiter::Bracket),
        _ => None,
    }
}

fn tree_token(tree: &TokenTree) -> Option<&Token> {
    match tree {
        TokenTree::Token(token) => Some(token),
        TokenTree::Group { .. } => None,
    }
}

fn token_text<'a>(tree: &'a TokenTree, source: &'a SourceFile) -> Option<&'a str> {
    tree_token(tree).map(|token| source.slice(token.span.clone()))
}

fn collect_functions(
    trees: &[TokenTree],
    source: &SourceFile,
    scope: &[String],
    output: &mut Vec<FunctionItem>,
) {
    let mut index = 0;
    while index < trees.len() {
        if token_text(&trees[index], source) == Some("fn")
            && let Some((item, body_index, end_index)) = function_at(trees, index, scope, source)
        {
            output.push(item);
            if let Some(body_index) = body_index
                && let TokenTree::Group { children, .. } = &trees[body_index]
            {
                let mut nested_scope = scope.to_vec();
                nested_scope.push(output.last().expect("function was added").name.clone());
                collect_functions(children, source, &nested_scope, output);
            }
            index = end_index.saturating_add(1);
            continue;
        }

        if let TokenTree::Group { children, .. } = &trees[index] {
            let macro_group = index > 0 && token_text(&trees[index - 1], source) == Some("!");
            if !macro_group {
                let nested_scope = group_scope(trees, index, scope, source);
                collect_functions(children, source, &nested_scope, output);
            }
        }
        index += 1;
    }
}

fn collect_modules_and_imports(
    trees: &[TokenTree],
    source: &SourceFile,
    scope: &[String],
    modules: &mut Vec<ModuleItem>,
    imports: &mut Vec<ImportItem>,
) {
    let mut index = 0;
    while index < trees.len() {
        match token_text(&trees[index], source) {
            Some("mod") => {
                let Some(name_token) = trees.get(index + 1).and_then(tree_token) else {
                    index += 1;
                    continue;
                };
                if name_token.kind != TokenKind::Identifier {
                    index += 1;
                    continue;
                }
                let name = source.slice(name_token.span.clone()).to_owned();
                let (prefix_start, is_public) = item_prefix(trees, index, source);
                let start = tree_token(&trees[prefix_start])
                    .map_or(name_token.span.start, |token| token.span.start);
                let following = index + 2;
                if let Some(TokenTree::Group {
                    delimiter: Delimiter::Brace,
                    span,
                    children,
                }) = trees.get(following)
                {
                    modules.push(ModuleItem {
                        name: name.clone(),
                        parent_scope: scope.to_vec(),
                        span: start..span.end,
                        body_span: Some(span.clone()),
                        path_override: None,
                        is_public,
                    });
                    let mut nested_scope = scope.to_vec();
                    nested_scope.push(format!("mod:{name}"));
                    collect_modules_and_imports(children, source, &nested_scope, modules, imports);
                    index = following + 1;
                } else if trees
                    .get(following)
                    .and_then(|tree| token_text(tree, source))
                    == Some(";")
                {
                    let Some(end_token) = tree_token(&trees[following]) else {
                        index += 1;
                        continue;
                    };
                    let end = end_token.span.end;
                    modules.push(ModuleItem {
                        name,
                        parent_scope: scope.to_vec(),
                        span: start..end,
                        body_span: None,
                        path_override: None,
                        is_public,
                    });
                    index = following + 1;
                } else {
                    index += 1;
                }
            }
            Some("use") => {
                let Some(end_index) = (index + 1..trees.len())
                    .find(|&at| token_text(&trees[at], source) == Some(";"))
                else {
                    index += 1;
                    continue;
                };
                let (prefix_start, is_public) = item_prefix(trees, index, source);
                let start = tree_token(&trees[prefix_start]).map_or_else(
                    || tree_token(&trees[index]).unwrap().span.start,
                    |token| token.span.start,
                );
                let end = tree_token(&trees[end_index]).unwrap().span.end;
                let span = start..end;
                flatten_use_tree(
                    &trees[index + 1..end_index],
                    &[],
                    scope,
                    span,
                    is_public,
                    source,
                    imports,
                );
                index = end_index + 1;
            }
            _ => {
                if let TokenTree::Group { children, .. } = &trees[index] {
                    let macro_group =
                        index > 0 && token_text(&trees[index - 1], source) == Some("!");
                    if !macro_group {
                        let nested_scope = group_scope(trees, index, scope, source);
                        collect_modules_and_imports(
                            children,
                            source,
                            &nested_scope,
                            modules,
                            imports,
                        );
                    }
                }
                index += 1;
            }
        }
    }
}

fn flatten_use_tree(
    trees: &[TokenTree],
    prefix: &[String],
    scope: &[String],
    span: Range<usize>,
    is_public: bool,
    source: &SourceFile,
    output: &mut Vec<ImportItem>,
) {
    for branch in split_commas(trees, source) {
        if let Some(group_index) = branch.iter().position(|tree| {
            matches!(
                tree,
                TokenTree::Group {
                    delimiter: Delimiter::Brace,
                    ..
                }
            )
        }) {
            let mut group_prefix = prefix.to_vec();
            let (segments, _, _) = use_path(&branch[..group_index], &[], source);
            group_prefix.extend(segments);
            if let TokenTree::Group { children, .. } = &branch[group_index] {
                flatten_use_tree(
                    children,
                    &group_prefix,
                    scope,
                    span.clone(),
                    is_public,
                    source,
                    output,
                );
            }
            continue;
        }
        let (path, alias, is_glob) = use_path(branch, prefix, source);
        output.push(ImportItem {
            scope: scope.to_vec(),
            path,
            alias,
            is_glob,
            span: span.clone(),
            is_public,
        });
    }
}

fn split_commas<'a>(trees: &'a [TokenTree], source: &SourceFile) -> Vec<&'a [TokenTree]> {
    let mut branches = Vec::new();
    let mut start = 0;
    for (index, tree) in trees.iter().enumerate() {
        if token_text(tree, source) == Some(",") {
            branches.push(&trees[start..index]);
            start = index + 1;
        }
    }
    if start < trees.len() {
        branches.push(&trees[start..]);
    }
    branches
}

fn use_path(
    trees: &[TokenTree],
    prefix: &[String],
    source: &SourceFile,
) -> (Vec<String>, Option<String>, bool) {
    let mut path = prefix.to_vec();
    let mut alias = None;
    let mut is_glob = false;
    let mut saw_self = false;
    let mut in_alias = false;
    for tree in trees {
        let Some(text) = token_text(tree, source) else {
            continue;
        };
        if text == "as" {
            in_alias = true;
        } else if in_alias {
            alias = Some(text.to_owned());
            break;
        } else if text == "*" {
            is_glob = true;
        } else if text == "self" {
            if prefix.is_empty() {
                path.push("self".to_owned());
            } else {
                saw_self = true;
            }
        } else if tree_token(tree).is_some_and(|token| token.kind == TokenKind::Identifier) {
            path.push(text.to_owned());
        }
    }
    if saw_self && path.len() == prefix.len() {
        path = prefix.to_vec();
    }
    (path, alias, is_glob)
}

fn function_at(
    trees: &[TokenTree],
    fn_index: usize,
    scope: &[String],
    source: &SourceFile,
) -> Option<(FunctionItem, Option<usize>, usize)> {
    let name_tree = trees.get(fn_index + 1)?;
    let name_token = tree_token(name_tree)?;
    if name_token.kind != TokenKind::Identifier {
        return None;
    }
    let name = source.slice(name_token.span.clone()).to_owned();
    let mut params_index = None;
    let mut is_generic = false;
    for (offset, tree) in trees.iter().enumerate().skip(fn_index + 2) {
        if let Some(text) = token_text(tree, source) {
            if text == ";" {
                let end_offset = tree_token(tree)?.span.end;
                return Some((
                    make_function_item(
                        trees,
                        fn_index,
                        name,
                        scope,
                        Vec::new(),
                        name_token,
                        end_offset,
                        None,
                        is_generic,
                        source,
                    ),
                    None,
                    offset,
                ));
            }
            if text == "<" && params_index.is_none() {
                is_generic = true;
            }
        } else if let TokenTree::Group { delimiter, .. } = tree {
            match delimiter {
                Delimiter::Parenthesis if params_index.is_none() => params_index = Some(offset),
                Delimiter::Brace if params_index.is_some() => {
                    let (span, calls) = match tree {
                        TokenTree::Group { span, children, .. } => {
                            (span.clone(), call_names(children, source))
                        }
                        TokenTree::Token(_) => unreachable!(),
                    };
                    return Some((
                        make_function_item(
                            trees,
                            fn_index,
                            name,
                            scope,
                            calls,
                            name_token,
                            span.end,
                            Some(span),
                            is_generic,
                            source,
                        ),
                        Some(offset),
                        offset,
                    ));
                }
                _ => {}
            }
        }
    }
    None
}

#[allow(clippy::too_many_arguments)]
fn make_function_item(
    trees: &[TokenTree],
    fn_index: usize,
    name: String,
    scope: &[String],
    calls: Vec<String>,
    name_token: &Token,
    end_offset: usize,
    body_span: Option<Range<usize>>,
    is_generic: bool,
    source: &SourceFile,
) -> FunctionItem {
    let (start, is_public) = item_prefix(trees, fn_index, source);
    let start_offset = tree_token(&trees[start])
        .map(|token| token.span.start)
        .unwrap_or(name_token.span.start);
    FunctionItem {
        name,
        scope: scope.to_vec(),
        calls,
        qualified_calls: Vec::new(),
        name_span: name_token.span.clone(),
        span: start_offset..end_offset,
        body_span,
        parameters: Vec::new(),
        return_type: None,
        body: None,
        is_public,
        is_generic,
        generic_parameters: Vec::new(),
        generic_copy_parameters: Vec::new(),
        generic_as_ref_str_parameters: Vec::new(),
        generic_add_parameters: Vec::new(),
        generic_has_bounds: is_generic,
        is_async: false,
        is_const: false,
        is_unsafe: false,
        has_abi: false,
    }
}

fn item_prefix(trees: &[TokenTree], fn_index: usize, source: &SourceFile) -> (usize, bool) {
    let mut start = fn_index;
    let mut is_public = false;
    loop {
        if start > 0 {
            if tree_token(&trees[start - 1]).is_some_and(|token| token.kind == TokenKind::String)
                && start > 1
                && token_text(&trees[start - 2], source) == Some("extern")
            {
                start -= 2;
                continue;
            }
            if let Some(qualifier) = token_text(&trees[start - 1], source) {
                if matches!(
                    qualifier,
                    "async" | "const" | "default" | "extern" | "unsafe"
                ) {
                    start -= 1;
                    continue;
                }
                if qualifier == "pub" {
                    start -= 1;
                    is_public = true;
                    continue;
                }
            }
            if start > 1
                && matches!(
                    trees[start - 1],
                    TokenTree::Group {
                        delimiter: Delimiter::Parenthesis,
                        ..
                    }
                )
                && token_text(&trees[start - 2], source) == Some("pub")
            {
                start -= 2;
                is_public = true;
                continue;
            }
        }
        break;
    }
    (start, is_public)
}

fn call_names(trees: &[TokenTree], source: &SourceFile) -> Vec<String> {
    let mut calls = Vec::new();
    for (index, tree) in trees.iter().enumerate() {
        if let Some(token) = tree_token(tree) {
            if token.kind == TokenKind::Identifier
                && matches!(
                    trees.get(index + 1),
                    Some(TokenTree::Group {
                        delimiter: Delimiter::Parenthesis,
                        ..
                    })
                )
                && (index == 0 || !matches!(token_text(&trees[index - 1], source), Some("." | "!")))
            {
                calls.push(source.slice(token.span.clone()).to_owned());
            }
        } else if let TokenTree::Group { children, .. } = tree {
            calls.extend(call_names(children, source));
        }
    }
    calls.sort_unstable();
    calls.dedup();
    calls
}

fn group_scope(
    trees: &[TokenTree],
    group_index: usize,
    parent: &[String],
    source: &SourceFile,
) -> Vec<String> {
    let mut scope = parent.to_vec();
    if group_index >= 2 {
        let owner = token_text(&trees[group_index - 2], source);
        if matches!(owner, Some("mod" | "trait"))
            && let Some(name) = token_text(&trees[group_index - 1], source)
        {
            scope.push(format!("{}:{name}", owner.unwrap()));
            return scope;
        }
    }
    let search_start = group_index.saturating_sub(12);
    if let Some(impl_index) = (search_start..group_index)
        .rev()
        .find(|&index| token_text(&trees[index], source) == Some("impl"))
    {
        let impl_target = trees[impl_index + 1..group_index]
            .iter()
            .filter_map(|tree| token_text(tree, source))
            .collect::<String>();
        scope.push(format!("impl:{impl_target}"));
    }
    scope
}

struct Lexer<'a> {
    text: &'a str,
    bytes: &'a [u8],
    offset: usize,
    result: Lexed,
}

impl<'a> Lexer<'a> {
    fn new(text: &'a str) -> Self {
        Self {
            text,
            bytes: text.as_bytes(),
            offset: 0,
            result: Lexed::default(),
        }
    }

    fn run(mut self) -> Lexed {
        while self.offset < self.bytes.len() {
            let start = self.offset;
            let byte = self.bytes[self.offset];
            if byte.is_ascii_whitespace() {
                self.offset += 1;
            } else if self.starts_with(b"//") {
                self.line_comment(start);
            } else if self.starts_with(b"/*") {
                self.block_comment(start);
            } else if byte == b'b' && self.bytes.get(self.offset + 1) == Some(&b'"') {
                self.quoted(start, b'"', TokenKind::String, 1);
            } else if self.raw_string_prefix().is_some() {
                self.raw_string(start);
            } else if byte == b'"' {
                self.quoted(start, b'"', TokenKind::String, 0);
            } else if byte == b'\''
                || (byte == b'b' && self.bytes.get(self.offset + 1) == Some(&b'\''))
            {
                self.character_or_lifetime(start);
            } else if byte.is_ascii_digit() {
                self.number(start);
            } else if self.identifier_start_at(self.offset) {
                self.identifier(start);
            } else {
                self.punctuation();
                self.push(TokenKind::Punctuation, start);
            }
        }
        self.result
    }

    fn starts_with(&self, needle: &[u8]) -> bool {
        self.bytes.get(self.offset..self.offset + needle.len()) == Some(needle)
    }

    fn char_width(&self, at: usize) -> usize {
        self.text[at..]
            .chars()
            .next()
            .map(char::len_utf8)
            .unwrap_or(1)
    }

    fn line_comment(&mut self, start: usize) {
        let doc = self.bytes.get(start + 2) == Some(&b'!')
            || (self.bytes.get(start + 2) == Some(&b'/')
                && self.bytes.get(start + 3) != Some(&b'/'));
        self.offset += 2;
        while self.offset < self.bytes.len() && self.bytes[self.offset] != b'\n' {
            self.offset += 1;
        }
        if doc {
            self.push(TokenKind::DocComment, start);
        }
    }

    fn block_comment(&mut self, start: usize) {
        let doc = self.bytes.get(start + 2) == Some(&b'!')
            || (self.bytes.get(start + 2) == Some(&b'*')
                && self.bytes.get(start + 3) != Some(&b'*'));
        self.offset += 2;
        let mut depth = 1usize;
        while self.offset < self.bytes.len() && depth > 0 {
            if self.starts_with(b"/*") {
                depth += 1;
                self.offset += 2;
            } else if self.starts_with(b"*/") {
                depth -= 1;
                self.offset += 2;
            } else {
                self.offset += self.char_width(self.offset);
            }
        }
        if depth > 0 {
            self.error(start..self.bytes.len(), "unterminated block comment");
        } else if doc {
            self.push(TokenKind::DocComment, start);
        }
    }

    fn raw_string_prefix(&self) -> Option<(usize, usize)> {
        let mut cursor = self.offset;
        if self.bytes.get(cursor) == Some(&b'b') {
            cursor += 1;
        }
        if self.bytes.get(cursor) != Some(&b'r') {
            return None;
        }
        cursor += 1;
        let hashes_start = cursor;
        while self.bytes.get(cursor) == Some(&b'#') {
            cursor += 1;
        }
        if self.bytes.get(cursor) == Some(&b'"') {
            Some((cursor + 1, cursor - hashes_start))
        } else {
            None
        }
    }

    fn raw_string(&mut self, start: usize) {
        let (content_start, hashes) = self.raw_string_prefix().expect("prefix checked");
        self.offset = content_start;
        let mut found = false;
        while self.offset < self.bytes.len() {
            if self.bytes[self.offset] == b'"'
                && self
                    .bytes
                    .get(self.offset + 1..self.offset + 1 + hashes)
                    .is_some_and(|tail| tail.iter().all(|byte| *byte == b'#'))
            {
                self.offset += 1 + hashes;
                found = true;
                break;
            }
            self.offset += self.char_width(self.offset);
        }
        if found {
            self.push(TokenKind::String, start);
        } else {
            self.error(start..self.bytes.len(), "unterminated raw string literal");
        }
    }

    fn quoted(&mut self, start: usize, quote: u8, kind: TokenKind, prefix: usize) {
        self.offset = start + prefix + 1;
        while self.offset < self.bytes.len() {
            match self.bytes[self.offset] {
                byte if byte == quote => {
                    self.offset += 1;
                    self.push(kind, start);
                    return;
                }
                b'\\' => {
                    self.offset += 1;
                    if self.offset < self.bytes.len() {
                        self.offset += self.char_width(self.offset);
                    }
                }
                b'\n' | b'\r' => {
                    self.error(start..self.offset, "newline in literal");
                    return;
                }
                _ => self.offset += self.char_width(self.offset),
            }
        }
        self.error(start..self.bytes.len(), "unterminated literal");
    }

    fn character_or_lifetime(&mut self, start: usize) {
        let prefix = usize::from(self.bytes[start] == b'b');
        let quote_at = start + prefix;
        let content_at = quote_at + 1;
        let mut cursor = content_at;
        if self.bytes.get(cursor) == Some(&b'\\') {
            cursor += 1;
            if cursor < self.bytes.len() {
                cursor += self.char_width(cursor);
            }
        } else if cursor < self.bytes.len() {
            cursor += self.char_width(cursor);
        }
        if self.bytes.get(cursor) == Some(&b'\'') {
            self.quoted(start, b'\'', TokenKind::Character, prefix);
            return;
        }
        if prefix == 0 && self.identifier_start_at(content_at) {
            self.offset = content_at + self.char_width(content_at);
            while self.offset < self.bytes.len() && self.identifier_continue_at(self.offset) {
                self.offset += self.char_width(self.offset);
            }
            self.push(TokenKind::Lifetime, start);
            return;
        }
        self.quoted(start, b'\'', TokenKind::Character, prefix);
    }

    fn number(&mut self, start: usize) {
        self.offset += 1;
        while self.offset < self.bytes.len() {
            let byte = self.bytes[self.offset];
            if byte.is_ascii_alphanumeric()
                || byte == b'_'
                || (byte == b'.' && self.bytes.get(self.offset + 1) != Some(&b'.'))
            {
                self.offset += 1;
            } else {
                break;
            }
        }
        self.push(TokenKind::Number, start);
    }

    fn identifier_start_at(&self, at: usize) -> bool {
        self.text
            .get(at..)
            .and_then(|tail| tail.chars().next())
            .is_some_and(|ch| ch == '_' || ch.is_alphabetic())
    }

    fn identifier_continue_at(&self, at: usize) -> bool {
        self.text
            .get(at..)
            .and_then(|tail| tail.chars().next())
            .is_some_and(|ch| ch == '_' || ch.is_alphanumeric())
    }

    fn identifier(&mut self, start: usize) {
        self.offset += self.char_width(self.offset);
        while self.identifier_continue_at(self.offset) {
            self.offset += self.char_width(self.offset);
        }
        self.push(TokenKind::Identifier, start);
    }

    fn punctuation(&mut self) {
        const PUNCTUATION: [&[u8]; 22] = [
            b"<<=", b">>=", b"..=", b"...", b"::", b"->", b"=>", b"==", b"!=", b"<=", b">=", b"&&",
            b"||", b"+=", b"-=", b"*=", b"/=", b"%=", b"^=", b"&=", b"|=", b"..",
        ];
        if let Some(punct) = PUNCTUATION.iter().find(|punct| self.starts_with(punct)) {
            self.offset += punct.len();
        } else {
            self.offset += self.char_width(self.offset);
        }
    }

    fn push(&mut self, kind: TokenKind, start: usize) {
        self.result.tokens.push(Token {
            kind,
            span: start..self.offset,
        });
    }

    fn error(&mut self, span: Range<usize>, message: &str) {
        self.result.had_error = true;
        self.result.diagnostics.push(Diagnostic {
            span,
            message: message.to_owned(),
        });
        self.offset = self.bytes.len();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_rust_2015_identifiers_without_rewriting_dyn_trait_objects() {
        let source = SourceFile::from_text(
            "edition-2015.rs",
            "trait Value {} fn accept(_: &dyn Value) {} fn main() { let async = 1u64; let await = 2u64; let dyn = 3u64; let _total = async + await + dyn; }",
        );
        let parsed = parse_ast_with_cfg_and_edition(&source, &quill_cfg::CfgSet::default(), "2015");
        assert!(!parsed.had_error, "{:?}", parsed.diagnostics);

        let async_function =
            SourceFile::from_text("edition-2015.rs", "pub async fn answer() -> u64 { 42 }");
        let parsed =
            parse_ast_with_cfg_and_edition(&async_function, &quill_cfg::CfgSet::default(), "2015");
        assert!(parsed.had_error, "Rust 2015 accepted async fn syntax");
    }

    #[test]
    fn enforces_rust_2024_keyword_and_unsafe_item_rules() {
        let invalid_sources = [
            "fn main() { let gen = 1u64; }",
            "extern \"C\" { fn abs(value: i32) -> i32; }",
            "#[no_mangle] pub extern \"C\" fn exported() {}",
        ];
        for source_text in invalid_sources {
            let source = SourceFile::from_text("edition-2024.rs", source_text);
            let parsed =
                parse_ast_with_cfg_and_edition(&source, &quill_cfg::CfgSet::default(), "2024");
            assert!(parsed.had_error, "accepted invalid source: {source_text}");
        }
    }

    #[test]
    fn accepts_rust_2024_raw_gen_and_unsafe_forms() {
        let source = SourceFile::from_text(
            "edition-2024.rs",
            "#[unsafe(no_mangle)] pub extern \"C\" fn exported() {} unsafe extern \"C\" { fn abs(value: i32) -> i32; } macro_rules! discard { ($item:tt) => {}; } fn main() { let r#gen = 1u64; let _value = r#gen; discard!(gen); }",
        );
        let parsed = parse_ast_with_cfg_and_edition(&source, &quill_cfg::CfgSet::default(), "2024");
        assert!(!parsed.had_error, "{:?}", parsed.diagnostics);
    }

    #[test]
    fn skips_nested_comments_and_preserves_doc_comments() {
        let source = SourceFile::from_text(
            "test.rs",
            "/* outer /* nested */ end */ /// docs\nfn main() {}",
        );
        let lexed = lex(&source);
        assert!(!lexed.had_error);
        assert_eq!(lexed.tokens[0].kind, TokenKind::DocComment);
        assert_eq!(source.slice(lexed.tokens[0].span.clone()), "/// docs");
        assert!(
            lexed
                .tokens
                .iter()
                .any(|token| source.slice(token.span.clone()) == "main")
        );
    }

    #[test]
    fn lexes_raw_strings_lifetimes_and_range_punctuation() {
        let source =
            SourceFile::from_text("test.rs", "let s = br##\"hello\"##; fn f<'a>() { 0..3 }");
        let lexed = lex(&source);
        assert!(!lexed.had_error);
        let texts: Vec<_> = lexed
            .tokens
            .iter()
            .map(|token| source.slice(token.span.clone()))
            .collect();
        assert!(texts.contains(&"br##\"hello\"##"));
        assert!(texts.contains(&"'a"));
        assert!(texts.contains(&"0"));
        assert!(texts.contains(&".."));
        assert!(texts.contains(&"3"));
    }

    #[test]
    fn lexes_character_and_byte_character_literals() {
        let source = SourceFile::from_text("test.rs", r"let newline = '\n'; let byte = b'a';");
        let lexed = lex(&source);
        assert!(!lexed.had_error);
        let literals: Vec<_> = lexed
            .tokens
            .iter()
            .filter(|token| token.kind == TokenKind::Character)
            .map(|token| source.slice(token.span.clone()))
            .collect();
        assert_eq!(literals, [r"'\n'", "b'a'"]);
    }

    #[test]
    fn reports_unterminated_constructs_with_byte_spans() {
        let source = SourceFile::from_text("test.rs", "let s = \"unterminated");
        let lexed = lex(&source);
        assert!(lexed.had_error);
        assert_eq!(lexed.diagnostics[0].span, 8..source.text().len());
    }

    #[test]
    fn parser_builds_nested_token_groups() {
        let source = SourceFile::from_text("test.rs", "fn f() { let x = [1, (2)]; }");
        let parsed = parse(&source);
        assert!(!parsed.had_error);
        assert_eq!(parsed.items.len(), 4);
        assert!(parsed.items.iter().any(|item| matches!(
            item,
            TokenTree::Group {
                delimiter: Delimiter::Brace,
                ..
            }
        )));
    }

    #[test]
    fn ast_parser_extracts_items_without_building_a_token_tree() {
        let source = SourceFile::from_text(
            "test.rs",
            "mod inner; use crate::api as selected; pub fn call() { helper(); }",
        );
        let parsed = parse_ast(&source);
        assert!(!parsed.had_error);
        assert!(parsed.items.is_empty());
        assert_eq!(parsed.modules[0].name, "inner");
        assert_eq!(parsed.imports[0].alias.as_deref(), Some("selected"));
        assert_eq!(parsed.functions[0].calls, ["helper"]);
    }

    #[test]
    fn parses_tuple_pattern_for_each_loops() {
        let source = SourceFile::from_text(
            "iter.rs",
            "fn visit(fields: &[(u64, u64)]) { for (key, value) in fields { consume(key + value); } }",
        );
        let parsed = parse_ast(&source);
        assert!(!parsed.had_error, "{:?}", parsed.diagnostics);
        let Some(BodyExpr {
            kind: BodyExprKind::Block { statements, .. },
            ..
        }) = parsed.functions[0].body.as_ref()
        else {
            panic!("expected a function block");
        };
        let BodyStatement::ForEach {
            names, iterator, ..
        } = &statements[0]
        else {
            panic!("expected a tuple for-each statement");
        };
        assert_eq!(names, &["key", "value"]);
        assert!(matches!(&iterator.kind, BodyExprKind::Path(path) if path == "fields"));
    }

    #[test]
    fn ast_parser_filters_cfg_items_and_applies_cfg_attr_path() {
        let source = SourceFile::from_text(
            "test.rs",
            "#[cfg(feature = \"on\")] fn enabled() {} #[cfg(feature = \"off\")] fn disabled() {} #[cfg_attr(feature = \"on\", path = \"alt.rs\")] mod child;",
        );
        let mut cfg = quill_cfg::CfgSet::default();
        cfg.insert_value("feature", "on");
        let parsed = parse_ast_with_cfg(&source, &cfg);
        assert!(!parsed.had_error);
        assert_eq!(
            parsed
                .functions
                .iter()
                .map(|f| f.name.as_str())
                .collect::<Vec<_>>(),
            ["enabled"]
        );
        assert_eq!(parsed.modules[0].path_override.as_deref(), Some("alt.rs"));
    }

    #[test]
    fn ast_parser_reports_invalid_syntax_with_in_bounds_span() {
        let source = SourceFile::from_text("invalid.rs", "fn broken() { let value = ; }");
        let parsed = parse_ast(&source);
        assert!(parsed.had_error);
        assert!(!parsed.diagnostics.is_empty());
        assert!(
            parsed
                .diagnostics
                .iter()
                .all(|d| d.span.end <= source.text().len())
        );
    }

    #[test]
    fn parser_reports_unclosed_delimiters() {
        let source = SourceFile::from_text("test.rs", "fn f( {");
        let parsed = parse(&source);
        assert!(parsed.had_error);
        assert_eq!(parsed.diagnostics.len(), 2);
    }

    #[test]
    fn extracts_function_names_generics_visibility_and_bodies() {
        let source = SourceFile::from_text(
            "test.rs",
            "pub fn map<T>(value: T) -> T { value } extern \"C\" { pub fn foreign(x: i32); } fn make() -> Option<i32> { None } pub(crate) async fn internal() {}",
        );
        let parsed = parse(&source);
        assert!(!parsed.had_error);
        assert_eq!(parsed.functions.len(), 4);
        let map = &parsed.functions[0];
        assert_eq!(map.name, "map");
        assert!(map.is_public);
        assert!(map.is_generic);
        assert_eq!(source.slice(map.body_span.clone().unwrap()), "{ value }");
        let foreign = &parsed.functions[1];
        assert_eq!(foreign.name, "foreign");
        assert!(foreign.is_public);
        assert!(foreign.body_span.is_none());
        assert!(source.slice(foreign.span.clone()).ends_with(';'));
        let make = &parsed.functions[2];
        assert_eq!(make.name, "make");
        assert!(!make.is_generic);
        let internal = &parsed.functions[3];
        assert_eq!(internal.name, "internal");
        assert!(!internal.is_public, "pub(crate) is not externally public");
    }

    #[test]
    fn recognizes_supported_copy_as_ref_and_add_generic_bounds() {
        let file = SourceFile::from_text(
            "generic-bounds.rs",
            "fn duplicate<T: Copy>(value: T) -> T { value } fn borrow<T: AsRef<str>>(value: T) -> T { value } fn add<T: std::ops::Add<Output = T>>(left: T, right: T) -> T { left + right }",
        );
        let parsed = parse_ast(&file);
        assert_eq!(parsed.functions.len(), 3);
        assert!(!parsed.functions[0].generic_has_bounds);
        assert_eq!(parsed.functions[0].generic_copy_parameters, ["T"]);
        assert!(!parsed.functions[1].generic_has_bounds);
        assert_eq!(parsed.functions[1].generic_as_ref_str_parameters, ["T"]);
        assert!(!parsed.functions[2].generic_has_bounds);
        assert_eq!(parsed.functions[2].generic_add_parameters, ["T"]);
    }

    #[test]
    fn extracts_struct_fields_and_impl_methods_with_owner_scope() {
        let source = SourceFile::from_text(
            "model.rs",
            "pub struct Pair<T> { pub first: T, second: i32 } impl<T> Pair<T> { pub fn first(&self) -> &T { &self.first } fn new(value: T) -> Self { Self { first: value, second: 0 } } }",
        );
        let parsed = parse_ast(&source);
        assert!(!parsed.had_error);
        assert_eq!(parsed.unsupported_item_count, 0);
        assert_eq!(parsed.structs.len(), 1);
        let pair = &parsed.structs[0];
        assert_eq!(pair.name, "Pair");
        assert_eq!(pair.generic_parameters, ["T"]);
        assert!(pair.is_public);
        assert_eq!(pair.fields.len(), 2);
        assert_eq!(pair.fields[0].name.as_deref(), Some("first"));
        assert_eq!(pair.fields[0].type_text, "T");
        assert!(pair.fields[0].is_public);
        assert_eq!(pair.fields[1].name.as_deref(), Some("second"));
        assert!(!pair.fields[1].is_public);

        let methods: Vec<_> = parsed
            .functions
            .iter()
            .filter(|function| function.scope.iter().any(|scope| scope == "impl:Pair"))
            .collect();
        assert_eq!(methods.len(), 2);
        assert_eq!(methods[0].name, "first");
        assert!(methods[0].is_public);
        assert_eq!(methods[1].name, "new");
        assert_eq!(methods[1].parameters[0].type_text, "T");
        assert!(methods[1].is_generic);
        assert_eq!(methods[1].generic_parameters, ["T"]);
    }

    #[test]
    fn preserves_supported_derive_metadata() {
        let source = SourceFile::from_text("model.rs", "#[derive(Clone)] struct Value(i32);");
        let parsed = parse_ast(&source);
        assert_eq!(parsed.structs.len(), 1);
        assert_eq!(parsed.unsupported_item_count, 0);
        assert_eq!(parsed.structs[0].derived_traits, ["Clone"]);
    }

    #[test]
    fn parses_fieldless_enum_discriminants_and_scopes() {
        let source = SourceFile::from_text(
            "status.rs",
            "#[repr(i32)] #[derive(Clone, Copy, PartialEq, Eq)] pub enum Status { Ready, Busy = 5, Done }",
        );
        let parsed = parse_ast(&source);
        assert!(!parsed.had_error);
        assert_eq!(parsed.unsupported_item_count, 0);
        assert_eq!(parsed.enums.len(), 1);
        let status = &parsed.enums[0];
        assert_eq!(status.name, "Status");
        assert!(status.is_public);
        assert_eq!(status.derived_traits, ["Clone", "Copy", "PartialEq", "Eq"]);
        assert_eq!(
            status
                .variants
                .iter()
                .map(|variant| (variant.name.as_str(), variant.discriminant))
                .collect::<Vec<_>>(),
            [("Ready", 0), ("Busy", 5), ("Done", 6)]
        );

        let unsupported =
            SourceFile::from_text("payload.rs", "enum Message { Empty, Text(String) }");
        assert!(parse_ast(&unsupported).unsupported_item_count > 0);
    }

    #[test]
    fn preserves_macro_statement_path_tokens_and_span() {
        let source =
            SourceFile::from_text("macro.rs", "fn main() { println!(\"value={value}\"); }");
        let parsed = parse_ast(&source);
        assert!(!parsed.had_error);
        let body = parsed.functions[0].body.as_ref().unwrap();
        let BodyExprKind::Block { statements, .. } = &body.kind else {
            panic!("function body should be a block");
        };
        let BodyStatement::MacroInvocation {
            path,
            tokens,
            format_string,
            arguments,
            span,
        } = &statements[0]
        else {
            panic!("expected a preserved macro invocation");
        };
        assert_eq!(path, "println");
        assert!(tokens.contains("value={value}"));
        assert_eq!(format_string.as_deref(), Some("value={value}"));
        assert!(arguments.is_empty());
        assert_eq!(source.slice(span.clone()), "println!(\"value={value}\");");
    }

    #[test]
    fn preserves_format_macro_expressions_and_captures() {
        let source = SourceFile::from_text(
            "format.rs",
            "fn make(revision: u64) -> String { format!(\"item-{revision}\") }",
        );
        let parsed = parse_ast(&source);
        assert!(!parsed.had_error);
        let body = parsed.functions[0].body.as_ref().unwrap();
        let BodyExprKind::Block {
            tail: Some(expression),
            ..
        } = &body.kind
        else {
            panic!("expected the function's format expression");
        };
        let BodyExprKind::MacroInvocation(macro_expression) = &expression.kind else {
            panic!("expected a preserved expression macro");
        };
        assert_eq!(macro_expression.path, "format");
        assert!(macro_expression.tokens.contains("item-{revision}"));
        assert_eq!(
            macro_expression.format_string.as_deref(),
            Some("item-{revision}")
        );
        assert!(macro_expression.arguments.is_empty());
    }

    #[test]
    fn computes_literal_concat_macro_expansion() {
        let source = SourceFile::from_text(
            "concat.rs",
            "fn value() -> &'static str { concat!(\"qu\", 'i', 12, 1.5, true) }",
        );
        let parsed = parse_ast(&source);
        assert!(!parsed.had_error);
        let body = parsed.functions[0].body.as_ref().unwrap();
        let BodyExprKind::Block {
            tail: Some(expression),
            ..
        } = &body.kind
        else {
            panic!("expected the function's concat expression");
        };
        let BodyExprKind::MacroInvocation(macro_expression) = &expression.kind else {
            panic!("expected a preserved concat invocation");
        };
        assert_eq!(macro_expression.path, "concat");
        assert_eq!(
            macro_expression.const_expansion.as_deref(),
            Some("qui121.5true")
        );
    }

    #[test]
    fn expands_zero_argument_literal_macro_rules_invocations() {
        let source = SourceFile::from_text(
            "literal-macro.rs",
            "macro_rules! answer { () => { 42u64 }; } fn main() { let value = answer!(); }",
        );
        let parsed = parse_ast(&source);
        assert!(!parsed.had_error);
        assert_eq!(parsed.unsupported_item_count, 0);
        let BodyExprKind::Block { statements, .. } =
            &parsed.functions[0].body.as_ref().unwrap().kind
        else {
            panic!("expected the main function body");
        };
        let BodyStatement::Let {
            initializer: Some(expression),
            ..
        } = &statements[0]
        else {
            panic!("expected the macro expansion to initialize a local");
        };
        assert_eq!(
            expression.kind,
            BodyExprKind::Literal(LiteralKind::Integer {
                text: "42".to_owned(),
                suffix: "u64".to_owned(),
            })
        );
    }

    #[test]
    fn expands_comma_separated_expression_macro_repetitions() {
        let source = SourceFile::from_text(
            "repetition-macro.rs",
            "macro_rules! sum { ($($value:expr),+ $(,)?) => {{ let mut total: u64 = 0; $(total = total + $value;)+ total }}; } fn main() { let result = sum!(7, 11, 24,); }",
        );
        let parsed = parse_ast(&source);
        assert!(!parsed.had_error);
        assert_eq!(parsed.unsupported_item_count, 0);
        let BodyExprKind::Block { statements, .. } =
            &parsed.functions[0].body.as_ref().unwrap().kind
        else {
            panic!("expected the main function body");
        };
        let BodyStatement::Let {
            initializer: Some(expression),
            ..
        } = &statements[0]
        else {
            panic!("expected the macro-expanded local");
        };
        let BodyExprKind::Block { statements, tail } = &expression.kind else {
            panic!("expected the expression block transcribed by the macro");
        };
        assert_eq!(statements.len(), 4);
        assert!(
            matches!(tail.as_deref().map(|expr| &expr.kind), Some(BodyExprKind::Path(name)) if name == "total")
        );
    }

    #[test]
    fn rejects_unsupported_derives_and_invalid_field_trait_combinations() {
        let unsupported =
            SourceFile::from_text("unsupported.rs", "#[derive(Serialize)] struct Value(i32);");
        assert_eq!(parse_ast(&unsupported).unsupported_item_count, 1);

        let invalid = SourceFile::from_text("invalid.rs", "#[derive(Eq)] struct Value(f64);");
        assert_eq!(parse_ast(&invalid).unsupported_item_count, 1);
    }

    #[test]
    fn lowers_named_struct_literals_and_field_access() {
        let source = SourceFile::from_text(
            "model.rs",
            "fn main() { let point = Point { x: 40, y: helper(2) }; std::process::exit(point.y); }",
        );
        let parsed = parse_ast(&source);
        let body = parsed.functions[0].body.as_ref().unwrap();
        let BodyExprKind::Block { statements, .. } = &body.kind else {
            panic!("function body should be a block");
        };
        let BodyStatement::Let {
            initializer: Some(initializer),
            ..
        } = &statements[0]
        else {
            panic!("expected initialized local");
        };
        let BodyExprKind::StructLiteral { path, fields } = &initializer.kind else {
            panic!("expected named struct literal");
        };
        assert_eq!(path, "Point");
        assert_eq!(
            fields
                .iter()
                .map(|field| field.0.as_str())
                .collect::<Vec<_>>(),
            ["x", "y"]
        );
        let BodyStatement::Expression { expression, .. } = &statements[1] else {
            panic!("expected exit expression");
        };
        let BodyExprKind::Call { arguments, .. } = &expression.kind else {
            panic!("expected function call");
        };
        assert!(
            matches!(arguments[0].kind, BodyExprKind::FieldAccess { ref field, .. } if field == "y")
        );
    }

    #[test]
    fn lowers_tuple_index_projection() {
        let source = SourceFile::from_text(
            "tuple.rs",
            "fn pair() -> (i32, i32) { (1, 2) } fn main() { std::process::exit(pair().1); }",
        );
        let parsed = parse_ast(&source);
        let body = parsed.functions[1].body.as_ref().unwrap();
        let BodyExprKind::Block { statements, .. } = &body.kind else {
            panic!("function body should be a block");
        };
        let BodyStatement::Expression { expression, .. } = &statements[0] else {
            panic!("expected exit expression");
        };
        let BodyExprKind::Call { arguments, .. } = &expression.kind else {
            panic!("expected function call");
        };
        assert!(matches!(
            arguments[0].kind,
            BodyExprKind::FieldAccess { ref field, .. } if field == "1"
        ));
    }

    #[test]
    fn lowers_simple_tuple_destructuring_bindings() {
        let source = SourceFile::from_text(
            "tuple-binding.rs",
            "fn pair() { let (count, digest) = (1usize, 2u64); }",
        );
        let parsed = parse_ast(&source);
        let body = parsed.functions[0].body.as_ref().unwrap();
        let BodyExprKind::Block { statements, .. } = &body.kind else {
            panic!("function body should be a block");
        };
        let BodyStatement::LetTuple {
            names, initializer, ..
        } = &statements[0]
        else {
            panic!("expected a tuple binding");
        };
        assert_eq!(
            names.iter().map(String::as_str).collect::<Vec<_>>(),
            ["count", "digest"]
        );
        assert!(
            matches!(initializer.kind, BodyExprKind::Tuple(ref elements) if elements.len() == 2)
        );
    }

    #[test]
    fn lowers_scalar_method_calls_with_receiver_and_arguments() {
        let source = SourceFile::from_text(
            "wrapping.rs",
            "fn mix(state: u64, byte: u64) -> u64 { state.wrapping_mul(33).wrapping_add(byte) }",
        );
        let parsed = parse_ast(&source);
        let BodyExprKind::Block {
            tail: Some(expression),
            ..
        } = &parsed.functions[0].body.as_ref().unwrap().kind
        else {
            panic!("expected a tail expression");
        };
        let BodyExprKind::MethodCall {
            receiver,
            method,
            arguments,
        } = &expression.kind
        else {
            panic!("expected the outer method call");
        };
        assert_eq!(method, "wrapping_add");
        assert_eq!(arguments.len(), 1);
        assert!(
            matches!(receiver.kind, BodyExprKind::MethodCall { ref method, .. } if method == "wrapping_mul")
        );
    }

    #[test]
    fn preserves_mutable_receiver_type_and_field_assignment_target() {
        let source = SourceFile::from_text(
            "mut.rs",
            "struct Counter { value: u64 } impl Counter { fn increment(&mut self) { self.value = self.value + 1u64; } fn bump(mut self) -> Self { self.value = self.value + 1u64; self } }",
        );
        let parsed = parse_ast(&source);
        assert!(!parsed.had_error, "{:?}", parsed.diagnostics);
        let method = parsed
            .functions
            .iter()
            .find(|function| function.name == "increment")
            .unwrap();
        assert_eq!(method.parameters[0].type_text, "&mut Self");
        let Some(BodyExpr {
            kind: BodyExprKind::Block { statements, .. },
            ..
        }) = method.body.as_ref()
        else {
            panic!("expected method body block");
        };
        assert!(matches!(
            statements.first(),
            Some(BodyStatement::Assign { name, .. }) if name == "self.value"
        ));
        let bump = parsed
            .functions
            .iter()
            .find(|function| function.name == "bump")
            .unwrap();
        assert_eq!(bump.parameters[0].type_text, "mut Self");
    }

    #[test]
    fn parses_inline_external_modules_and_nested_import_trees() {
        let source = SourceFile::from_text(
            "test.rs",
            "pub mod net { pub mod client; use std::fmt::{self, Debug as D, Write}; }",
        );
        let parsed = parse(&source);
        assert!(!parsed.had_error);
        assert_eq!(parsed.modules.len(), 2);
        let net = &parsed.modules[0];
        assert_eq!(net.name, "net");
        assert!(net.is_public);
        assert!(net.body_span.is_some());
        let client = &parsed.modules[1];
        assert_eq!(client.name, "client");
        assert_eq!(client.parent_scope, ["mod:net"]);
        assert!(client.body_span.is_none());

        assert_eq!(parsed.imports.len(), 3);
        assert_eq!(parsed.imports[0].path, ["std", "fmt"]);
        assert_eq!(parsed.imports[1].path, ["std", "fmt", "Debug"]);
        assert_eq!(parsed.imports[1].alias.as_deref(), Some("D"));
        assert_eq!(parsed.imports[2].path, ["std", "fmt", "Write"]);
        assert!(
            parsed
                .imports
                .iter()
                .all(|import| import.scope == ["mod:net"])
        );
    }

    #[test]
    fn preserves_external_module_path_attributes() {
        let source =
            SourceFile::from_text("lib.rs", "#[path = \"implementation.rs\"] pub mod api;");
        let parsed = parse(&source);
        assert!(!parsed.had_error);
        assert_eq!(parsed.modules.len(), 1);
        assert_eq!(
            parsed.modules[0].path_override.as_deref(),
            Some("implementation.rs")
        );
    }

    #[test]
    fn validates_full_item_and_expression_grammar() {
        let valid = SourceFile::from_text(
            "valid.rs",
            "trait Family { type Member<'a> where Self: 'a; } async fn select<T>(input: Option<T>) -> T { let Some(value) = input else { panic!() }; value }",
        );
        assert!(!parse(&valid).had_error);

        let invalid = SourceFile::from_text("invalid.rs", "fn broken() { let value = ; }");
        let parsed = parse(&invalid);
        assert!(parsed.had_error);
        assert!(
            parsed
                .diagnostics
                .iter()
                .any(|diagnostic| diagnostic.message.contains("invalid Rust syntax"))
        );
        assert!(
            parsed
                .diagnostics
                .iter()
                .all(|diagnostic| diagnostic.span.end <= invalid.text().len())
        );
    }

    #[test]
    fn only_extracts_unqualified_direct_function_calls() {
        let source = SourceFile::from_text(
            "calls.rs",
            "fn plain() {} fn caller() { plain(); module::qualified(); Type::associated(); object.method(); }",
        );
        let parsed = parse(&source);
        assert!(!parsed.had_error);
        let caller = parsed
            .functions
            .iter()
            .find(|function| function.name == "caller")
            .unwrap();
        assert_eq!(caller.calls, ["plain"]);
    }

    #[test]
    fn preserves_qualified_calls_separately_from_local_calls() {
        let source = SourceFile::from_text(
            "calls.rs",
            "fn caller() { local(); dependency::api::run(); value.method(); } fn local() {}",
        );
        let parsed = parse(&source);
        let caller = parsed
            .functions
            .iter()
            .find(|function| function.name == "caller")
            .unwrap();
        assert_eq!(caller.calls, ["local"]);
        assert_eq!(caller.qualified_calls, ["dependency::api::run"]);
    }
}
