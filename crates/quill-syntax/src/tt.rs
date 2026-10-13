//! Token-tree helpers over rust-analyzer syntax.
//!
//! Macro inputs stay unparsed token trees in rust-analyzer's AST. Quill
//! inspects a few of them (derive lists, `repr`, local `macro_rules!`
//! definitions and invocations), so this module converts them into owned
//! token sequences with source spans and reproduces the `proc_macro2`
//! token spacing that the compiler model has always recorded.

use std::ops::Range;

use ra_ap_syntax::{AstNode, NodeOrToken, SyntaxKind, SyntaxToken, ast};

use crate::span::SpanMap;

const PUNCT_CHARS: &str = "~!@#$%^&*-=+|;:,<.>/?'";

#[derive(Clone, Debug)]
pub(crate) enum OwnedTt {
    Leaf(Leaf),
    Group(Group),
}

#[derive(Clone, Debug)]
pub(crate) struct Leaf {
    pub text: String,
    pub kind: SyntaxKind,
    pub source: Range<usize>,
    /// For single-character punctuation: whether the next character is
    /// adjacent punctuation (`proc_macro2::Spacing::Joint`).
    pub punct: Option<(char, bool)>,
}

#[derive(Clone, Debug)]
pub(crate) struct Group {
    pub delimiter: char,
    pub open: Range<usize>,
    pub close: Range<usize>,
    pub children: Vec<OwnedTt>,
}

impl OwnedTt {
    pub fn is_punct(&self, expected: char) -> bool {
        matches!(self, OwnedTt::Leaf(Leaf { punct: Some((ch, _)), .. }) if *ch == expected)
    }

    pub fn ident(&self) -> Option<&str> {
        match self {
            OwnedTt::Leaf(leaf) if leaf.punct.is_none() && !is_literal_kind(leaf.kind) => {
                Some(&leaf.text)
            }
            _ => None,
        }
    }

    pub fn group(&self) -> Option<&Group> {
        match self {
            OwnedTt::Group(group) => Some(group),
            OwnedTt::Leaf(_) => None,
        }
    }
}

pub(crate) fn is_literal_kind(kind: SyntaxKind) -> bool {
    matches!(
        kind,
        SyntaxKind::INT_NUMBER
            | SyntaxKind::FLOAT_NUMBER
            | SyntaxKind::STRING
            | SyntaxKind::BYTE_STRING
            | SyntaxKind::C_STRING
            | SyntaxKind::CHAR
            | SyntaxKind::BYTE
            | SyntaxKind::TRUE_KW
            | SyntaxKind::FALSE_KW
    )
}

fn is_punct_token(token: &SyntaxToken) -> bool {
    token.kind() != SyntaxKind::LIFETIME_IDENT
        && !is_literal_kind(token.kind())
        && token
            .text()
            .chars()
            .next()
            .is_some_and(|ch| PUNCT_CHARS.contains(ch))
}

fn delimiter_char(kind: SyntaxKind) -> Option<(char, bool)> {
    match kind {
        SyntaxKind::L_PAREN => Some(('(', true)),
        SyntaxKind::L_BRACK => Some(('[', true)),
        SyntaxKind::L_CURLY => Some(('{', true)),
        SyntaxKind::R_PAREN => Some((')', false)),
        SyntaxKind::R_BRACK => Some((']', false)),
        SyntaxKind::R_CURLY => Some(('}', false)),
        _ => None,
    }
}

/// The delimiter and inner (between-delimiter) range of a token tree, in the
/// coordinates of the tree's own text.
pub(crate) fn delimiter_and_inner(tt: &ast::TokenTree) -> (char, Range<usize>) {
    let node = tt.syntax();
    let range = node.text_range();
    let mut start = usize::from(range.start());
    let mut end = usize::from(range.end());
    let mut delimiter = '(';
    if let Some(first) = node.first_token()
        && let Some((ch, true)) = delimiter_char(first.kind())
    {
        delimiter = ch;
        start = usize::from(first.text_range().end());
    }
    if let Some(last) = node.last_token()
        && let Some((_, false)) = delimiter_char(last.kind())
        && usize::from(last.text_range().start()) >= start
    {
        end = usize::from(last.text_range().start());
    }
    (delimiter, start..end)
}

