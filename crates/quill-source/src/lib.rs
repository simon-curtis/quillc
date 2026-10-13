use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::Arc;

#[derive(Clone, Debug)]
pub struct SourceFile {
    path: PathBuf,
    text: Arc<str>,
    line_starts: Arc<[usize]>,
}

impl SourceFile {
    pub fn load(path: impl AsRef<Path>) -> std::io::Result<Self> {
        let path = path.as_ref().to_path_buf();
        let text: Arc<str> = std::fs::read_to_string(&path)?.into();
        let mut line_starts = vec![0];
        line_starts.extend(text.match_indices('\n').map(|(index, _)| index + 1));
        Ok(Self {
            path,
            text,
            line_starts: line_starts.into(),
        })
    }

    pub fn from_text(path: impl Into<PathBuf>, text: impl Into<Arc<str>>) -> Self {
        let text = text.into();
        let mut line_starts = vec![0];
        line_starts.extend(text.match_indices('\n').map(|(index, _)| index + 1));
        Self {
            path: path.into(),
            text,
            line_starts: line_starts.into(),
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn text(&self) -> &str {
        &self.text
    }

    pub fn slice(&self, span: Range<usize>) -> &str {
        &self.text[span]
    }

    /// Convert a one-based line and zero-based byte column to a bounded byte offset.
    pub fn byte_offset(&self, one_based_line: usize, zero_based_column: usize) -> usize {
        let line_start = self
            .line_starts
            .get(one_based_line.saturating_sub(1))
            .copied()
            .unwrap_or(self.text.len());
        line_start
            .saturating_add(zero_based_column)
            .min(self.text.len())
    }

    pub fn line_column(&self, byte_offset: usize) -> (usize, usize) {
        let line = self
            .line_starts
            .partition_point(|&start| start <= byte_offset)
            .saturating_sub(1);
        (line + 1, byte_offset - self.line_starts[line] + 1)
    }
}

#[cfg(test)]
mod tests {
    use super::SourceFile;

    #[test]
    fn maps_line_columns_to_bounded_byte_offsets() {
        let source = SourceFile::from_text("sample.rs", "a\né\n");
        assert_eq!(source.byte_offset(1, 0), 0);
        assert_eq!(source.byte_offset(2, 0), 2);
        assert_eq!(source.byte_offset(2, 2), 4);
        assert_eq!(source.byte_offset(3, 0), 5);
        assert_eq!(source.byte_offset(4, 100), 5);
        assert_eq!(source.byte_offset(0, 1), 1);
    }
}
