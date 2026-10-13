//! Mapping from offsets in a parsed text back to UTF-8 byte offsets in the
//! original source file.
//!
//! Most syntax is parsed directly from the source, so offsets map by
//! identity. Macro arguments and local `macro_rules!` expansions are parsed
//! from synthesized text; their nodes map through a constant shift and, for
//! expansions, a per-token table pointing at the tokens' original locations.

use std::ops::Range;
use std::rc::Rc;

use ra_ap_syntax::{SyntaxKind, SyntaxNode, TextRange};

#[derive(Clone, Debug, Default)]
pub(crate) struct SpanMap {
    /// Added to a parsed offset before the token table lookup.
    delta: isize,
    /// Sorted by text range: (range in rendered text, range in source).
    tokens: Option<Rc<[(Range<usize>, Range<usize>)]>>,
}

impl SpanMap {
    pub fn shifted(&self, delta: isize) -> Self {
        Self {
            delta: self.delta + delta,
            tokens: self.tokens.clone(),
        }
    }

    pub fn expansion(delta: isize, tokens: Vec<(Range<usize>, Range<usize>)>) -> Self {
        Self {
            delta,
            tokens: Some(tokens.into()),
        }
    }

    fn shift(&self, offset: usize) -> usize {
        (offset as isize + self.delta).max(0) as usize
    }

    pub fn map_offsets(&self, range: Range<usize>) -> Range<usize> {
        let start = self.shift(range.start);
        let end = self.shift(range.end).max(start);
        let Some(tokens) = &self.tokens else {
            return start..end;
        };
        if tokens.is_empty() {
            return 0..0;
        }
        let first = tokens
            .partition_point(|(text, _)| text.end <= start)
            .min(tokens.len() - 1);
        if end <= start {
            let source = &tokens[first].1;
            return source.start..source.start;
        }
        let last = tokens
            .partition_point(|(text, _)| text.start < end)
            .saturating_sub(1)
            .max(first);
        let source_start = tokens[first].1.start;
        let mut source_end = tokens[last].1.end;
        if source_end < source_start {
            source_end = tokens[first].1.end;
        }
        source_start..source_end
    }

    pub fn map(&self, range: TextRange) -> Range<usize> {
        self.map_offsets(usize::from(range.start())..usize::from(range.end()))
    }

    /// Span of a node without the leading and trailing trivia that
    /// rust-analyzer attaches to items (plain comments, whitespace).
    pub fn node(&self, node: &SyntaxNode) -> Range<usize> {
        self.map(trimmed_range(node))
    }
}

pub(crate) fn trimmed_range(node: &SyntaxNode) -> TextRange {
    let range = node.text_range();
    let mut start = range.start();
    let mut end = range.end();
    let mut token = node.first_token();
    while let Some(current) = token {
        if current.text_range().start() >= range.end() || !is_trivia(current.kind()) {
            break;
        }
        start = current.text_range().end();
        token = current.next_token();
    }
    let mut token = node.last_token();
    while let Some(current) = token {
        if current.text_range().end() <= start || !is_trivia(current.kind()) {
            break;
        }
        end = current.text_range().start();
        token = current.prev_token();
    }
    TextRange::new(start, end.max(start))
}

fn is_trivia(kind: SyntaxKind) -> bool {
    matches!(kind, SyntaxKind::WHITESPACE | SyntaxKind::COMMENT)
}
