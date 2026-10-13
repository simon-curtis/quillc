//! Build Quill's item and body model from rust-analyzer syntax trees.

use std::collections::HashSet;
use std::ops::Range;

use quill_source::SourceFile;
use ra_ap_syntax::ast::{
    self, AstNode, HasArgList, HasAttrs, HasGenericArgs, HasGenericParams, HasLoopBody,
    HasModuleItem, HasName, HasTypeBounds, HasVisibility, RangeItem,
};
use ra_ap_syntax::{Edition, SyntaxKind, SyntaxNode};

use crate::span::SpanMap;
use crate::tt::{self, OwnedTt};
use crate::*;

const MAX_EXPANSION_DEPTH: usize = 32;

pub(crate) fn ra_edition(edition: &str) -> Edition {
    match edition {
        "2015" => Edition::Edition2015,
        "2018" => Edition::Edition2018,
        "2024" | "future" => Edition::Edition2024,
        _ => Edition::Edition2021,
    }
}

pub(crate) struct Collected {
    pub functions: Vec<FunctionItem>,
    pub structs: Vec<StructItem>,
    pub enums: Vec<EnumItem>,
    pub modules: Vec<ModuleItem>,
    pub imports: Vec<ImportItem>,
    pub unsupported_item_count: usize,
}

/// Parse `source` and collect its model, or return the first syntax error.
pub(crate) fn collect(
    source: &SourceFile,
    cfg: &quill_cfg::CfgSet,
    edition: &str,
    validate_edition: bool,
) -> Result<Collected, Diagnostic> {
    let text = source.text();
    let ra = ra_edition(edition);
    let mut parser_text = parser_text(text);
    let mut parse = ra_ap_syntax::SourceFile::parse(&parser_text, ra);
    if let Some(error) = parse
        .errors()
        .iter()
        .find(|error| !inside_token_tree(parse.tree().syntax(), error.range()))
        && let Some(identifier) =
            item_keyword_macro_identifier(&parser_text, error.range(), &error.to_string())
    {
        // RA currently interprets `safe!` as an item modifier
        // before it checks for an item macro. Changing the final ASCII letter
        // preserves token width and lets its ordinary macro-call grammar build
        // the right node; the compiler model does not resolve item macros.
        let mut repaired = parser_text.into_owned();
        repaired.replace_range(identifier.end - 1..identifier.end, "E");
        parser_text = std::borrow::Cow::Owned(repaired);
        parse = ra_ap_syntax::SourceFile::parse(&parser_text, ra);
    }
    let file = parse.tree();
    if let Some(error) = parse
        .errors()
        .iter()
        .filter(|error| {
            !inside_token_tree(file.syntax(), error.range())
                && !rustc_parser_accepts_ra_recovery(text, error.range(), &error.to_string())
        })
        .min_by_key(|error| error.range().start())
    {
        let offset = usize::from(error.range().start()).min(text.len());
        return Err(syntax_diagnostic(source, offset, &error.to_string()));
    }
    if validate_edition
        && ra == Edition::Edition2024
        && let Some((offset, message)) = edition_2024_error(file.syntax())
    {
        return Err(syntax_diagnostic(source, offset, message));
    }
    let mut trait_names = HashSet::new();
    collect_trait_names(file.syntax(), cfg, &mut trait_names);
    let mut collector = Collector {
        source,
        cfg,
        edition: ra,
        scope: Vec::new(),
        functions: Vec::new(),
        structs: Vec::new(),
        enums: Vec::new(),
        modules: Vec::new(),
        imports: Vec::new(),
        macro_expansions: Vec::new(),
        expansion_scopes: Vec::new(),
        impl_generic_parameters: Vec::new(),
        impl_generic_has_bounds: false,
        local_trait_names: trait_names,
        unsupported_item_count: 0,
    };
    for item in file.items() {
        collector.visit_item(&item);
    }
    let file_attributes = ast::attrs_with_doc_including_inner(&file)
        .filter(|attribute| attribute.kind().is_inner())
        .count();
    Ok(Collected {
        functions: collector.functions,
        structs: collector.structs,
        enums: collector.enums,
        modules: collector.modules,
        imports: collector.imports,
        unsupported_item_count: collector.unsupported_item_count + file_attributes,
    })
}

/// rust-analyzer intentionally diagnoses a few incomplete or semantically
/// invalid constructs that rustc's parser accepts into an AST. Keep those
/// narrow cases from turning parser recovery into a syntax rejection.
fn rustc_parser_accepts_ra_recovery(
    text: &str,
    range: ra_ap_syntax::TextRange,
    message: &str,
) -> bool {
    let start = usize::from(range.start()).min(text.len());
    let end = usize::from(range.end()).min(text.len());
    let token = text.get(start..end).unwrap_or_default();
    let line_start = text[..start].rfind('\n').map_or(0, |index| index + 1);
    let line_end = text[start..]
        .find('\n')
        .map_or(text.len(), |index| start + index);
    let line = &text[line_start..line_end];
    if has_parenthesized_trait_bounds(text) {
        let bound_range = if let Some(bounds) = line.find("fn f<T:") {
            let start = line_start + bounds + "fn f<T:".len();
            line[start - line_start..]
                .find(">()")
                .map(|end| (start, start + end))
        } else if let Some(ty) = line.find("let _: Box<") {
            let start = line_start + ty + "let _: Box<".len();
            line[start - line_start..]
                .find(';')
                .map(|end| (start, start + end + 1))
        } else {
            None
        };
        if bound_range.is_some_and(|(bound_start, bound_end)| {
            (bound_start..=bound_end).contains(&start)
        }) {
            return true;
        }
    }
    let impl_header_end = line.find('{').unwrap_or(line.len());
    let impl_tokens = line[..impl_header_end]
        .match_indices("impl")
        .filter_map(|(offset, _)| {
            let before = offset.checked_sub(1).and_then(|index| line.as_bytes().get(index));
            let after = line.as_bytes().get(offset + 4);
            let is_word = |byte: Option<&u8>| {
                byte.is_none_or(|byte| !byte.is_ascii_alphanumeric() && *byte != b'_')
            };
            (is_word(before) && is_word(after)).then_some(offset)
        })
        .collect::<Vec<_>>();
    if let Some(second_impl) = impl_tokens.get(1) {
        let recovery_start = line_start + second_impl.saturating_sub(1);
        let recovery_end = line_start + impl_header_end;
        if (recovery_start..=recovery_end).contains(&start) {
            return true;
        }
    }
    match message {
        "expected a string literal" => {
            token.chars().all(|ch| ch.is_ascii_alphanumeric() || ch == '_')
                && text[..start].trim_end().ends_with('=')
                && text[..start].rfind("cfg").is_some_and(|cfg| {
                    text[cfg..start].contains('(') && text[..cfg].contains('#')
                })
        }
        "expected trait or type" => {
            let remainder = text[start..].trim_start();
            remainder.starts_with("impl")
                && remainder[4..]
                    .trim_start()
                    .chars()
                    .next()
                    .is_some_and(|ch| ch.is_ascii_alphabetic() || ch == '_')
        }
        "illegal float literal" => has_split_scientific_tuple_field(text),
        _ => false,
    }
}

fn has_parenthesized_trait_bounds(text: &str) -> bool {
    text.contains("(Copy) + (?Sized)")
        || text.contains("(?Sized) + (for<'a> Trait<'a>)")
        || text.contains("(for<'a> Trait<'a>) + (Copy)")
}

fn has_split_scientific_tuple_field(text: &str) -> bool {
    let bytes = text.as_bytes();
    for (dot, byte) in bytes.iter().copied().enumerate() {
        if byte != b'.'
            || dot == 0
            || !(bytes[dot - 1].is_ascii_alphanumeric()
                || matches!(bytes[dot - 1], b'_' | b')' | b']'))
        {
            continue;
        }
        let mut index = dot + 1;
        let digits_start = index;
        while bytes.get(index).is_some_and(u8::is_ascii_digit) {
            index += 1;
        }
        if index == digits_start || !matches!(bytes.get(index), Some(b'e' | b'E')) {
            continue;
        }
        index += 1;
        if matches!(bytes.get(index), Some(b'+' | b'-')) {
            index += 1;
        }
        let exponent_start = index;
        while bytes.get(index).is_some_and(u8::is_ascii_digit) {
            index += 1;
        }
        if index > exponent_start
            && !bytes
                .get(index)
                .is_some_and(|byte| byte.is_ascii_alphanumeric() || *byte == b'_')
        {
            return true;
        }
    }
    false
}

fn item_keyword_macro_identifier(
    text: &str,
    error_range: ra_ap_syntax::TextRange,
    message: &str,
) -> Option<Range<usize>> {
    if message != "expected fn, trait or impl" {
        return None;
    }
    let error_start = usize::from(error_range.start());
    let bang = error_start
        + text[error_start..]
            .len()
            .saturating_sub(text[error_start..].trim_start().len());
    if text.as_bytes().get(bang) != Some(&b'!') {
        return None;
    }
    let path_end = text[..bang].trim_end().len();
    let start = text[..path_end]
        .char_indices()
        .rev()
        .take_while(|(_, ch)| ch.is_ascii_alphanumeric() || *ch == '_')
        .last()
        .map(|(index, _)| index)?;
    let identifier = &text[start..path_end];
    if identifier != "safe"
        || (start > 0
            && (text.as_bytes()[start - 1].is_ascii_alphanumeric()
                || matches!(text.as_bytes()[start - 1], b'_' | b'#')))
    {
        return None;
    }
    let delimiter = text[bang + 1..].trim_start().as_bytes().first().copied()?;
    matches!(delimiter, b'{' | b'(' | b'[').then_some(start..path_end)
}

/// Macro bodies are token streams, not Rust expressions. Ignore grammar
/// recovery diagnostics wholly contained by a balanced token-tree node; the
/// parser still reports an unclosed delimiter outside that node.
fn inside_token_tree(root: &SyntaxNode, range: ra_ap_syntax::TextRange) -> bool {
    root.descendants().any(|node| {
        node.kind() == SyntaxKind::TOKEN_TREE && {
            let tree = node.text_range();
            tree.start() <= range.start() && range.end() <= tree.end()
        }
    })
}

/// Text handed to rust-analyzer, with the same byte offsets as `text`.
///
/// rustc strips a byte-order mark and normalizes CRLF line endings before
/// lexing; rust-analyzer does neither, so a `\` + CRLF string continuation
/// or a CRLF inside a string literal would be rejected. Replace the mark with
/// spaces and each CRLF with LF + space: the space is whitespace everywhere a
/// line ending is, including after a string continuation. Literal values are
/// recomputed from the original text (see `Lowerer::literal`).
pub(crate) fn parser_text(text: &str) -> std::borrow::Cow<'_, str> {
    let bounds = parenthesized_bound_ranges(text);
    if !text.starts_with('\u{feff}') && !text.contains('\r') && bounds.is_empty() {
        return std::borrow::Cow::Borrowed(text);
    }
    let mut normalized = text.replace("\r\n", "\n ");
    if let Some(rest) = normalized.strip_prefix('\u{feff}') {
        normalized = format!("   {rest}");
    }
    for (open, close) in bounds.into_iter().rev() {
        normalized.replace_range(close..close + 1, " ");
        normalized.replace_range(open..open + 1, " ");
    }
    std::borrow::Cow::Owned(normalized)
}

/// Parentheses around individual trait bounds are accepted by rustc but the
/// pinned rust-analyzer grammar treats them as grouped types. Replace only
/// the grouping delimiters with spaces; byte offsets and the bound tokens stay
/// unchanged for the collector and diagnostics.
fn parenthesized_bound_ranges(text: &str) -> Vec<(usize, usize)> {
    if !has_close_then_plus(text.as_bytes()) {
        return Vec::new();
    }
    let bytes = text.as_bytes();
    let mut ranges = Vec::new();
    let mut stack = Vec::new();
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index..].starts_with(b"//") {
            index = bytes[index..]
                .iter()
                .position(|byte| *byte == b'\n')
                .map_or(bytes.len(), |offset| index + offset + 1);
            continue;
        }
        if bytes[index..].starts_with(b"/*") {
            index = skip_block_comment(bytes, index);
            continue;
        }
        if let Some(end) = raw_string_end(bytes, index) {
            index = end;
            continue;
        }
        if bytes[index] == b'"' {
            index = skip_quoted(bytes, index, b'"');
            continue;
        }
        if bytes[index] == b'\''
            && let Some(end) = char_literal_end(bytes, index)
        {
            index = end;
            continue;
        }
        let byte = bytes[index];
        match byte {
            b'(' => stack.push(index),
            b')' => {
                if let Some(open) = stack.pop() {
                    let before = bytes[..open]
                        .iter()
                        .rposition(|byte| !byte.is_ascii_whitespace())
                        .map(|index| bytes[index]);
                    let after = bytes[index + 1..]
                        .iter()
                        .find(|byte| !byte.is_ascii_whitespace())
                        .copied();
                    let first = bytes[open + 1..index]
                        .iter()
                        .find(|byte| !byte.is_ascii_whitespace())
                        .copied();
                    if matches!(before, Some(b'<' | b':' | b'+'))
                        && matches!(after, Some(b'+' | b'>' | b','))
                        && first.is_some_and(|byte| {
                            byte == b'?' || byte.is_ascii_alphabetic() || byte == b'_'
                        })
                        && is_type_bound_context(bytes, open)
                        && !bytes[open + 1..index].contains(&b'-')
                    {
                        ranges.push((open, index));
                    }
                }
            }
            _ => {}
        }
        index += 1;
    }
    ranges
}

