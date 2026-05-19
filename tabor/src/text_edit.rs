use unicode_width::UnicodeWidthChar;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct TextEditState {
    text: String,
    cursor: usize,
}

impl TextEditState {
    pub(crate) fn new(text: String) -> Self {
        let cursor = text.chars().count();
        Self { text, cursor }
    }

    #[cfg(test)]
    pub(crate) fn text(&self) -> &str {
        &self.text
    }

    #[cfg(test)]
    pub(crate) fn cursor(&self) -> usize {
        self.cursor
    }

    pub(crate) fn into_text(self) -> String {
        self.text
    }

    pub(crate) fn move_left(&mut self) -> bool {
        if self.cursor == 0 {
            return false;
        }

        self.cursor -= 1;
        true
    }

    pub(crate) fn move_right(&mut self) -> bool {
        let len = self.text.chars().count();
        if self.cursor >= len {
            return false;
        }

        self.cursor += 1;
        true
    }

    pub(crate) fn move_home(&mut self) -> bool {
        if self.cursor == 0 {
            return false;
        }

        self.cursor = 0;
        true
    }

    pub(crate) fn move_end(&mut self) -> bool {
        let len = self.text.chars().count();
        if self.cursor == len {
            return false;
        }

        self.cursor = len;
        true
    }

    pub(crate) fn backspace(&mut self) -> bool {
        if self.cursor == 0 {
            return false;
        }

        let start = char_to_byte_idx(&self.text, self.cursor - 1);
        let end = char_to_byte_idx(&self.text, self.cursor);
        self.text.replace_range(start..end, "");
        self.cursor -= 1;
        true
    }

    pub(crate) fn delete(&mut self) -> bool {
        let len = self.text.chars().count();
        if self.cursor >= len {
            return false;
        }

        let start = char_to_byte_idx(&self.text, self.cursor);
        let end = char_to_byte_idx(&self.text, self.cursor + 1);
        self.text.replace_range(start..end, "");
        true
    }

    pub(crate) fn insert_text(&mut self, text: &str) -> bool {
        let mut filtered = String::new();
        for ch in text.chars() {
            if !ch.is_control() {
                filtered.push(ch);
            }
        }

        if filtered.is_empty() {
            return false;
        }

        let idx = char_to_byte_idx(&self.text, self.cursor);
        self.text.insert_str(idx, &filtered);
        self.cursor += filtered.chars().count();
        true
    }

    pub(crate) fn layout(&self, max_columns: usize) -> TextEditLayout {
        TextEditLayout::new(&self.text, self.cursor, max_columns)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct TextEditLayout {
    pub(crate) visible_text: String,
    pub(crate) visible_columns: usize,
    pub(crate) cursor_column: usize,
}

impl TextEditLayout {
    fn new(text: &str, cursor: usize, max_columns: usize) -> Self {
        if max_columns == 0 {
            return Self { visible_text: String::new(), visible_columns: 0, cursor_column: 0 };
        }

        let chars: Vec<char> = text.chars().collect();
        let cursor = cursor.min(chars.len());
        let mut columns_by_boundary = Vec::with_capacity(chars.len() + 1);
        columns_by_boundary.push(0);

        let mut columns = 0;
        for ch in &chars {
            columns += ch.width().unwrap_or(0);
            columns_by_boundary.push(columns);
        }

        let cursor_columns = columns_by_boundary[cursor];
        let mut start = 0;
        while start < cursor
            && cursor_columns.saturating_sub(columns_by_boundary[start]) > max_columns
        {
            start += 1;
        }

        let mut visible_text = String::new();
        let mut visible_columns = 0;
        for ch in chars.iter().skip(start) {
            let ch_columns = ch.width().unwrap_or(0);
            if visible_columns + ch_columns > max_columns {
                break;
            }
            visible_columns += ch_columns;
            visible_text.push(*ch);
        }

        let cursor_column = cursor_columns.saturating_sub(columns_by_boundary[start]);
        Self { visible_text, visible_columns, cursor_column }
    }
}

fn char_to_byte_idx(text: &str, char_idx: usize) -> usize {
    if char_idx == 0 {
        return 0;
    }

    text.char_indices().nth(char_idx).map(|(idx, _)| idx).unwrap_or_else(|| text.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn edits_in_the_middle_without_byte_index_errors() {
        let mut state = TextEditState::new(String::from("coex"));
        state.move_left();
        state.move_left();

        assert!(state.insert_text("d"));
        assert_eq!(state.text(), "codex");
        assert_eq!(state.cursor(), 3);

        assert!(state.backspace());
        assert_eq!(state.text(), "coex");

        assert!(state.delete());
        assert_eq!(state.text(), "cox");
    }

    #[test]
    fn ignores_control_character_input() {
        let mut state = TextEditState::new(String::from("codex"));

        assert!(!state.insert_text("\n\t"));
        assert_eq!(state.text(), "codex");
    }

    #[test]
    fn layout_does_not_emit_a_caret_glyph() {
        let mut state = TextEditState::new(String::from("codex"));
        state.move_left();
        state.move_left();

        let layout = state.layout(10);
        assert_eq!(layout.visible_text, "codex");
        assert_eq!(layout.cursor_column, 3);
    }

    #[test]
    fn layout_keeps_end_cursor_visible_for_long_text() {
        let state = TextEditState::new(String::from("abcdefghij"));

        let layout = state.layout(5);
        assert_eq!(layout.visible_text, "fghij");
        assert_eq!(layout.visible_columns, 5);
        assert_eq!(layout.cursor_column, 5);
    }

    #[test]
    fn layout_counts_full_width_characters() {
        let state = TextEditState::new(String::from("ab界cd"));

        let layout = state.layout(4);
        assert_eq!(layout.visible_text, "界cd");
        assert_eq!(layout.visible_columns, 4);
        assert_eq!(layout.cursor_column, 4);
    }
}
