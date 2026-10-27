//! A document's text with conversions between byte offsets and LSP positions (UTF-16 columns).

use std::ops::Range as Bytes;

use tower_lsp_server::ls_types::{Position, Range};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Text {
    source: String,
    /// Byte offset each line starts at.
    lines: Vec<usize>,
}

impl Text {
    pub fn new(source: String) -> Self {
        let lines = std::iter::once(0)
            .chain(source.match_indices('\n').map(|(at, _)| at + 1))
            .collect();
        Text { source, lines }
    }

    pub fn as_str(&self) -> &str {
        &self.source
    }

    pub fn len(&self) -> usize {
        self.source.len()
    }

    pub fn is_empty(&self) -> bool {
        self.source.is_empty()
    }

    pub fn position(&self, offset: usize) -> Position {
        let offset = floor_boundary(&self.source, offset);
        let line = self
            .lines
            .partition_point(|&start| start <= offset)
            .saturating_sub(1);
        let start = self.lines.get(line).copied().unwrap_or(0);
        let character = self
            .source
            .get(start..offset)
            .map_or(0, |prefix| prefix.encode_utf16().count());
        Position::new(to_u32(line), to_u32(character))
    }

    pub fn range(&self, bytes: Bytes<usize>) -> Range {
        Range::new(self.position(bytes.start), self.position(bytes.end))
    }

    pub fn offset(&self, position: Position) -> usize {
        let Some(&start) = self.lines.get(position.line as usize) else {
            return self.source.len();
        };
        let line = self.line_text(start);
        let mut units = 0;
        for (at, c) in line.char_indices() {
            if units >= position.character as usize {
                return start + at;
            }
            units += c.len_utf16();
        }
        start + line.len()
    }

    /// The offset of a 0-based line and character column, as starlark's codemap counts them.
    pub fn char_offset(&self, line: usize, column: usize) -> usize {
        let Some(&start) = self.lines.get(line) else {
            return self.source.len();
        };
        let text = self.line_text(start);
        text.char_indices()
            .nth(column)
            .map_or(start + text.len(), |(at, _)| start + at)
    }

    /// The line starting at `start`, without its newline.
    fn line_text(&self, start: usize) -> &str {
        let rest = self.source.get(start..).unwrap_or("");
        rest.split_once('\n').map_or(rest, |(line, _)| line)
    }

    pub fn slice(&self, bytes: Bytes<usize>) -> &str {
        self.source.get(bytes).unwrap_or("")
    }
}

fn floor_boundary(source: &str, offset: usize) -> usize {
    let mut offset = offset.min(source.len());
    while offset > 0 && !source.is_char_boundary(offset) {
        offset -= 1;
    }
    offset
}

fn to_u32(n: usize) -> u32 {
    u32::try_from(n).unwrap_or(u32::MAX)
}

#[cfg(test)]
mod tests {
    use crate::text::*;

    #[test]
    fn positions_round_trip_through_offsets() {
        let text = Text::new("ab\ncd\n\nef".to_string());
        for offset in 0..=text.len() {
            assert_eq!(
                text.offset(text.position(offset)),
                offset,
                "offset {offset}"
            );
        }
        assert_eq!(text.position(4), Position::new(1, 1));
        assert_eq!(text.position(7), Position::new(3, 0));
    }

    #[test]
    fn columns_count_utf16_units() {
        let text = Text::new("é😀x".to_string());
        assert_eq!(text.position("é😀".len()), Position::new(0, 3));
        assert_eq!(text.offset(Position::new(0, 3)), "é😀".len());
        assert_eq!(text.position(1), Position::new(0, 0), "mid-char floors");
    }

    #[test]
    fn out_of_range_positions_clamp() {
        let text = Text::new("ab\ncd".to_string());
        assert_eq!(text.offset(Position::new(0, 99)), 2, "end of line");
        assert_eq!(text.offset(Position::new(9, 0)), text.len());
        assert_eq!(text.position(99), Position::new(1, 2));
    }

    #[test]
    fn character_columns_resolve_to_bytes() {
        let text = Text::new("x\né = 1\n".to_string());
        assert_eq!(text.char_offset(1, 1), 2 + "é".len());
        assert_eq!(text.char_offset(1, 99), 2 + "é = 1".len());
        assert_eq!(text.char_offset(7, 0), text.len());
    }
}