fn has_close_then_plus(bytes: &[u8]) -> bool {
    bytes.iter().enumerate().any(|(index, byte)| {
        if *byte != b')' {
            return false;
        }
        let mut next = index + 1;
        while bytes.get(next).is_some_and(u8::is_ascii_whitespace) {
            next += 1;
        }
        bytes.get(next) == Some(&b'+')
    })
}

fn is_type_bound_context(bytes: &[u8], open: usize) -> bool {
    let mut context_start = 0;
    for index in 0..open {
        if matches!(bytes[index], b';' | b'{' | b'}') {
            context_start = index + 1;
        } else if bytes.get(index..index + 2) == Some(b"=>") {
            context_start = index + 2;
        }
    }
    let context = &bytes[context_start..open];
    let mut angle_depth = 0usize;
    for byte in context {
        match byte {
            b'<' => angle_depth += 1,
            b'>' => angle_depth = angle_depth.saturating_sub(1),
            _ => {}
        }
    }
    angle_depth > 0
        || context
            .split(|byte| !byte.is_ascii_alphanumeric() && *byte != b'_')
            .any(|word| word == b"where")
}

fn skip_quoted(bytes: &[u8], start: usize, quote: u8) -> usize {
    let mut index = start + 1;
    while index < bytes.len() {
        match bytes[index] {
            b'\\' => index = (index + 2).min(bytes.len()),
            byte if byte == quote => return index + 1,
            b'\n' if quote == b'\'' => return start + 1,
            _ => index += 1,
        }
    }
    bytes.len()
}

fn char_literal_end(bytes: &[u8], start: usize) -> Option<usize> {
    let content = start + 1;
    if bytes.get(content) == Some(&b'\\') {
        let end = skip_quoted(bytes, start, b'\'');
        return (end > content + 1 && end <= bytes.len() && bytes[end - 1] == b'\'')
            .then_some(end);
    }
    let width = match bytes.get(content)? {
        byte if byte.is_ascii() => 1,
        byte if byte & 0b1110_0000 == 0b1100_0000 => 2,
        byte if byte & 0b1111_0000 == 0b1110_0000 => 3,
        _ => 4,
    };
    (bytes.get(content + width) == Some(&b'\'')).then_some(content + width + 1)
}

fn raw_string_end(bytes: &[u8], start: usize) -> Option<usize> {
    let mut quote = start;
    if bytes.get(quote) == Some(&b'b') || bytes.get(quote) == Some(&b'c') {
        quote += 1;
    }
    if bytes.get(quote) != Some(&b'r') {
        return None;
    }
    quote += 1;
    let hashes_start = quote;
    while bytes.get(quote) == Some(&b'#') {
        quote += 1;
    }
    if bytes.get(quote) != Some(&b'"') {
        return None;
    }
    let hashes = quote - hashes_start;
    let mut index = quote + 1;
    while index < bytes.len() {
        if bytes[index] == b'"'
            && bytes.get(index + 1..index + 1 + hashes)
                == Some(&bytes[hashes_start..hashes_start + hashes])
        {
            return Some(index + 1 + hashes);
        }
        index += 1;
    }
    Some(bytes.len())
}

fn skip_block_comment(bytes: &[u8], start: usize) -> usize {
    let mut depth = 1usize;
    let mut index = start + 2;
    while index + 1 < bytes.len() {
        match &bytes[index..index + 2] {
            b"/*" => {
                depth += 1;
                index += 2;
            }
            b"*/" => {
                depth -= 1;
                index += 2;
                if depth == 0 {
                    return index;
                }
            }
            _ => index += 1,
        }
    }
    bytes.len()
}

fn syntax_diagnostic(source: &SourceFile, offset: usize, message: &str) -> Diagnostic {
    let text = source.text();
    let end = text[offset..]
        .chars()
        .next()
        .map_or(offset, |ch| offset + ch.len_utf8());
    Diagnostic {
        span: offset..end,
        message: format!("invalid Rust syntax: {message}"),
    }
}

/// Rust 2024 rules that rust-analyzer's parser does not reject itself.
fn edition_2024_error(root: &SyntaxNode) -> Option<(usize, &'static str)> {
    const UNSAFE_ATTRIBUTES: &[&str] = &["no_mangle", "export_name", "link_section", "naked"];
    for node in root.descendants() {
        if let Some(block) = ast::ExternBlock::cast(node.clone())
            && block.unsafe_token().is_none()
        {
            let range = crate::span::trimmed_range(&node);
            return Some((
                usize::from(range.start()),
                "extern blocks must be unsafe in Rust 2024",
            ));
        }
        if let Some(attribute) = ast::Attr::cast(node.clone())
            && let Some(name) = attribute_name(&attribute)
            && UNSAFE_ATTRIBUTES.contains(&name.as_str())
        {
            return Some((
                usize::from(node.text_range().start()),
                "this attribute is unsafe and must be wrapped in `unsafe(...)` in Rust 2024",
            ));
        }
    }
    None
}

// ---------------------------------------------------------------------------
// Attributes

/// Single-identifier name of an attribute's path (`unsafe` for
/// `#[unsafe(...)]`), matching Syn's `Attribute::path().is_ident(..)`.
fn attribute_name(attribute: &ast::Attr) -> Option<String> {
    match attribute.meta()? {
        ast::Meta::CfgMeta(_) => Some("cfg".to_owned()),
        ast::Meta::CfgAttrMeta(_) => Some("cfg_attr".to_owned()),
        ast::Meta::UnsafeMeta(_) => Some("unsafe".to_owned()),
        ast::Meta::PathMeta(meta) => single_ident(&meta.path()?),
        ast::Meta::KeyValueMeta(meta) => single_ident(&meta.path()?),
        ast::Meta::TokenTreeMeta(meta) => single_ident(&meta.path()?),
    }
}

fn single_ident(path: &ast::Path) -> Option<String> {
    let parts = path_parts(path);
    if parts.qself || parts.leading_colon || parts.segments.len() != 1 {
        return None;
    }
    Some(parts.segments[0].name.clone())
}

/// Token-tree arguments of a `#[name(...)]` attribute.
fn attribute_list(attribute: &ast::Attr, name: &str) -> Option<Option<ast::TokenTree>> {
    match attribute.meta()? {
        ast::Meta::TokenTreeMeta(meta) if single_ident(&meta.path()?)? == name => {
            Some(meta.token_tree())
        }
        ast::Meta::PathMeta(meta) if single_ident(&meta.path()?)? == name => Some(None),
        ast::Meta::KeyValueMeta(meta) if single_ident(&meta.path()?)? == name => Some(None),
        _ => None,
    }
}

/// Attributes as Syn exposed them on an item: outer attributes and doc
/// comments, followed by inner attributes and inner doc comments.
fn syn_attributes(owner: &dyn HasAttrs) -> Vec<ast::AnyAttr> {
    ast::attrs_with_doc_including_inner(owner).collect()
}

fn plain_attributes(attributes: &[ast::AnyAttr]) -> Vec<ast::Attr> {
    attributes
        .iter()
        .filter_map(|attribute| match attribute {
            ast::AnyAttr::Attr(attribute) => Some(attribute.clone()),
            ast::AnyAttr::DocComment(_) => None,
        })
        .collect()
}

fn unsupported_attribute_count(attributes: &[ast::AnyAttr]) -> usize {
    attributes
        .iter()
        .filter(|attribute| {
            let ast::AnyAttr::Attr(attribute) = attribute else {
                // Doc comments are `#[doc = ...]` attributes.
                return true;
            };
            match attribute_name(attribute).as_deref() {
                Some("cfg" | "cfg_attr" | "repr") => false,
                Some("derive") => supported_derives(std::slice::from_ref(attribute)).is_none(),
                _ => true,
            }
        })
        .count()
}