/// Convert the contents of a token tree (without its delimiters).
///
/// `original` is the unnormalized source text (or empty): multi-line
/// literals keep their original CRLF line endings from it.
pub(crate) fn owned_children(tt: &ast::TokenTree, map: &SpanMap, original: &str) -> Vec<OwnedTt> {
    let node = tt.syntax();
    let elements: Vec<_> = node.children_with_tokens().collect();
    let mut start = 0;
    let mut end = elements.len();
    if let Some(NodeOrToken::Token(first)) = elements.first()
        && matches!(delimiter_char(first.kind()), Some((_, true)))
    {
        start = 1;
    }
    if end > start
        && let Some(NodeOrToken::Token(last)) = elements.last()
        && matches!(delimiter_char(last.kind()), Some((_, false)))
    {
        end -= 1;
    }
    let mut output = Vec::new();
    for element in &elements[start..end] {
        match element {
            NodeOrToken::Token(token)
                if token.text().starts_with("//") || token.text().starts_with("/*") =>
            {
                push_doc_comment(token, map, &mut output);
            }
            NodeOrToken::Token(token) if token.kind().is_trivia() => {}
            NodeOrToken::Token(token) => push_token(token, map, original, &mut output),
            NodeOrToken::Node(child) => {
                if let Some(child) = ast::TokenTree::cast(child.clone()) {
                    output.push(OwnedTt::Group(owned_group(&child, map, original)));
                }
            }
        }
    }
    output
}

fn owned_group(tt: &ast::TokenTree, map: &SpanMap, original: &str) -> Group {
    let node = tt.syntax();
    let range = node.text_range();
    let (delimiter, inner) = delimiter_and_inner(tt);
    let open = usize::from(range.start())..inner.start;
    let close = inner.end..usize::from(range.end());
    Group {
        delimiter,
        open: map.map_offsets(open),
        close: map.map_offsets(close),
        children: owned_children(tt, map, original),
    }
}

/// `proc_macro2` turns doc comments inside token streams into
/// `#[doc = "..."]` (or `#![doc = "..."]`) token sequences.
fn push_doc_comment(token: &SyntaxToken, map: &SpanMap, output: &mut Vec<OwnedTt>) {
    let text = token.text();
    let (inner, body) = if let Some(body) = text.strip_prefix("//!") {
        (true, body)
    } else if let Some(body) = text.strip_prefix("///").filter(|body| !body.starts_with('/')) {
        (false, body)
    } else if let Some(body) = text.strip_prefix("/*!") {
        (true, body.strip_suffix("*/").unwrap_or(body))
    } else if let Some(body) = text
        .strip_prefix("/**")
        .filter(|body| !body.starts_with('*') && !body.starts_with('/'))
    {
        (false, body.strip_suffix("*/").unwrap_or(body))
    } else {
        return;
    };
    let range = token.text_range();
    let source = map.map_offsets(usize::from(range.start())..usize::from(range.end()));
    let punct = |ch: char| {
        OwnedTt::Leaf(Leaf {
            text: ch.to_string(),
            kind: SyntaxKind::POUND,
            source: source.clone(),
            punct: Some((ch, false)),
        })
    };
    output.push(punct('#'));
    if inner {
        output.push(punct('!'));
    }
    let mut literal = String::from('"');
    for ch in body.chars() {
        match ch {
            '\'' => literal.push(ch),
            _ => literal.extend(ch.escape_debug()),
        }
    }
    literal.push('"');
    output.push(OwnedTt::Group(Group {
        delimiter: '[',
        open: source.clone(),
        close: source.clone(),
        children: vec![
            OwnedTt::Leaf(Leaf {
                text: "doc".to_owned(),
                kind: SyntaxKind::IDENT,
                source: source.clone(),
                punct: None,
            }),
            punct('='),
            OwnedTt::Leaf(Leaf {
                text: literal,
                kind: SyntaxKind::STRING,
                source: source.clone(),
                punct: None,
            }),
        ],
    }));
}

fn push_token(token: &SyntaxToken, map: &SpanMap, original: &str, output: &mut Vec<OwnedTt>) {
    let range = token.text_range();
    let start = usize::from(range.start());
    if !is_punct_token(token) {
        let source = map.map_offsets(start..usize::from(range.end()));
        let text = match original.get(source.clone()) {
            Some(text) if token.text().contains('\n') && text.contains('\r') => text.to_owned(),
            _ => token.text().to_owned(),
        };
        output.push(OwnedTt::Leaf(Leaf {
            text,
            kind: token.kind(),
            source,
            punct: None,
        }));
        return;
    }
    let text = token.text();
    let next_joint = token.next_token().is_some_and(|next| {
        next.text_range().start() == range.end()
            && !matches!(next.kind(), SyntaxKind::COMMENT | SyntaxKind::WHITESPACE)
            && next
                .text()
                .chars()
                .next()
                .is_some_and(|ch| PUNCT_CHARS.contains(ch))
    });
    let chars: Vec<_> = text.char_indices().collect();
    for (index, (offset, ch)) in chars.iter().enumerate() {
        let joint = if index + 1 < chars.len() {
            true
        } else {
            next_joint
        };
        let at = start + offset;
        output.push(OwnedTt::Leaf(Leaf {
            text: ch.to_string(),
            kind: token.kind(),
            source: map.map_offsets(at..at + ch.len_utf8()),
            punct: Some((*ch, joint)),
        }));
    }
}

/// Format tokens like `proc_macro2::TokenStream`'s `Display` implementation.
pub(crate) fn display(tokens: &[OwnedTt]) -> String {
    let mut output = String::new();
    display_into(tokens, &mut output);
    output
}

fn display_into(tokens: &[OwnedTt], output: &mut String) {
    let mut joint = false;
    for (index, token) in tokens.iter().enumerate() {
        if index != 0 && !joint {
            output.push(' ');
        }
        joint = false;
        match token {
            OwnedTt::Group(group) => {
                let (open, close) = match group.delimiter {
                    '{' => ("{ ", "}"),
                    '[' => ("[", "]"),
                    _ => ("(", ")"),
                };
                output.push_str(open);
                display_into(&group.children, output);
                if group.delimiter == '{' && !group.children.is_empty() {
                    output.push(' ');
                }
                output.push_str(close);
            }
            OwnedTt::Leaf(leaf) => {
                if let Some((_, is_joint)) = leaf.punct {
                    joint = is_joint;
                }
                output.push_str(&leaf.text);
            }
        }
    }
}

/// Render tokens as parseable source text, recording where each token came
/// from so spans in a parse of the text can be mapped back to the file.
pub(crate) fn render(tokens: &[OwnedTt]) -> (String, Vec<(Range<usize>, Range<usize>)>) {
    let mut text = String::new();
    let mut mapping = Vec::new();
    let mut joint = true;
    render_into(tokens, &mut text, &mut mapping, &mut joint);
    (text, mapping)
}

fn render_into(
    tokens: &[OwnedTt],
    text: &mut String,
    mapping: &mut Vec<(Range<usize>, Range<usize>)>,
    joint: &mut bool,
) {
    for token in tokens {
        match token {
            OwnedTt::Leaf(leaf) => render_piece(
                &leaf.text,
                &leaf.source,
                leaf.punct.is_some_and(|(_, joint)| joint),
                text,
                mapping,
                joint,
            ),
            OwnedTt::Group(group) => {
                let (open, close) = match group.delimiter {
                    '{' => ("{", "}"),
                    '[' => ("[", "]"),
                    _ => ("(", ")"),
                };
                render_piece(open, &group.open, true, text, mapping, joint);
                render_into(&group.children, text, mapping, joint);
                render_piece(close, &group.close, false, text, mapping, joint);
            }
        }
    }
}

fn render_piece(
    piece: &str,
    source: &Range<usize>,
    next_joint: bool,
    text: &mut String,
    mapping: &mut Vec<(Range<usize>, Range<usize>)>,
    joint: &mut bool,
) {
    if !*joint {
        text.push(' ');
    }
    let start = text.len();
    text.push_str(piece);
    mapping.push((start..text.len(), source.clone()));
    *joint = next_joint;
}

/// Parse a comma-separated list of single-segment paths, as `derive`
/// accepts them. Returns `None` when the list is not a plain path list.
pub(crate) fn derive_paths(tokens: &[OwnedTt]) -> Option<Vec<(bool, Vec<String>)>> {
    let mut paths = Vec::new();
    let mut index = 0;
    while index < tokens.len() {
        let mut leading_colon = false;
        let mut segments = Vec::new();
        if tokens[index].is_punct(':') && tokens.get(index + 1)?.is_punct(':') {
            leading_colon = true;
            index += 2;
        }
        loop {
            segments.push(tokens.get(index)?.ident()?.to_owned());
            index += 1;
            if index + 1 < tokens.len()
                && tokens[index].is_punct(':')
                && tokens[index + 1].is_punct(':')
            {
                index += 2;
                continue;
            }
            break;
        }
        paths.push((leading_colon, segments));
        match tokens.get(index) {
            None => break,
            Some(token) if token.is_punct(',') => index += 1,
            Some(_) => return None,
        }
    }
    Some(paths)
}