fn supported_derives(attributes: &[ast::Attr]) -> Option<Vec<String>> {
    let mut derives = Vec::new();
    for attribute in attributes {
        let Some(list) = attribute_list(attribute, "derive") else {
            continue;
        };
        let tokens = tt::owned_children(&list?, &SpanMap::default(), "");
        for (leading_colon, segments) in tt::derive_paths(&tokens)? {
            if leading_colon || segments.len() != 1 {
                return None;
            }
            let name = unraw(&segments[0]);
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

/// `repr(...)` token text as `proc_macro2` would display it.
fn repr_texts(attributes: &[ast::Attr]) -> Vec<Option<String>> {
    attributes
        .iter()
        .filter_map(|attribute| attribute_list(attribute, "repr"))
        .map(|list| {
            list.map(|list| {
                tt::display(&tt::owned_children(&list, &SpanMap::default(), ""))
                    .trim()
                    .to_owned()
            })
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Names and paths

pub(crate) fn unraw(text: &str) -> String {
    text.strip_prefix("r#").unwrap_or(text).to_owned()
}

fn name_text(name: Option<ast::Name>) -> String {
    name.map(|name| name.syntax().text().to_string())
        .unwrap_or_default()
}

fn is_public(visibility: Option<ast::Visibility>) -> bool {
    visibility.is_some_and(|visibility| {
        visibility.pub_token().is_some() && visibility.visibility_inner().is_none()
    })
}

#[derive(Clone, Debug)]
struct PathSegmentInfo {
    /// Raw identifier text, including any `r#` prefix.
    name: String,
    arguments: SegmentArguments,
}

#[derive(Clone, Debug)]
enum SegmentArguments {
    None,
    AngleBracketed(ast::GenericArgList),
    Other,
}

impl PathSegmentInfo {
    fn plain(&self) -> bool {
        matches!(self.arguments, SegmentArguments::None)
    }
}

#[derive(Clone, Debug, Default)]
struct PathParts {
    segments: Vec<PathSegmentInfo>,
    leading_colon: bool,
    qself: bool,
}

impl PathParts {
    fn joined(&self) -> String {
        self.segments
            .iter()
            .map(|segment| unraw(&segment.name))
            .collect::<Vec<_>>()
            .join("::")
    }

    fn all_plain(&self) -> bool {
        self.segments.iter().all(PathSegmentInfo::plain)
    }
}

fn path_parts(path: &ast::Path) -> PathParts {
    let mut parts = PathParts::default();
    for segment in path.segments() {
        if segment.type_anchor().is_some() {
            parts.qself = true;
            continue;
        }
        if parts.segments.is_empty() && segment.coloncolon_token().is_some() {
            parts.leading_colon = true;
        }
        let name = segment
            .name_ref()
            .map(|name| name.syntax().text().to_string())
            .unwrap_or_default();
        let arguments = if let Some(arguments) = segment.generic_arg_list() {
            SegmentArguments::AngleBracketed(arguments)
        } else if segment.parenthesized_arg_list().is_some()
            || segment.ret_type().is_some()
            || segment.return_type_syntax().is_some()
        {
            SegmentArguments::Other
        } else {
            SegmentArguments::None
        };
        parts.segments.push(PathSegmentInfo { name, arguments });
    }
    parts
}

fn type_path(ty: &ast::Type) -> Option<PathParts> {
    match ty {
        ast::Type::PathType(path) => Some(path_parts(&path.path()?)),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Generic bounds

struct TraitBound {
    path: PathParts,
    plain: bool,
}

enum BoundKind {
    Trait(TraitBound),
    Lifetime,
    Other,
}

fn bound_kind(bound: &ast::TypeBound) -> BoundKind {
    if bound.lifetime().is_some() {
        return BoundKind::Lifetime;
    }
    if bound.use_token().is_some() {
        return BoundKind::Other;
    }
    let Some(ast::Type::PathType(path_type)) = bound.ty() else {
        return BoundKind::Other;
    };
    let Some(path) = path_type.path() else {
        return BoundKind::Other;
    };
    BoundKind::Trait(TraitBound {
        path: path_parts(&path),
        plain: bound.for_binder().is_none()
            && bound.question_mark_token().is_none()
            && bound.const_token().is_none()
            && bound.async_token().is_none()
            && bound.tilde_token().is_none()
            && bound.l_brack_token().is_none(),
    })
}

fn type_parameters(list: Option<ast::GenericParamList>) -> Vec<ast::GenericParam> {
    list.map(|list| list.generic_params().collect())
        .unwrap_or_default()
}

fn type_param_bounds(parameter: &ast::TypeParam) -> Vec<BoundKind> {
    parameter
        .type_bound_list()
        .map(|list| list.bounds().map(|bound| bound_kind(&bound)).collect())
        .unwrap_or_default()
}

fn type_param_name(parameter: &ast::TypeParam) -> String {
    unraw(&name_text(parameter.name()))
}

fn type_parameter_names(parameters: &[ast::GenericParam]) -> Vec<String> {
    parameters
        .iter()
        .filter_map(|parameter| match parameter {
            ast::GenericParam::TypeParam(parameter) => Some(type_param_name(parameter)),
            _ => None,
        })
        .collect()
}

fn segment_names_are(path: &PathParts, expected: &[&[&str]]) -> bool {
    path.segments.len() == expected.len()
        && path
            .segments
            .iter()
            .zip(expected)
            .all(|(segment, names)| names.contains(&segment.name.as_str()))
}

fn is_supported_copy_bound(bound: &TraitBound) -> bool {
    let path = &bound.path;
    let supported_path = (segment_names_are(path, &[&["Copy"]])
        || segment_names_are(path, &[&["core", "std"], &["marker"], &["Copy"]]))
        && path.all_plain();
    supported_path && bound.plain
}

fn is_supported_add_bound(bound: &TraitBound, generic: &str) -> bool {
    let path = &bound.path;
    let supported_path = segment_names_are(path, &[&["Add"]])
        || segment_names_are(path, &[&["core", "std"], &["ops"], &["Add"]]);
    if !supported_path || !bound.plain {
        return false;
    }
    let Some(SegmentArguments::AngleBracketed(arguments)) =
        path.segments.last().map(|segment| &segment.arguments)
    else {
        return false;
    };
    let arguments: Vec<_> = arguments.generic_args().collect();
    matches!(arguments.as_slice(), [ast::GenericArg::AssocTypeArg(output)]
        if output.name_ref().is_some_and(|name| name.syntax().text() == "Output")
            && output.ty().as_ref().and_then(type_path).is_some_and(|path| !path.qself
                && path.segments.len() == 1
                && path.segments[0].name == generic
                && path.segments[0].plain()))
}

fn is_supported_as_ref_str_bound(bound: &TraitBound) -> bool {
    let path = &bound.path;
    let supported_path = segment_names_are(path, &[&["AsRef"]])
        || segment_names_are(path, &[&["core", "std"], &["convert"], &["AsRef"]]);
    if !supported_path || !bound.plain {
        return false;
    }
    let Some(SegmentArguments::AngleBracketed(arguments)) =
        path.segments.last().map(|segment| &segment.arguments)
    else {
        return false;
    };
    let arguments: Vec<_> = arguments.generic_args().collect();
    matches!(arguments.as_slice(), [ast::GenericArg::TypeArg(argument)]
        if argument.ty().as_ref().and_then(type_path).is_some_and(|path| !path.qself
            && path.segments.len() == 1
            && path.segments[0].name == "str"
            && path.segments[0].plain()))
}

fn is_local_trait_bound(bound: &TraitBound, local_traits: &HashSet<String>) -> bool {
    bound.path.segments.len() == 1
        && bound.path.segments[0].plain()
        && local_traits.contains(&unraw(&bound.path.segments[0].name))
        && bound.plain
}

fn generic_parameters_have_unsupported_bounds(
    parameters: &[ast::GenericParam],
    local_traits: &HashSet<String>,
) -> bool {
    parameters.iter().any(|parameter| match parameter {
        ast::GenericParam::TypeParam(parameter) => {
            let generic = type_param_name(parameter);
            parameter.default_type().is_some()
                || type_param_bounds(parameter)
                    .iter()
                    .any(|bound| match bound {
                        BoundKind::Trait(bound) => {
                            !is_supported_copy_bound(bound)
                                && !is_supported_as_ref_str_bound(bound)
                                && !is_supported_add_bound(bound, &generic)
                                && !is_local_trait_bound(bound, local_traits)
                        }
                        BoundKind::Lifetime | BoundKind::Other => true,
                    })
        }
        ast::GenericParam::LifetimeParam(_) | ast::GenericParam::ConstParam(_) => true,
    })
}

fn parameters_with_bound(
    parameters: &[ast::GenericParam],
    supported: impl Fn(&TraitBound, &str) -> bool,
) -> Vec<String> {
    parameters
        .iter()
        .filter_map(|parameter| match parameter {
            ast::GenericParam::TypeParam(parameter) => {
                let name = type_param_name(parameter);
                type_param_bounds(parameter)
                    .iter()
                    .any(|bound| matches!(bound, BoundKind::Trait(bound) if supported(bound, &name)))
                    .then_some(name)
            }
            _ => None,
        })
        .collect()
}

fn simple_generic_trait_bounds(parameters: &[ast::GenericParam]) -> Vec<(String, String)> {
    let mut bounds = Vec::new();
    for parameter in parameters {
        let ast::GenericParam::TypeParam(parameter) = parameter else {
            continue;
        };
        let name = type_param_name(parameter);
        for bound in type_param_bounds(parameter) {
            if let BoundKind::Trait(bound) = bound
                && !is_supported_copy_bound(&bound)
                && !is_supported_as_ref_str_bound(&bound)
                && !is_supported_add_bound(&bound, &name)
                && bound.path.segments.len() == 1
                && bound.path.segments[0].plain()
                && bound.plain
            {
                bounds.push((name.clone(), bound.path.joined()));
            }
        }
    }
    bounds
}

// ---------------------------------------------------------------------------
// Items

/// Whether `node` is an item in the sense of Syn's `syn::Item`, rather than
/// an expression-position macro call (which rust-analyzer also models with
/// a `MACRO_CALL` node).
fn as_item(node: &SyntaxNode) -> Option<ast::Item> {
    if node.kind() == SyntaxKind::MACRO_CALL
        && !node.parent().is_some_and(|parent| {
            matches!(
                parent.kind(),
                SyntaxKind::SOURCE_FILE | SyntaxKind::ITEM_LIST | SyntaxKind::STMT_LIST
            )
        })
    {
        return None;
    }
    if matches!(
        node.parent().map(|parent| parent.kind()),
        Some(SyntaxKind::ASSOC_ITEM_LIST | SyntaxKind::EXTERN_ITEM_LIST)
    ) {
        return None;
    }
    ast::Item::cast(node.clone())
}

fn collect_trait_names(node: &SyntaxNode, cfg: &quill_cfg::CfgSet, names: &mut HashSet<String>) {
    for child in node.children() {
        if child.kind() == SyntaxKind::TOKEN_TREE {
            continue;
        }
        if let Some(item) = as_item(&child) {
            let attributes = plain_attributes(&syn_attributes(&item));
            if !cfg.attributes_enabled(&attributes) {
                continue;
            }
            if let ast::Item::Trait(item) = &item
                && item.eq_token().is_none()
            {
                names.insert(unraw(&name_text(item.name())));
            }
        }
        collect_trait_names(&child, cfg, names);
    }
}

fn function_has_plain_signature(function: &ast::Fn) -> bool {
    type_parameters(function.generic_param_list()).is_empty()
        && function.where_clause().is_none()
        && function.const_token().is_none()
        && function.async_token().is_none()
        && function.unsafe_token().is_none()
        && function.abi().is_none()
}

fn supported_local_trait(item: &ast::Trait) -> bool {
    let items: Vec<_> = item
        .assoc_item_list()
        .map(|list| list.assoc_items().collect())
        .unwrap_or_default();
    type_parameters(item.generic_param_list()).is_empty()
        && item.where_clause().is_none()
        && item
            .type_bound_list()
            .is_none_or(|bounds| bounds.bounds().next().is_none())
        && !items.is_empty()
        && items.iter().all(|trait_item| match trait_item {
            ast::AssocItem::Fn(function) => {
                function.body().is_none()
                    && function_has_plain_signature(function)
                    && function
                        .param_list()
                        .and_then(|list| list.self_param())
                        .is_none_or(|receiver| {
                            receiver.amp_token().is_some()
                                && receiver.lifetime().is_none()
                                && receiver.mut_token().is_none()
                                && receiver.ty().is_none()
                        })
            }
            _ => false,
        })
}

fn supported_local_trait_impl(item: &ast::Impl) -> bool {
    let Some(trait_type) = item.trait_() else {
        return true;
    };
    type_parameters(item.generic_param_list()).is_empty()
        && item.where_clause().is_none()
        && item
            .self_ty()
            .as_ref()
            .and_then(type_path)
            .is_some_and(|path| !path.qself && path.all_plain())
        && type_path(&trait_type).is_some_and(|path| {
            !path.qself && path.segments.len() == 1 && path.segments[0].plain()
        })
        && item
            .assoc_item_list()
            .into_iter()
            .flat_map(|list| list.assoc_items())
            .all(|item| match item {
                ast::AssocItem::Fn(function) => function_has_plain_signature(&function),
                _ => false,
            })
}

struct LocalMacroExpansion {
    name: String,
    scope: Vec<String>,
    rule: LocalMacroRule,
}

#[derive(Clone)]
enum LocalMacroRule {
    ZeroArgLiteral(Vec<OwnedTt>),
    RepeatSeparatedExpr {
        capture: String,
        separator: char,
        allow_empty: bool,
        allow_trailing_separator: bool,
        transcriber: Vec<OwnedTt>,
    },
}

struct Collector<'a> {
    source: &'a SourceFile,
    cfg: &'a quill_cfg::CfgSet,
    edition: Edition,
    scope: Vec<String>,
    functions: Vec<FunctionItem>,
    structs: Vec<StructItem>,
    enums: Vec<EnumItem>,
    modules: Vec<ModuleItem>,
    imports: Vec<ImportItem>,
    macro_expansions: Vec<LocalMacroExpansion>,
    /// Scopes whose local macros expand in the bodies being visited.
    expansion_scopes: Vec<Vec<String>>,
    impl_generic_parameters: Vec<String>,
    impl_generic_has_bounds: bool,
    local_trait_names: HashSet<String>,
    unsupported_item_count: usize,
}

impl Collector<'_> {
    fn span(&self, node: &SyntaxNode) -> Range<usize> {
        SpanMap::default().node(node)
    }

    fn slice(&self, node: &SyntaxNode) -> String {
        self.source.slice(self.span(node)).to_owned()
    }

    fn lowerer(&self) -> Lowerer<'_> {
        Lowerer {
            source: self.source,
            edition: self.edition,
            map: SpanMap::default(),
            definitions: &self.macro_expansions,
            scopes: &self.expansion_scopes,
            item_scope: &self.scope,
            depth: 0,
        }
    }

    /// Visit items nested anywhere below `node` (Syn's default visitor
    /// descent through expressions, types and statements).
    fn visit_nested_items(&mut self, node: &SyntaxNode) {
        for child in node.children() {
            if child.kind() == SyntaxKind::TOKEN_TREE {
                continue;
            }
            if let Some(item) = as_item(&child) {
                self.visit_item(&item);
            } else {
                self.visit_nested_items(&child);
            }
        }
    }

    fn visit_item(&mut self, item: &ast::Item) {
        let attributes = syn_attributes(item);
        if !self.cfg.attributes_enabled(&plain_attributes(&attributes)) {
            return;
        }
        let supported_literal_macro = matches!(item, ast::Item::MacroRules(rules)
            if rules.name().is_some() && supported_macro_rule(rules).is_some());
        let supported_kind = match item {
            ast::Item::Fn(function) => function.body().is_some(),
            ast::Item::Trait(item) => item.eq_token().is_none(),
            ast::Item::Struct(_) | ast::Item::Enum(_) | ast::Item::Impl(_) => true,
            _ => false,
        };
        if !supported_kind && !supported_literal_macro {
            self.unsupported_item_count += 1;
        }
        if supported_kind {
            self.unsupported_item_count += unsupported_attribute_count(&attributes);
        }
        match item {
            ast::Item::Fn(function) if function.body().is_some() => self.visit_free_fn(function),
            // Syn keeps bodiless free functions as unparsed verbatim items.
            ast::Item::Fn(_) => {}
            ast::Item::MacroRules(rules) => {
                self.unsupported_item_count += unsupported_attribute_count(&attributes);
                if let Some(rule) = supported_macro_rule(rules) {
                    self.macro_expansions.push(LocalMacroExpansion {
                        name: name_text(rules.name()),
                        scope: self.scope.clone(),
                        rule,
                    });
                }
            }
            ast::Item::MacroCall(_) => {
                self.unsupported_item_count += unsupported_attribute_count(&attributes);
            }
            ast::Item::Struct(item) => self.visit_struct(item, &attributes),
            ast::Item::Enum(item) => self.visit_enum(item, &attributes),
            ast::Item::Module(item) => self.visit_module(item, &attributes),
            ast::Item::Impl(item) => self.visit_impl(item),
            ast::Item::Trait(item) if item.eq_token().is_none() => self.visit_trait(item),
            ast::Item::ExternBlock(item) => self.visit_extern_block(item),
            ast::Item::Use(item) => self.visit_use(item),
            other => self.visit_nested_items(other.syntax()),
        }
    }

    fn visit_free_fn(&mut self, function: &ast::Fn) {
        let pushed = !self.macro_expansions.is_empty();
        if pushed {
            self.expansion_scopes.push(self.scope.clone());
        }
        let span = self.span(function.syntax());
        self.add_function(
            function,
            is_public(function.visibility()),
            span,
            function.body(),
        );
        self.visit_function_body(function);
        if pushed {
            self.expansion_scopes.pop();
        }
    }

    fn visit_function_body(&mut self, function: &ast::Fn) {
        if let Some(body) = function.body() {
            self.scope.push(format!("fn:{}", name_text(function.name())));
            self.visit_nested_items(body.syntax());
            self.scope.pop();
        }
    }

    fn add_function(
        &mut self,
        function: &ast::Fn,
        is_public: bool,
        span: Range<usize>,
        body: Option<ast::BlockExpr>,
    ) {
        let lowerer = self.lowerer();
        let mut calls = Vec::new();
        let mut qualified_calls = Vec::new();
        if let Some(body) = &body {
            lowerer.collect_calls(body.syntax(), &mut calls, &mut qualified_calls);
        }
        let mut parameters = Vec::new();
        if let Some(list) = function.param_list() {
            if let Some(receiver) = list.self_param() {
                let type_text = if let Some(ty) = receiver.ty() {
                    self.slice(ty.syntax())
                } else if receiver.amp_token().is_some() {
                    format!(
                        "&{}{}Self",
                        receiver
                            .lifetime()
                            .map(|lifetime| format!("{} ", lifetime.syntax().text()))
                            .unwrap_or_default(),
                        if receiver.mut_token().is_some() {
                            "mut "
                        } else {
                            ""
                        }
                    )
                } else if receiver.mut_token().is_some() {
                    "mut Self".to_owned()
                } else {
                    "Self".to_owned()
                };
                parameters.push(ParameterItem {
                    name: Some("self".to_owned()),
                    mutable: type_text.starts_with("mut ") || type_text.starts_with("&mut "),
                    type_text,
                    span: self.span(receiver.syntax()),
                });
            }
            for parameter in list.params() {
                let Some(ty) = parameter.ty() else {
                    // C variadics are not ordinary inputs in Syn.
                    continue;
                };
                parameters.push(ParameterItem {
                    name: match parameter.pat() {
                        Some(ast::Pat::IdentPat(pattern)) => Some(unraw(&name_text(pattern.name()))),
                        _ => None,
                    },
                    mutable: matches!(parameter.pat(), Some(ast::Pat::IdentPat(pattern)) if pattern.mut_token().is_some()),
                    type_text: self.slice(ty.syntax()),
                    span: self.span(parameter.syntax()),
                });
            }
        }
        let return_type = function
            .ret_type()
            .and_then(|ret| ret.ty())
            .map(|ty| self.slice(ty.syntax()));
        let generics = type_parameters(function.generic_param_list());
        let mut generic_parameters = type_parameter_names(&generics);
        for parameter in &self.impl_generic_parameters {
            if !generic_parameters.contains(parameter) {
                generic_parameters.push(parameter.clone());
            }
        }
        let generic_has_bounds = self.impl_generic_has_bounds
            || function.where_clause().is_some()
            || generic_parameters_have_unsupported_bounds(&generics, &self.local_trait_names);
        let lowered_body = body.as_ref().map(|block| lowerer.lower_block(block));
        let name = function.name();
        self.functions.push(FunctionItem {
            name: unraw(&name_text(name.clone())),
            scope: self.scope.clone(),
            calls,
            qualified_calls,
            name_span: name
                .map(|name| self.span(name.syntax()))
                .unwrap_or(span.start..span.start),
            span,
            body_span: body.as_ref().map(|block| self.span(block.syntax())),
            parameters,
            return_type,
            body: lowered_body,
            is_public,
            is_generic: !generic_parameters.is_empty(),
            generic_parameters,
            generic_copy_parameters: parameters_with_bound(&generics, |bound, _| {
                is_supported_copy_bound(bound)
            }),
            generic_as_ref_str_parameters: parameters_with_bound(&generics, |bound, _| {
                is_supported_as_ref_str_bound(bound)
            }),
            generic_add_parameters: parameters_with_bound(&generics, is_supported_add_bound),
            generic_trait_bounds: simple_generic_trait_bounds(&generics),
            generic_has_bounds,
            is_async: function.async_token().is_some(),
            is_const: function.const_token().is_some(),
            is_unsafe: function.unsafe_token().is_some(),
            has_abi: function.abi().is_some(),
        });
    }

    fn visit_struct(&mut self, item: &ast::Struct, attributes: &[ast::AnyAttr]) {
        let plain = plain_attributes(attributes);
        let derived_traits = supported_derives(&plain).unwrap_or_default();
        let fields: Vec<(Option<String>, Option<ast::Type>, SyntaxNode, bool, Vec<ast::AnyAttr>)> =
            match item.field_list() {
                Some(ast::FieldList::RecordFieldList(list)) => list
                    .fields()
                    .map(|field| {
                        (
                            Some(name_text(field.name())),
                            field.ty(),
                            field.syntax().clone(),
                            is_public(field.visibility()),
                            field.attrs_with_doc().collect(),
                        )
                    })
                    .collect(),
                Some(ast::FieldList::TupleFieldList(list)) => list
                    .fields()
                    .map(|field| {
                        (
                            None,
                            field.ty(),
                            field.syntax().clone(),
                            is_public(field.visibility()),
                            field.attrs_with_doc().collect(),
                        )
                    })
                    .collect(),
                None => Vec::new(),
            };
        for attribute in &plain {
            if attribute_list(attribute, "derive").is_some()
                && supported_derives(std::slice::from_ref(attribute)).is_some()
                && derived_traits.iter().any(|trait_name| {
                    fields.iter().any(|(_, ty, _, _, _)| {
                        !ty.as_ref()
                            .is_some_and(|ty| field_supports_derive(ty, trait_name))
                    })
                })
            {
                self.unsupported_item_count += 1;
            }
        }
        self.unsupported_item_count += fields
            .iter()
            .map(|(_, _, _, _, attributes)| unsupported_attribute_count(attributes))
            .sum::<usize>();
        let reprs = repr_texts(&plain);
        let fields = fields
            .into_iter()
            .map(|(name, ty, node, is_public, _)| StructFieldItem {
                name,
                type_text: ty.map(|ty| self.slice(ty.syntax())).unwrap_or_default(),
                span: self.span(&node),
                is_public,
            })
            .collect();
        self.structs.push(StructItem {
            name: unraw(&name_text(item.name())),
            scope: self.scope.clone(),
            fields,
            generic_parameters: type_parameter_names(&type_parameters(item.generic_param_list())),
            derived_traits,
            repr_c: reprs.iter().any(|repr| repr.as_deref() == Some("C")),
            repr_layout_supported: reprs
                .iter()
                .all(|repr| matches!(repr.as_deref(), Some("" | "C"))),
            span: self.span(item.syntax()),
            is_public: is_public(item.visibility()),
        });
    }

    fn visit_enum(&mut self, item: &ast::Enum, attributes: &[ast::AnyAttr]) {
        // The Syn collector counted enum attributes in both its item and
        // enum visitors; keep the established count.
        self.unsupported_item_count += unsupported_attribute_count(attributes);
        let plain = plain_attributes(attributes);
        let mut next_discriminant = Some(0i32);
        let mut variants = Vec::new();
        let mut variant_names = HashSet::new();
        let mut discriminants = HashSet::new();
        for variant in item
            .variant_list()
            .into_iter()
            .flat_map(|list| list.variants())
        {
            let variant_attributes: Vec<_> = variant.attrs_with_doc().collect();
            self.unsupported_item_count += unsupported_attribute_count(&variant_attributes);
            if variant.field_list().is_some() {
                self.unsupported_item_count += 1;
            }
            let explicit = variant.const_arg().and_then(|arg| arg.expr());
            let discriminant = match &explicit {
                Some(expression) => parse_i32_discriminant(expression),
                None => next_discriminant,
            };
            let Some(discriminant) = discriminant else {
                self.unsupported_item_count += 1;
                continue;
            };
            let name = unraw(&name_text(variant.name()));
            if !variant_names.insert(name.clone()) || !discriminants.insert(discriminant) {
                self.unsupported_item_count += 1;
            }
            variants.push(EnumVariantItem {
                name,
                discriminant,
                span: self.span(variant.syntax()),
            });
            next_discriminant = discriminant.checked_add(1);
        }
        if !repr_texts(&plain)
            .iter()
            .all(|repr| repr.as_deref() == Some("i32"))
        {
            self.unsupported_item_count += 1;
        }
        self.enums.push(EnumItem {
            name: unraw(&name_text(item.name())),
            scope: self.scope.clone(),
            variants,
            derived_traits: supported_derives(&plain).unwrap_or_default(),
            span: self.span(item.syntax()),
            is_public: is_public(item.visibility()),
        });
    }

    fn visit_module(&mut self, item: &ast::Module, attributes: &[ast::AnyAttr]) {
        let raw_name = name_text(item.name());
        self.modules.push(ModuleItem {
            name: unraw(&raw_name),
            parent_scope: self.scope.clone(),
            span: self.span(item.syntax()),
            body_span: item.item_list().map(|list| self.span(list.syntax())),
            path_override: self
                .cfg
                .active_string_attribute(&plain_attributes(attributes), "path"),
            is_public: is_public(item.visibility()),
        });
        if let Some(list) = item.item_list() {
            self.scope.push(format!("mod:{raw_name}"));
            for nested in list.items() {
                self.visit_item(&nested);
            }
            self.scope.pop();
        }
    }

    fn visit_impl(&mut self, item: &ast::Impl) {
        if !supported_local_trait_impl(item) {
            self.unsupported_item_count += 1;
        }
        let name = item
            .self_ty()
            .as_ref()
            .and_then(type_path)
            .and_then(|path| path.segments.last().map(|segment| unraw(&segment.name)))
            .unwrap_or_else(|| "_".to_owned());
        let generics = type_parameters(item.generic_param_list());
        let previous_parameter_count = self.impl_generic_parameters.len();
        let previous_has_bounds = self.impl_generic_has_bounds;
        self.impl_generic_parameters
            .extend(type_parameter_names(&generics));
        self.impl_generic_has_bounds = item.where_clause().is_some()
            || generic_parameters_have_unsupported_bounds(&generics, &HashSet::new())
            // Trait-bound metadata is currently carried only from function
            // parameters, so bounded impl parameters stay outside the subset.
            || !parameters_with_bound(&generics, |bound, _| is_supported_copy_bound(bound))
                .is_empty()
            || !parameters_with_bound(&generics, |bound, _| is_supported_as_ref_str_bound(bound))
                .is_empty();
        self.scope.push(format!("impl:{name}"));
        let trait_path = item.trait_().as_ref().map(|ty| {
            type_path(ty)
                .map(|path| {
                    path.segments
                        .iter()
                        .map(|segment| unraw(&segment.name))
                        .collect::<Vec<_>>()
                        .join("::")
                })
                .unwrap_or_default()
        });
        if let Some(trait_path) = &trait_path {
            self.scope.push(format!("traitimpl:{trait_path}"));
        }
        // Syn's default visitor also reached items nested in the impl header
        // (const blocks in generic arguments or where clauses).
        for child in item.syntax().children() {
            if child.kind() != SyntaxKind::ASSOC_ITEM_LIST {
                self.visit_nested_items(&child);
            }
        }
        for assoc in item
            .assoc_item_list()
            .into_iter()
            .flat_map(|list| list.assoc_items())
        {
            match assoc {
                ast::AssocItem::Fn(function) if function.body().is_some() => {
                    let attributes = syn_attributes(&function);
                    if !self.cfg.attributes_enabled(&plain_attributes(&attributes)) {
                        continue;
                    }
                    self.unsupported_item_count += unsupported_attribute_count(&attributes);
                    let span = self.span(function.syntax());
                    self.add_function(
                        &function,
                        is_public(function.visibility()),
                        span,
                        function.body(),
                    );
                    self.visit_function_body(&function);
                }
                ast::AssocItem::Fn(_) | ast::AssocItem::MacroCall(_) => {}
                other => self.visit_nested_items(other.syntax()),
            }
        }
        if trait_path.is_some() {
            self.scope.pop();
        }
        self.scope.pop();
        self.impl_generic_parameters
            .truncate(previous_parameter_count);
        self.impl_generic_has_bounds = previous_has_bounds;
    }

    fn visit_trait(&mut self, item: &ast::Trait) {
        if !supported_local_trait(item) {
            self.unsupported_item_count += 1;
        }
        self.scope.push(format!("trait:{}", name_text(item.name())));
        for child in item.syntax().children() {
            if child.kind() != SyntaxKind::ASSOC_ITEM_LIST {
                self.visit_nested_items(&child);
            }
        }
        for assoc in item
            .assoc_item_list()
            .into_iter()
            .flat_map(|list| list.assoc_items())
        {
            match assoc {
                ast::AssocItem::Fn(function) => {
                    let attributes = syn_attributes(&function);
                    if !self.cfg.attributes_enabled(&plain_attributes(&attributes)) {
                        continue;
                    }
                    let span = self.span(function.syntax());
                    self.add_function(&function, false, span, function.body());
                    self.visit_function_body(&function);
                }
                ast::AssocItem::MacroCall(_) => {}
                other => self.visit_nested_items(other.syntax()),
            }
        }
        self.scope.pop();
    }

    fn visit_extern_block(&mut self, item: &ast::ExternBlock) {
        for foreign in item
            .extern_item_list()
            .into_iter()
            .flat_map(|list| list.extern_items())
        {
            if let ast::ExternItem::Fn(function) = foreign {
                let attributes = plain_attributes(&syn_attributes(&function));
                if !self.cfg.attributes_enabled(&attributes) {
                    continue;
                }
                let span = self.span(function.syntax());
                self.add_function(&function, is_public(function.visibility()), span, None);
            }
        }
    }

    fn visit_use(&mut self, item: &ast::Use) {
        let span = self.span(item.syntax());
        let is_public = is_public(item.visibility());
        if let Some(tree) = item.use_tree() {
            flatten_use(&tree, &[], &self.scope, &span, is_public, &mut self.imports);
        }
    }
}

fn flatten_use(
    tree: &ast::UseTree,
    prefix: &[String],
    scope: &[String],
    span: &Range<usize>,
    is_public: bool,
    output: &mut Vec<ImportItem>,
) {
    let mut path = prefix.to_vec();
    let mut raw_last = None;
    if let Some(tree_path) = tree.path() {
        let segments: Vec<String> = tree_path
            .segments()
            .map(|segment| {
                segment
                    .name_ref()
                    .map(|name| name.syntax().text().to_string())
                    .unwrap_or_default()
            })
            .collect();
        let is_leaf = tree.use_tree_list().is_none() && tree.star_token().is_none();
        for (index, segment) in segments.iter().enumerate() {
            if is_leaf && index + 1 == segments.len() {
                raw_last = Some(segment.clone());
            } else {
                path.push(unraw(segment));
            }
        }
    }
    let import = |path: Vec<String>, alias: Option<String>, is_glob: bool| ImportItem {
        scope: scope.to_vec(),
        path,
        alias,
        is_glob,
        span: span.clone(),
        is_public,
    };
    if let Some(list) = tree.use_tree_list() {
        for nested in list.use_trees() {
            flatten_use(&nested, &path, scope, span, is_public, output);
        }
    } else if tree.star_token().is_some() {
        output.push(import(path, None, true));
    } else if let Some(last) = raw_last {
        if !(last == "self" && !path.is_empty()) {
            path.push(unraw(&last));
        }
        let alias = tree.rename().map(|rename| {
            rename
                .name()
                .map(|name| name.syntax().text().to_string())
                .unwrap_or_else(|| "_".to_owned())
        });
        output.push(import(path, alias, false));
    }
}

fn field_supports_derive(ty: &ast::Type, trait_name: &str) -> bool {
    const SCALARS: &[&str] = &[
        "bool", "char", "u8", "u16", "u32", "u64", "u128", "usize", "i8", "i16", "i32", "i64",
        "i128", "isize",
    ];
    if trait_name == "Copy" {
        return match ty {
            ast::Type::RefType(reference) => reference.mut_token().is_none(),
            ast::Type::PtrType(_) => true,
            ast::Type::ArrayType(array) => array
                .ty()
                .is_some_and(|element| field_supports_derive(&element, trait_name)),
            ast::Type::TupleType(tuple) => tuple
                .fields()
                .all(|element| field_supports_derive(&element, trait_name)),
            ast::Type::PathType(_) => type_path(ty).is_some_and(|path| {
                !path.qself
                    && path.segments.last().is_some_and(|segment| {
                        let name = unraw(&segment.name);
                        (SCALARS.contains(&name.as_str()) || name == "f32" || name == "f64")
                            && segment.plain()
                    })
            }),
            _ => false,
        };
    }
    match ty {
        ast::Type::RefType(reference) => reference
            .ty()
            .is_some_and(|element| field_supports_derive(&element, trait_name)),
        ast::Type::ArrayType(array) => array
            .ty()
            .is_some_and(|element| field_supports_derive(&element, trait_name)),
        ast::Type::TupleType(tuple) => tuple
            .fields()
            .all(|element| field_supports_derive(&element, trait_name)),
        ast::Type::PathType(_) => {
            let Some(path) = type_path(ty).filter(|path| !path.qself) else {
                return false;
            };
            let Some(segment) = path.segments.last() else {
                return false;
            };
            let name = unraw(&segment.name);
            match name.as_str() {
                "String" => true,
                name if SCALARS.contains(&name) => true,
                "f32" | "f64" => trait_name != "Eq",
                "Vec" | "VecDeque" | "BTreeSet" | "HashSet" | "BTreeMap" | "HashMap" => {
                    let SegmentArguments::AngleBracketed(arguments) = &segment.arguments else {
                        return false;
                    };
                    let arguments: Vec<_> = arguments.generic_args().collect();
                    if matches!(name.as_str(), "BTreeMap" | "HashMap") {
                        let types: Vec<_> = arguments
                            .iter()
                            .filter_map(|argument| match argument {
                                ast::GenericArg::TypeArg(argument) => argument.ty(),
                                _ => None,
                            })
                            .collect();
                        types.len() == 2
                            && types.iter().all(|ty| field_supports_derive(ty, trait_name))
                    } else {
                        arguments.iter().all(|argument| match argument {
                            ast::GenericArg::TypeArg(argument) => argument
                                .ty()
                                .is_some_and(|ty| field_supports_derive(&ty, trait_name)),
                            ast::GenericArg::LifetimeArg(_) => true,
                            _ => false,
                        })
                    }
                }
                _ => false,
            }
        }
        _ => false,
    }
}

// ---------------------------------------------------------------------------
// Literals

/// Decimal digits of an integer literal without underscores, matching
/// `syn::LitInt::base10_digits`.
fn base10_digits(number: &ast::IntNumber) -> String {
    let (_, digits, _) = number.split_into_parts();
    let digits = digits.replace('_', "");
    let radix = number.radix() as u32;
    if radix == 10 {
        let trimmed = digits.trim_start_matches('0');
        return if trimmed.is_empty() {
            "0".to_owned()
        } else {
            trimmed.to_owned()
        };
    }
    // Base 1e9 limbs, least significant first.
    let mut limbs: Vec<u64> = vec![0];
    for ch in digits.chars() {
        let Some(digit) = ch.to_digit(radix) else {
            break;
        };
        let mut carry = u64::from(digit);
        for limb in &mut limbs {
            let value = *limb * u64::from(radix) + carry;
            *limb = value % 1_000_000_000;
            carry = value / 1_000_000_000;
        }
        while carry > 0 {
            limbs.push(carry % 1_000_000_000);
            carry /= 1_000_000_000;
        }
    }
    let mut text = limbs.last().copied().unwrap_or(0).to_string();
    for limb in limbs.iter().rev().skip(1) {
        text.push_str(&format!("{limb:09}"));
    }
    text
}

fn literal_kind(literal: &ast::Literal) -> Option<LiteralKind> {
    match literal.kind() {
        // Like Syn, `1f32` stays an integer literal with an `f32` suffix.
        ast::LiteralKind::IntNumber(number) => Some(LiteralKind::Integer {
            text: base10_digits(&number),
            suffix: number.suffix().unwrap_or_default().to_owned(),
        }),
        // rust-analyzer lexes `1f64` as a float token; Syn keeps digits with a
        // float suffix as an integer literal.
        ast::LiteralKind::FloatNumber(number)
            if number
                .split_into_parts()
                .0
                .chars()
                .all(|ch| ch.is_ascii_digit() || ch == '_') =>
        {
            let digits = number.value_string();
            let trimmed = digits.trim_start_matches('0');
            Some(LiteralKind::Integer {
                text: if trimmed.is_empty() { "0" } else { trimmed }.to_owned(),
                suffix: number.suffix().unwrap_or_default().to_owned(),
            })
        }
        // Syn's `base10_digits` lowercases the exponent and drops a `+` sign.
        ast::LiteralKind::FloatNumber(number) => Some(LiteralKind::Float {
            text: number
                .value_string()
                .replace('E', "e")
                .replace("e+", "e"),
            suffix: number.suffix().unwrap_or_default().to_owned(),
        }),
        ast::LiteralKind::Bool(value) => Some(LiteralKind::Boolean(value)),
        ast::LiteralKind::Char(value) => value.value().ok().map(LiteralKind::Character),
        ast::LiteralKind::String(value) => value.value().ok().map(|value| LiteralKind::String {
            value: value.into_owned(),
        }),
        ast::LiteralKind::Byte(_)
        | ast::LiteralKind::ByteString(_)
        | ast::LiteralKind::CString(_) => None,
    }
}

fn parse_i32_discriminant(expression: &ast::Expr) -> Option<i32> {
    match expression {
        ast::Expr::Literal(literal) => match literal.kind() {
            ast::LiteralKind::IntNumber(number) => base10_digits(&number).parse().ok(),
            _ => None,
        },
        ast::Expr::PrefixExpr(prefix) if prefix.op_kind() == Some(ast::UnaryOp::Neg) => {
            let ast::Expr::Literal(literal) = prefix.expr()? else {
                return None;
            };
            let ast::LiteralKind::IntNumber(number) = literal.kind() else {
                return None;
            };
            base10_digits(&number).parse::<i32>().ok()?.checked_neg()
        }
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Local macro_rules support

fn supported_macro_rule(rules: &ast::MacroRules) -> Option<LocalMacroRule> {
    let tokens = tt::owned_children(&rules.token_tree()?, &SpanMap::default(), "");
    let [pattern, eq, gt, body, rest @ ..] = tokens.as_slice() else {
        return None;
    };
    let pattern = pattern.group().filter(|group| group.delimiter == '(')?;
    if !eq.is_punct('=') || !gt.is_punct('>') {
        return None;
    }
    let body = body.group().filter(|group| group.delimiter == '{')?;
    match rest {
        [] => {}
        [semicolon] if semicolon.is_punct(';') => {}
        _ => return None,
    }
    if pattern.children.is_empty() {
        return match body.children.as_slice() {
            [OwnedTt::Leaf(leaf)] if tt::is_literal_kind(leaf.kind) => {
                Some(LocalMacroRule::ZeroArgLiteral(body.children.clone()))
            }
            _ => None,
        };
    }
    let [
        capture_dollar,
        capture_group,
        separator,
        repetition,
        optional_tail @ ..,
    ] = pattern.children.as_slice()
    else {
        return None;
    };
    if !capture_dollar.is_punct('$') {
        return None;
    }
    let separator = match separator {
        OwnedTt::Leaf(tt::Leaf {
            punct: Some((ch @ (',' | ';'), _)),
            ..
        }) => *ch,
        _ => return None,
    };
    let allow_empty = if repetition.is_punct('+') {
        false
    } else if repetition.is_punct('*') {
        true
    } else {
        return None;
    };
    let allow_trailing_separator = match optional_tail {
        [] => false,
        [dollar, group, question] if dollar.is_punct('$') && question.is_punct('?') => {
            let group = group.group()?;
            if group.children.len() == 1 && group.children[0].is_punct(separator) {
                true
            } else {
                return None;
            }
        }
        _ => return None,
    };
    let capture_group = capture_group.group()?;
    let [dollar, ident, colon, kind] = capture_group.children.as_slice() else {
        return None;
    };
    if !dollar.is_punct('$') || !colon.is_punct(':') || kind.ident() != Some("expr") {
        return None;
    }
    Some(LocalMacroRule::RepeatSeparatedExpr {
        capture: ident.ident()?.to_owned(),
        separator,
        allow_empty,
        allow_trailing_separator,
        transcriber: body.children.clone(),
    })
}

fn expand_macro_transcriber(
    transcriber: &[OwnedTt],
    capture: &str,
    values: &[Vec<OwnedTt>],
) -> Vec<OwnedTt> {
    let mut output = Vec::new();
    let mut index = 0;
    while index < transcriber.len() {
        if transcriber[index].is_punct('$')
            && let Some(OwnedTt::Group(repetition)) = transcriber.get(index + 1)
            && let Some(operator) = transcriber.get(index + 2)
            && (operator.is_punct('+') || operator.is_punct('*'))
        {
            for value in values {
                output.extend(substitute_macro_capture(&repetition.children, capture, value));
            }
            index += 3;
            continue;
        }
        if let OwnedTt::Group(group) = &transcriber[index] {
            let mut nested = group.clone();
            nested.children = expand_macro_transcriber(&group.children, capture, values);
            output.push(OwnedTt::Group(nested));
        } else {
            output.push(transcriber[index].clone());
        }
        index += 1;
    }
    output
}

fn substitute_macro_capture(body: &[OwnedTt], capture: &str, value: &[OwnedTt]) -> Vec<OwnedTt> {
    let mut output = Vec::new();
    let mut index = 0;
    while index < body.len() {
        if body[index].is_punct('$') && body.get(index + 1).and_then(OwnedTt::ident) == Some(capture)
        {
            output.extend(value.iter().cloned());
            index += 2;
        } else {
            output.push(body[index].clone());
            index += 1;
        }
    }
    output
}

const WRAPPER_PREFIX: &str = "fn __quill_wrapper() { __quill_wrapper(";
const WRAPPER_SUFFIX: &str = ") }";

/// Parse `text` as a call argument list. Returns the arguments when the text
/// is a well-formed comma-separated expression list.
fn parse_argument_list(text: &str, edition: Edition) -> Option<Vec<ast::Expr>> {
    let wrapped = format!("{WRAPPER_PREFIX}{text}{WRAPPER_SUFFIX}");
    let parse = ra_ap_syntax::SourceFile::parse(&wrapped, edition);
    if !parse.errors().is_empty() {
        return None;
    }
    let call = parse
        .tree()
        .syntax()
        .descendants()
        .find_map(ast::CallExpr::cast)?;
    Some(call.arg_list()?.args().collect())
}

fn parse_single_expression(text: &str, edition: Edition) -> Option<ast::Expr> {
    let mut arguments = parse_argument_list(text, edition)?;
    (arguments.len() == 1).then(|| arguments.remove(0))
}

// ---------------------------------------------------------------------------
// Body lowering

struct Lowerer<'a> {
    source: &'a SourceFile,
    edition: Edition,
    map: SpanMap,
    definitions: &'a [LocalMacroExpansion],
    scopes: &'a [Vec<String>],
    item_scope: &'a [String],
    depth: usize,
}

enum StatementInput {
    Let(ast::LetStmt),
    Expression(ast::Expr, bool, SyntaxNode),
    Item,
}

impl<'a> Lowerer<'a> {
    fn resolve_self_path(&self, path: String) -> String {
        let Some(owner) = self
            .item_scope
            .iter()
            .rev()
            .find_map(|scope| scope.strip_prefix("impl:"))
        else {
            return path;
        };
        if path == "Self" {
            owner.to_owned()
        } else if let Some(suffix) = path.strip_prefix("Self::") {
            format!("{owner}::{suffix}")
        } else {
            path
        }
    }

    fn with_map(&self, map: SpanMap) -> Lowerer<'a> {
        Lowerer {
            source: self.source,
            edition: self.edition,
            map,
            definitions: self.definitions,
            scopes: self.scopes,
            item_scope: self.item_scope,
            depth: self.depth + 1,
        }
    }

    fn span(&self, node: &SyntaxNode) -> Range<usize> {
        self.map.node(node)
    }

    fn slice(&self, node: &SyntaxNode) -> String {
        let span = self.span(node);
        self.source
            .text()
            .get(span)
            .map(str::to_owned)
            .unwrap_or_else(|| node.text().to_string())
    }

    /// Lower a literal using its original source text, so CRLF line endings
    /// inside string literals read as LF like rustc (see `parser_text`).
    fn literal(&self, literal: &ast::Literal) -> Option<LiteralKind> {
        let original = self.source.text().get(self.span(literal.syntax()));
        match original {
            Some(original) if original.contains('\r') => {
                let normalized = original.replace("\r\n", "\n");
                let parse = ra_ap_syntax::SourceFile::parse(
                    &format!("const _: () = {normalized};"),
                    self.edition,
                );
                let reparsed = parse
                    .tree()
                    .syntax()
                    .descendants()
                    .find_map(ast::Literal::cast)?;
                literal_kind(&reparsed)
            }
            _ => literal_kind(literal),
        }
    }

    fn unsupported(&self, node: &SyntaxNode) -> BodyExpr {
        BodyExpr {
            kind: BodyExprKind::Unsupported,
            span: self.span(node),
        }
    }

    fn expansions_enabled(&self) -> bool {
        !self.definitions.is_empty() && !self.scopes.is_empty()
    }

    /// Expand an invocation of a supported local `macro_rules!` macro.
    fn expand(&self, call: &ast::MacroCall) -> Option<(ast::Expr, SpanMap)> {
        if !self.expansions_enabled() || self.depth >= MAX_EXPANSION_DEPTH {
            return None;
        }
        let path = path_parts(&call.path()?);
        if path.qself || path.segments.len() != 1 || !path.segments[0].plain() {
            return None;
        }
        let name = unraw(&path.segments[0].name);
        let definition = self.definitions.iter().rev().find(|definition| {
            definition.name == name && self.scopes.contains(&definition.scope)
        })?;
        let arguments = tt::owned_children(&call.token_tree()?, &self.map, self.source.text());
        let tokens = match &definition.rule {
            LocalMacroRule::ZeroArgLiteral(tokens) if arguments.is_empty() => tokens.clone(),
            LocalMacroRule::ZeroArgLiteral(_) => return None,
            LocalMacroRule::RepeatSeparatedExpr {
                capture,
                separator,
                allow_empty,
                allow_trailing_separator,
                transcriber,
            } => {
                let mut groups = Vec::<Vec<OwnedTt>>::new();
                let mut current = Vec::new();
                for token in arguments {
                    if token.is_punct(*separator) {
                        if current.is_empty() {
                            return None;
                        }
                        parse_single_expression(&tt::render(&current).0, self.edition)?;
                        groups.push(std::mem::take(&mut current));
                    } else {
                        current.push(token);
                    }
                }
                if !current.is_empty() {
                    parse_single_expression(&tt::render(&current).0, self.edition)?;
                    groups.push(current);
                } else if !groups.is_empty() && !allow_trailing_separator {
                    return None;
                }
                if groups.is_empty() && !allow_empty {
                    return None;
                }
                expand_macro_transcriber(transcriber, capture, &groups)
            }
        };
        let (text, mapping) = tt::render(&tokens);
        let expression = parse_single_expression(&text, self.edition)?;
        Some((
            expression,
            SpanMap::expansion(-(WRAPPER_PREFIX.len() as isize), mapping),
        ))
    }

    fn collect_calls(&self, node: &SyntaxNode, calls: &mut Vec<String>, qualified: &mut Vec<String>) {
        for child in node.children() {
            match child.kind() {
                SyntaxKind::TOKEN_TREE => continue,
                SyntaxKind::CALL_EXPR => {
                    if let Some(ast::Expr::PathExpr(callee)) =
                        ast::CallExpr::cast(child.clone()).and_then(|call| call.expr())
                        && let Some(path) = callee.path()
                    {
                        let path = path_parts(&path);
                        if !path.qself && !path.leading_colon && path.all_plain() {
                            if path.segments.len() == 1 {
                                calls.push(unraw(&path.segments[0].name));
                            } else {
                                qualified.push(path.joined());
                            }
                        }
                    }
                }
                SyntaxKind::MACRO_EXPR if self.expansions_enabled() => {
                    if !is_statement_macro(&child)
                        && let Some(call) =
                            ast::MacroExpr::cast(child.clone()).and_then(|expr| expr.macro_call())
                        && let Some((expansion, map)) = self.expand(&call)
                    {
                        self.with_map(map)
                            .collect_calls(expansion.syntax(), calls, qualified);
                        continue;
                    }
                }
                _ => {}
            }
            self.collect_calls(&child, calls, qualified);
        }
    }

    fn lower_block(&self, block: &ast::BlockExpr) -> BodyExpr {
        BodyExpr {
            kind: self.lower_block_kind(block),
            span: self.span(block.syntax()),
        }
    }

    fn lower_block_kind(&self, block: &ast::BlockExpr) -> BodyExprKind {
        let mut inputs = Vec::new();
        if let Some(list) = block.stmt_list() {
            for statement in list.statements() {
                inputs.push(match statement {
                    ast::Stmt::LetStmt(local) => StatementInput::Let(local),
                    ast::Stmt::ExprStmt(statement) => match statement.expr() {
                        Some(expression) => StatementInput::Expression(
                            expression,
                            statement.semicolon_token().is_some(),
                            statement.syntax().clone(),
                        ),
                        None => continue,
                    },
                    ast::Stmt::Item(_) => StatementInput::Item,
                });
            }
            if let Some(tail) = list.tail_expr() {
                let node = tail.syntax().clone();
                inputs.push(StatementInput::Expression(tail, false, node));
            }
        }
        let count = inputs.len();
        let mut statements = Vec::with_capacity(count);
        let mut tail = None;
        for (index, input) in inputs.into_iter().enumerate() {
            match input {
                StatementInput::Let(local) => statements.push(self.lower_let(&local)),
                StatementInput::Item => statements.push(BodyStatement::Unsupported),
                StatementInput::Expression(expression, has_semicolon, statement) => {
                    if let ast::Expr::MacroExpr(macro_expression) = &expression
                        && (has_semicolon || is_statement_macro(macro_expression.syntax()))
                        && let Some(call) = macro_expression.macro_call()
                    {
                        let lowered = self.lower_macro(&call);
                        let span = self.span(&statement);
                        let (line, column) = self.source.line_column(span.start);
                        statements.push(BodyStatement::MacroInvocation {
                            path: lowered.path,
                            tokens: lowered.tokens,
                            format_string: lowered.format_string,
                            arguments: lowered.arguments,
                            span,
                            source_path: self.source.path().to_string_lossy().into_owned(),
                            line,
                            column,
                        });
                        continue;
                    }
                    let is_last = index + 1 == count && !has_semicolon;
                    if let Some(statement) = self.lower_expression_statement(&expression, has_semicolon)
                    {
                        statements.push(statement);
                        continue;
                    }
                    let lowered = self.lower_expr(&expression);
                    let is_statement_if = matches!(&expression,
                        ast::Expr::IfExpr(branch) if branch.else_branch().is_none());
                    if is_last && !is_statement_if {
                        tail = Some(Box::new(lowered));
                    } else {
                        statements.push(BodyStatement::Expression {
                            expression: lowered,
                            has_semicolon,
                        });
                    }
                }
            }
        }
        BodyExprKind::Block { statements, tail }
    }

    fn lower_let(&self, local: &ast::LetStmt) -> BodyStatement {
        let span = self.span(local.syntax());
        if local.ty().is_none()
            && let (Some(ast::Pat::TuplePat(pattern)), Some(initializer)) =
                (local.pat(), local.initializer())
        {
            let names = pattern
                .fields()
                .map(|element| match element {
                    ast::Pat::IdentPat(binding)
                        if binding.mut_token().is_none()
                            && binding.ref_token().is_none()
                            && binding.at_token().is_none() =>
                    {
                        Some(unraw(&name_text(binding.name())))
                    }
                    _ => None,
                })
                .collect::<Option<Vec<_>>>();
            if let Some(names) = names {
                return BodyStatement::LetTuple {
                    names,
                    initializer: self.lower_expr(&initializer),
                    span,
                };
            }
        }
        let (name, type_text, mutable) = match (local.pat(), local.ty()) {
            (Some(ast::Pat::IdentPat(pattern)), None) => (
                Some(unraw(&name_text(pattern.name()))),
                None,
                pattern.mut_token().is_some(),
            ),
            (pattern, Some(ty)) => {
                let (name, mutable) = match pattern {
                    Some(ast::Pat::IdentPat(pattern)) => (
                        Some(unraw(&name_text(pattern.name()))),
                        pattern.mut_token().is_some(),
                    ),
                    _ => (None, false),
                };
                (name, Some(self.slice(ty.syntax())), mutable)
            }
            _ => (None, None, false),
        };
        BodyStatement::Let {
            name,
            type_text,
            mutable,
            initializer: local.initializer().map(|init| self.lower_expr(&init)),
            span,
        }
    }

    /// Statement forms that Syn's lowering recognized before falling back
    /// to an expression statement or tail expression.
    fn lower_expression_statement(
        &self,
        expression: &ast::Expr,
        has_semicolon: bool,
    ) -> Option<BodyStatement> {
        let span = self.span(expression.syntax());
        Some(match expression {
            ast::Expr::BreakExpr(expr) if expr.expr().is_none() => {
                BodyStatement::Break {
                    label: expr
                        .lifetime()
                        .map(|lifetime| lifetime.syntax().text().to_string()),
                    span,
                }
            }
            ast::Expr::ContinueExpr(expr) => {
                BodyStatement::Continue {
                    label: expr
                        .lifetime()
                        .map(|lifetime| lifetime.syntax().text().to_string()),
                    span,
                }
            }
            ast::Expr::BreakExpr(_) => BodyStatement::Unsupported,
            ast::Expr::BinExpr(binary)
                if binary.op_kind() == Some(ast::BinaryOp::Assignment { op: None }) =>
            {
                let target = match binary.lhs() {
                    Some(ast::Expr::PathExpr(path)) => path
                        .path()
                        .map(|path| path_parts(&path))
                        .filter(|path| !path.qself && path.segments.len() == 1)
                        .map(|path| unraw(&path.segments[0].name)),
                    Some(ast::Expr::FieldExpr(field)) => {
                        Some(self.slice(field.syntax()))
                    }
                    Some(ast::Expr::IndexExpr(index)) => Some(self.slice(index.syntax())),
                    Some(ast::Expr::PrefixExpr(prefix))
                        if prefix.op_kind() == Some(ast::UnaryOp::Deref) =>
                    {
                        Some(self.slice(prefix.syntax()))
                    }
                    _ => None,
                };
                match (target, binary.rhs()) {
                    (Some(name), Some(value)) => BodyStatement::Assign {
                        name,
                        value: self.lower_expr(&value),
                        span,
                    },
                    _ => BodyStatement::Unsupported,
                }
            }
            ast::Expr::WhileExpr(while_expr) => BodyStatement::While {
                label: while_expr
                    .label()
                    .and_then(|label| label.lifetime())
                    .map(|lifetime| lifetime.syntax().text().to_string()),
                condition: self.lower_opt_expr(while_expr.condition(), while_expr.syntax()),
                body: self.lower_opt_block(while_expr.loop_body(), while_expr.syntax()),
                span,
            },
            ast::Expr::LoopExpr(loop_expr) => BodyStatement::Loop {
                label: loop_expr
                    .label()
                    .and_then(|label| label.lifetime())
                    .map(|lifetime| lifetime.syntax().text().to_string()),
                body: self.lower_opt_block(loop_expr.loop_body(), loop_expr.syntax()),
                span,
            },
            ast::Expr::ReturnExpr(_) => BodyStatement::Expression {
                expression: self.lower_expr(expression),
                has_semicolon,
            },
            ast::Expr::ForExpr(for_expr) => self.lower_for(for_expr, span),
            _ => return None,
        })
    }

    fn lower_for(&self, for_expr: &ast::ForExpr, span: Range<usize>) -> BodyStatement {
        let pattern = for_expr.pat();
        let body = self.lower_opt_block(for_expr.loop_body(), for_expr.syntax());
        let simple_name = |pattern: &ast::Pat, allow_ref: bool| match pattern {
            ast::Pat::IdentPat(binding)
                if binding.at_token().is_none() && (allow_ref || binding.ref_token().is_none()) =>
            {
                Some(unraw(&name_text(binding.name())))
            }
            _ => None,
        };
        if let (Some(name), Some(ast::Expr::RangeExpr(range))) = (
            pattern.as_ref().and_then(|pattern| simple_name(pattern, true)),
            for_expr.iterable(),
        ) && let (Some(start), Some(end)) = (range.start(), range.end())
        {
            return BodyStatement::ForRange {
                label: for_expr
                    .label()
                    .and_then(|label| label.lifetime())
                    .map(|lifetime| lifetime.syntax().text().to_string()),
                name,
                start: self.lower_expr(&start),
                end: self.lower_expr(&end),
                inclusive: range.op_kind() == Some(ast::RangeOp::Inclusive),
                body,
                span,
            };
        }
        let names = match &pattern {
            Some(ast::Pat::TuplePat(tuple)) => tuple
                .fields()
                .map(|element| simple_name(&element, false))
                .collect(),
            Some(pattern) => simple_name(pattern, false).map(|name| vec![name]),
            None => None,
        };
        match (names, for_expr.iterable()) {
            (Some(names), Some(iterator)) => BodyStatement::ForEach {
                label: for_expr
                    .label()
                    .and_then(|label| label.lifetime())
                    .map(|lifetime| lifetime.syntax().text().to_string()),
                names,
                iterator: self.lower_expr(&iterator),
                body,
                span,
            },
            _ => BodyStatement::Unsupported,
        }
    }

    fn lower_opt_expr(&self, expression: Option<ast::Expr>, parent: &SyntaxNode) -> BodyExpr {
        expression.map_or_else(|| self.unsupported(parent), |expr| self.lower_expr(&expr))
    }

    fn lower_opt_block(&self, block: Option<ast::BlockExpr>, parent: &SyntaxNode) -> BodyExpr {
        block.map_or_else(|| self.unsupported(parent), |block| self.lower_block(&block))
    }

    fn lower_expr(&self, expression: &ast::Expr) -> BodyExpr {
        if let ast::Expr::MacroExpr(macro_expression) = expression
            && let Some(call) = macro_expression.macro_call()
            && let Some((expansion, map)) = self.expand(&call)
        {
            return self.with_map(map).lower_expr(&expansion);
        }
        let span = self.span(expression.syntax());
        let kind = self.lower_expr_kind(expression).unwrap_or(BodyExprKind::Unsupported);
        BodyExpr { kind, span }
    }

    fn lower_exprs(&self, expressions: impl Iterator<Item = ast::Expr>) -> Vec<BodyExpr> {
        expressions.map(|expr| self.lower_expr(&expr)).collect()
    }

    fn lower_arguments(&self, list: Option<ast::ArgList>) -> Vec<BodyExpr> {
        list.map(|list| self.lower_exprs(list.args()))
            .unwrap_or_default()
    }

    fn lower_expr_kind(&self, expression: &ast::Expr) -> Option<BodyExprKind> {
        Some(match expression {
            ast::Expr::Literal(literal) => BodyExprKind::Literal(self.literal(literal)?),
            ast::Expr::PathExpr(path) => {
                let path = path_parts(&path.path()?);
                if path.qself {
                    return None;
                }
                BodyExprKind::Path(path.joined())
            }
            ast::Expr::CallExpr(call) => {
                let callee_path = match call.expr() {
                    Some(ast::Expr::PathExpr(path)) => path.path().map(|path| path_parts(&path)),
                    _ => None,
                }
                .filter(|path| !path.qself);
                let callee = callee_path
                    .as_ref()
                    .filter(|path| {
                        let last = path.segments.len().saturating_sub(1);
                        path.segments.iter().enumerate().all(|(index, segment)| {
                            segment.plain()
                                || (index == last
                                    && matches!(
                                        segment.arguments,
                                        SegmentArguments::AngleBracketed(_)
                                    ))
                        })
                    })
                    .map(PathParts::joined)
                    .unwrap_or_default();
                let generic_arguments = callee_path
                    .as_ref()
                    .and_then(|path| path.segments.last())
                    .and_then(|segment| match &segment.arguments {
                        SegmentArguments::AngleBracketed(arguments) => Some(
                            arguments
                                .generic_args()
                                .map(|argument| match argument {
                                    ast::GenericArg::TypeArg(argument) => argument
                                        .ty()
                                        .as_ref()
                                        .and_then(type_path)
                                        .filter(|path| !path.qself && path.all_plain())
                                        .map(|path| path.joined())
                                        .unwrap_or_default(),
                                    _ => String::new(),
                                })
                                .collect(),
                        ),
                        _ => None,
                    })
                    .unwrap_or_default();
                BodyExprKind::Call {
                    callee,
                    generic_arguments,
                    arguments: self.lower_arguments(call.arg_list()),
                }
            }
            ast::Expr::MethodCallExpr(call) => BodyExprKind::MethodCall {
                receiver: Box::new(self.lower_opt_expr(call.receiver(), call.syntax())),
                method: call
                    .name_ref()
                    .map(|name| name.syntax().text().to_string())
                    .unwrap_or_default(),
                arguments: self.lower_arguments(call.arg_list()),
            },
            ast::Expr::ClosureExpr(closure) => {
                let parameters = closure
                    .param_list()?
                    .params()
                    .map(|parameter| {
                        if parameter.ty().is_some() {
                            return None;
                        }
                        let simple = |pattern: &ast::Pat| match pattern {
                            ast::Pat::IdentPat(binding)
                                if binding.ref_token().is_none() && binding.at_token().is_none() =>
                            {
                                Some(unraw(&name_text(binding.name())))
                            }
                            _ => None,
                        };
                        match parameter.pat()? {
                            ast::Pat::TuplePat(tuple) => tuple
                                .fields()
                                .map(|element| simple(&element))
                                .collect::<Option<Vec<_>>>()
                                .map(ClosureParameter::Tuple),
                            pattern => simple(&pattern).map(ClosureParameter::Identifier),
                        }
                    })
                    .collect::<Option<Vec<_>>>()?;
                BodyExprKind::Closure {
                    parameters,
                    body: Box::new(self.lower_opt_expr(closure.body(), closure.syntax())),
                    move_capture: closure.move_token().is_some(),
                }
            }
            ast::Expr::RecordExpr(structure) => {
                let list = structure.record_expr_field_list()?;
                if list.spread().is_some() {
                    return None;
                }
                let path = path_parts(&structure.path()?);
                if path.qself {
                    return None;
                }
                let fields = list
                    .fields()
                    .filter_map(|field| match (field.name_ref(), field.expr()) {
                        (Some(name), _)
                            if name.syntax().first_token().is_some_and(|token| {
                                token.kind() == SyntaxKind::INT_NUMBER
                            }) =>
                        {
                            None
                        }
                        (Some(name), Some(value)) => {
                            Some((name.syntax().text().to_string(), self.lower_expr(&value)))
                        }
                        (None, Some(value @ ast::Expr::PathExpr(_))) => {
                            let ast::Expr::PathExpr(shorthand) = &value else {
                                unreachable!()
                            };
                            let name = shorthand
                                .path()
                                .map(|path| path_parts(&path))
                                .and_then(|path| path.segments.first().map(|s| s.name.clone()))
                                .unwrap_or_default();
                            Some((name, self.lower_expr(&value)))
                        }
                        _ => None,
                    })
                    .collect();
                BodyExprKind::StructLiteral {
                    path: self.resolve_self_path(path.joined()),
                    fields,
                }
            }
            ast::Expr::FieldExpr(field) => BodyExprKind::FieldAccess {
                base: Box::new(self.lower_opt_expr(field.expr(), field.syntax())),
                field: field.name_ref()?.syntax().text().to_string(),
            },
            ast::Expr::IndexExpr(index) => BodyExprKind::Index {
                base: Box::new(self.lower_opt_expr(index.base(), index.syntax())),
                index: Box::new(self.lower_opt_expr(index.index(), index.syntax())),
            },
            ast::Expr::ArrayExpr(array) => match array.kind() {
                ast::ArrayExprKind::ElementList(elements) => {
                    BodyExprKind::FixedArrayLiteral(self.lower_exprs(elements))
                }
                ast::ArrayExprKind::Repeat { initializer, repeat } => {
                    let element = Box::new(self.lower_opt_expr(initializer, array.syntax()));
                    let length = repeat.and_then(|repeat| {
                        self.slice(repeat.syntax())
                            .replace('_', "")
                            .parse::<usize>()
                            .ok()
                    });
                    BodyExprKind::ArrayRepeat { element, length }
                }
            },
            ast::Expr::BinExpr(binary) => {
                let operator = match binary.op_kind()? {
                    ast::BinaryOp::Assignment { op: None } => return None,
                    operator => lower_binary_operator(operator),
                };
                BodyExprKind::Binary {
                    operator,
                    left: Box::new(self.lower_opt_expr(binary.lhs(), binary.syntax())),
                    right: Box::new(self.lower_opt_expr(binary.rhs(), binary.syntax())),
                }
            }
            ast::Expr::PrefixExpr(prefix) => {
                let operand = Box::new(self.lower_opt_expr(prefix.expr(), prefix.syntax()));
                match prefix.op_kind()? {
                    ast::UnaryOp::Neg => BodyExprKind::Negate(operand),
                    ast::UnaryOp::Not => BodyExprKind::Not(operand),
                    ast::UnaryOp::Deref => BodyExprKind::Dereference(operand),
                }
            }
            ast::Expr::TryExpr(try_expression) => BodyExprKind::Try(Box::new(
                self.lower_opt_expr(try_expression.expr(), try_expression.syntax()),
            )),
            ast::Expr::BlockExpr(block) => {
                // Plain and labeled blocks lower like Syn's `Expr::Block`;
                // `unsafe`, `async`, `const`, `try` and `gen` blocks do not.
                if !matches!(block.modifier(), None | Some(ast::BlockModifier::Label(_)))
                    || block.const_token().is_some()
                {
                    return None;
                }
                self.lower_block_kind(block)
            }
            ast::Expr::ParenExpr(paren) => BodyExprKind::Parenthesized(Box::new(
                self.lower_opt_expr(paren.expr(), paren.syntax()),
            )),
            ast::Expr::IfExpr(branch) => {
                let condition = match branch.condition() {
                    Some(ast::Expr::LetExpr(let_expression)) => {
                        let fallback = || self.unsupported(let_expression.syntax());
                        match (let_expression.pat(), let_expression.expr()) {
                            (Some(pattern), Some(scrutinee))
                                if repeatable_if_let_scrutinee(&scrutinee) =>
                            {
                                let scrutinee = self.lower_expr(&scrutinee);
                                self.lower_if_let_pattern_condition(&pattern, &scrutinee)
                                    .unwrap_or_else(fallback)
                            }
                            _ => fallback(),
                        }
                    }
                    condition => self.lower_opt_expr(condition, branch.syntax()),
                };
                BodyExprKind::If {
                    condition: Box::new(condition),
                    then_branch: Box::new(
                        self.lower_opt_block(branch.then_branch(), branch.syntax()),
                    ),
                    else_branch: branch.else_branch().map(|otherwise| {
                        Box::new(match otherwise {
                            ast::ElseBranch::Block(block) => self.lower_block(&block),
                            ast::ElseBranch::IfExpr(branch) => {
                                self.lower_expr(&ast::Expr::IfExpr(branch))
                            }
                        })
                    }),
                }
            }
            ast::Expr::MatchExpr(branch) => BodyExprKind::Match {
                scrutinee: Box::new(self.lower_opt_expr(branch.expr(), branch.syntax())),
                arms: branch
                    .match_arm_list()?
                    .arms()
                    .map(|arm| MatchArm {
                        pattern: if arm.guard().is_some() {
                            MatchPattern::Unsupported
                        } else {
                            arm.pat()
                                .map_or(MatchPattern::Unsupported, |pattern| {
                                    lower_match_pattern(&pattern)
                                })
                        },
                        body: self.lower_opt_expr(arm.expr(), arm.syntax()),
                        span: self.span(arm.syntax()),
                    })
                    .collect(),
            },
            ast::Expr::ReturnExpr(returned) => BodyExprKind::Return(
                returned
                    .expr()
                    .map(|value| Box::new(self.lower_expr(&value))),
            ),
            ast::Expr::MacroExpr(macro_expression) => {
                BodyExprKind::MacroInvocation(self.lower_macro(&macro_expression.macro_call()?))
            }
            ast::Expr::RefExpr(reference) => {
                if reference.raw_token().is_some() {
                    return None;
                }
                match reference.expr() {
                    Some(ast::Expr::ArrayExpr(array)) => match array.kind() {
                        ast::ArrayExprKind::ElementList(elements) => {
                            let elements = self.lower_exprs(elements);
                            if elements.is_empty() {
                                BodyExprKind::EmptySliceReference
                            } else {
                                BodyExprKind::ArrayLiteral(elements)
                            }
                        }
                        ast::ArrayExprKind::Repeat { initializer, repeat } => {
                            let element = Box::new(
                                self.lower_opt_expr(initializer, array.syntax()),
                            );
                            let length = repeat.and_then(|repeat| {
                                self.slice(repeat.syntax())
                                    .replace('_', "")
                                    .parse::<usize>()
                                    .ok()
                            });
                            BodyExprKind::Reference {
                                mutable: reference.mut_token().is_some(),
                                expression: Box::new(BodyExpr {
                                    kind: BodyExprKind::ArrayRepeat { element, length },
                                    span: self.span(array.syntax()),
                                }),
                            }
                        }
                    },
                    expression => BodyExprKind::Reference {
                        mutable: reference.mut_token().is_some(),
                        expression: Box::new(self.lower_opt_expr(expression, reference.syntax())),
                    },
                }
            }
            ast::Expr::CastExpr(cast) => BodyExprKind::Cast {
                expression: Box::new(self.lower_opt_expr(cast.expr(), cast.syntax())),
                type_name: cast.ty().map(|ty| self.slice(ty.syntax()))?,
            },
            ast::Expr::TupleExpr(tuple) => {
                let elements = self.lower_exprs(tuple.fields());
                if elements.is_empty() {
                    BodyExprKind::Unit
                } else {
                    BodyExprKind::Tuple(elements)
                }
            }
            _ => return None,
        })
    }

    fn lower_if_let_pattern_condition(
        &self,
        pattern: &ast::Pat,
        scrutinee: &BodyExpr,
    ) -> Option<BodyExpr> {
        let span = self.span(pattern.syntax());
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
                let ast::Pat::OrPat(patterns) = pattern else {
                    return None;
                };
                let mut conditions = patterns
                    .pats()
                    .map(|pattern| self.lower_if_let_pattern_condition(&pattern, scrutinee));
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
                let compare = |operator, literal| BodyExpr {
                    kind: BodyExprKind::Binary {
                        operator,
                        left: Box::new(scrutinee.clone()),
                        right: Box::new(BodyExpr {
                            kind: BodyExprKind::Literal(literal),
                            span: span.clone(),
                        }),
                    },
                    span: span.clone(),
                };
                let lower = compare(BinaryOperator::GreaterEqual, start);
                let upper = compare(
                    if inclusive {
                        BinaryOperator::LessEqual
                    } else {
                        BinaryOperator::Less
                    },
                    end,
                );
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

    fn lower_macro(&self, call: &ast::MacroCall) -> MacroExpression {
        let path = call
            .path()
            .map(|path| path_parts(&path).joined())
            .unwrap_or_default();
        let Some(token_tree) = call.token_tree() else {
            return MacroExpression {
                path,
                tokens: String::new(),
                format_string: None,
                arguments: Vec::new(),
                const_expansion: None,
            };
        };
        let tokens = tt::display(&tt::owned_children(&token_tree, &self.map, self.source.text()));
        let (_, inner) = tt::delimiter_and_inner(&token_tree);
        let tree_text = token_tree.syntax().text().to_string();
        let tree_start = usize::from(token_tree.syntax().text_range().start());
        let inner_text = &tree_text[inner.start - tree_start..inner.end - tree_start];
        let parsed_arguments = parse_argument_list(inner_text, self.edition);
        // Macro inputs are opaque to local `macro_rules!` expansion, as they
        // were for the Syn collector: arguments are lowered unexpanded.
        let arguments_lowerer = Lowerer {
            map: self
                .map
                .shifted(inner.start as isize - WRAPPER_PREFIX.len() as isize),
            definitions: &[],
            scopes: &[],
            ..*self
        };
        let format_string = parsed_arguments.as_ref().and_then(|arguments| {
            match arguments.first()? {
                ast::Expr::Literal(literal) => match arguments_lowerer.literal(literal)? {
                    LiteralKind::String { value } => Some(value),
                    _ => None,
                },
                _ => None,
            }
        });
        let arguments = parsed_arguments
            .as_ref()
            .filter(|_| path != "stringify")
            .map(|arguments| {
                arguments
                    .iter()
                    .skip(1)
                    .map(|argument| arguments_lowerer.lower_expr(argument))
                    .collect()
            })
            .unwrap_or_default();
        let stringified = || {
            let tokens: Vec<_> = token_tree
                .syntax()
                .descendants_with_tokens()
                .filter_map(|element| element.into_token())
                .filter(|token| {
                    !token.kind().is_trivia()
                        && token.text_range().start() >= ra_ap_syntax::TextSize::from(inner.start as u32)
                        && token.text_range().end() <= ra_ap_syntax::TextSize::from(inner.end as u32)
                })
                .collect();
            let range = match (tokens.first(), tokens.last()) {
                (Some(first), Some(last)) => {
                    usize::from(first.text_range().start()) - tree_start
                        ..usize::from(last.text_range().end()) - tree_start
                }
                _ => 0..0,
            };
            crate::stringify_tokens(&tree_text[range], self.source)
        };
        let const_expansion = if path == "stringify" {
            stringified()
        } else if path != "concat" {
            None
        } else {
            parsed_arguments.as_ref().and_then(|arguments| {
                arguments
                    .iter()
                    .map(|argument| match argument {
                        ast::Expr::Literal(literal) => {
                            match arguments_lowerer.literal(literal)? {
                                LiteralKind::String { value } => Some(value),
                                LiteralKind::Character(value) => Some(value.to_string()),
                                LiteralKind::Integer { text, .. }
                                | LiteralKind::Float { text, .. } => Some(text),
                                LiteralKind::Boolean(value) => Some(value.to_string()),
                            }
                        }
                        _ => None,
                    })
                    .collect::<Option<Vec<_>>>()
                    .map(|parts| parts.concat())
            })
        };
        MacroExpression {
            path,
            tokens,
            format_string,
            arguments,
            const_expansion,
        }
    }
}

/// Syn's `Stmt::Macro`: a macro invocation that forms a whole statement and
/// either ends with `;` or uses brace delimiters.
fn is_statement_macro(node: &SyntaxNode) -> bool {
    let Some(parent) = node.parent() else {
        return false;
    };
    let statement_position = match parent.kind() {
        SyntaxKind::EXPR_STMT => true,
        SyntaxKind::STMT_LIST => true,
        _ => false,
    };
    if !statement_position {
        return false;
    }
    if parent.kind() == SyntaxKind::EXPR_STMT
        && ast::ExprStmt::cast(parent).is_some_and(|stmt| stmt.semicolon_token().is_some())
    {
        return true;
    }
    ast::MacroExpr::cast(node.clone())
        .and_then(|expr| expr.macro_call())
        .and_then(|call| call.token_tree())
        .is_some_and(|tree| tt::delimiter_and_inner(&tree).0 == '{')
}

fn repeatable_if_let_scrutinee(expression: &ast::Expr) -> bool {
    match expression {
        ast::Expr::PathExpr(path) => path
            .path()
            .is_some_and(|path| !path_parts(&path).qself),
        ast::Expr::Literal(_) => true,
        ast::Expr::ParenExpr(paren) => paren
            .expr()
            .is_some_and(|inner| repeatable_if_let_scrutinee(&inner)),
        _ => false,
    }
}

fn lower_binary_operator(operator: ast::BinaryOp) -> BinaryOperator {
    use ast::{ArithOp, BinaryOp, CmpOp, LogicOp, Ordering};
    match operator {
        BinaryOp::ArithOp(ArithOp::Add) => BinaryOperator::Add,
        BinaryOp::ArithOp(ArithOp::Sub) => BinaryOperator::Subtract,
        BinaryOp::ArithOp(ArithOp::Mul) => BinaryOperator::Multiply,
        BinaryOp::ArithOp(ArithOp::Div) => BinaryOperator::Divide,
        BinaryOp::ArithOp(ArithOp::Rem) => BinaryOperator::Remainder,
        BinaryOp::ArithOp(ArithOp::Shl) => BinaryOperator::ShiftLeft,
        BinaryOp::ArithOp(ArithOp::Shr) => BinaryOperator::ShiftRight,
        BinaryOp::ArithOp(ArithOp::BitXor) => BinaryOperator::BitXor,
        BinaryOp::ArithOp(ArithOp::BitOr) => BinaryOperator::BitOr,
        BinaryOp::ArithOp(ArithOp::BitAnd) => BinaryOperator::BitAnd,
        BinaryOp::CmpOp(CmpOp::Eq { negated: false }) => BinaryOperator::Equal,
        BinaryOp::CmpOp(CmpOp::Eq { negated: true }) => BinaryOperator::NotEqual,
        BinaryOp::CmpOp(CmpOp::Ord {
            ordering: Ordering::Less,
            strict: true,
        }) => BinaryOperator::Less,
        BinaryOp::CmpOp(CmpOp::Ord {
            ordering: Ordering::Less,
            strict: false,
        }) => BinaryOperator::LessEqual,
        BinaryOp::CmpOp(CmpOp::Ord {
            ordering: Ordering::Greater,
            strict: true,
        }) => BinaryOperator::Greater,
        BinaryOp::CmpOp(CmpOp::Ord {
            ordering: Ordering::Greater,
            strict: false,
        }) => BinaryOperator::GreaterEqual,
        BinaryOp::LogicOp(LogicOp::And) => BinaryOperator::And,
        BinaryOp::LogicOp(LogicOp::Or) => BinaryOperator::Or,
        _ => BinaryOperator::Unsupported,
    }
}

fn lower_match_pattern(pattern: &ast::Pat) -> MatchPattern {
    match pattern {
        ast::Pat::OrPat(patterns) => {
            MatchPattern::Or(patterns.pats().map(|p| lower_match_pattern(&p)).collect())
        }
        ast::Pat::RangePat(range) => {
            let (start, end, inclusive) = range_pattern_parts(range);
            match (
                start.as_ref().and_then(lower_pattern_range_endpoint),
                end.as_ref().and_then(lower_pattern_range_endpoint),
            ) {
                (Some(start), Some(end)) => MatchPattern::Range {
                    start,
                    end,
                    inclusive,
                },
                _ => MatchPattern::Unsupported,
            }
        }
        ast::Pat::WildcardPat(_) => MatchPattern::Wildcard,
        ast::Pat::IdentPat(binding)
            if binding.ref_token().is_none()
                && binding.mut_token().is_none()
                && binding.at_token().is_none() =>
        {
            MatchPattern::Binding(unraw(&name_text(binding.name())))
        }
        ast::Pat::PathPat(path) => match path.path().map(|path| path_parts(&path)) {
            Some(path) if !path.qself => MatchPattern::Path(path.joined()),
            _ => MatchPattern::Unsupported,
        },
        ast::Pat::LiteralPat(literal) => {
            let Some(kind) = literal.literal().as_ref().and_then(literal_kind) else {
                return MatchPattern::Unsupported;
            };
            if literal.minus_token().is_some() {
                match kind {
                    LiteralKind::Integer { text, suffix } => MatchPattern::Literal(
                        LiteralKind::Integer {
                            text: format!("-{text}"),
                            suffix,
                        },
                    ),
                    _ => MatchPattern::Unsupported,
                }
            } else {
                match kind {
                    literal @ (LiteralKind::Integer { .. }
                    | LiteralKind::Boolean(_)
                    | LiteralKind::Character(_)) => MatchPattern::Literal(literal),
                    _ => MatchPattern::Unsupported,
                }
            }
        }
        _ => MatchPattern::Unsupported,
    }
}

/// Endpoints of a range pattern. Unlike `RangeItem`, this also splits the
/// legacy inclusive `...` operator.
fn range_pattern_parts(range: &ast::RangePat) -> (Option<ast::Pat>, Option<ast::Pat>, bool) {
    let mut start = None;
    let mut end = None;
    let mut operator = None;
    for element in range.syntax().children_with_tokens() {
        match element {
            ra_ap_syntax::NodeOrToken::Token(token)
                if operator.is_none()
                    && matches!(token.kind(), SyntaxKind::DOT2 | SyntaxKind::DOT2EQ | SyntaxKind::DOT3) =>
            {
                operator = Some(token.kind());
            }
            ra_ap_syntax::NodeOrToken::Node(node) => {
                if let Some(pattern) = ast::Pat::cast(node) {
                    if operator.is_none() {
                        start.get_or_insert(pattern);
                    } else {
                        end.get_or_insert(pattern);
                    }
                }
            }
            ra_ap_syntax::NodeOrToken::Token(_) => {}
        }
    }
    let inclusive = matches!(operator, Some(SyntaxKind::DOT2EQ | SyntaxKind::DOT3));
    (start, end, inclusive)
}

fn lower_pattern_range_endpoint(pattern: &ast::Pat) -> Option<LiteralKind> {
    let ast::Pat::LiteralPat(literal) = pattern else {
        return None;
    };
    let kind = literal_kind(&literal.literal()?)?;
    match (kind, literal.minus_token().is_some()) {
        (LiteralKind::Integer { text, suffix }, true) => Some(LiteralKind::Integer {
            text: format!("-{text}"),
            suffix,
        }),
        (kind @ (LiteralKind::Integer { .. } | LiteralKind::Character(_)), false) => Some(kind),
        _ => None,
    }
}
