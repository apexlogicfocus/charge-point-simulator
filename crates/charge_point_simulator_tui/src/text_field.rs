/// A single-line, unicode-aware text input: a string buffer plus a cursor position
/// (in `char`s, not bytes) and an optional byte-length cap.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TextField {
    value: String,
    cursor: usize,
    max_bytes: Option<usize>,
}

impl TextField {
    pub fn new(value: impl Into<String>) -> Self {
        let value = value.into();
        let cursor = value.chars().count();
        Self {
            value,
            cursor,
            max_bytes: None,
        }
    }

    pub fn with_max_bytes(mut self, max_bytes: usize) -> Self {
        self.max_bytes = Some(max_bytes);
        self
    }

    pub fn value(&self) -> &str {
        &self.value
    }

    pub fn cursor(&self) -> usize {
        self.cursor
    }

    pub fn insert_char(&mut self, c: char) {
        if let Some(max_bytes) = self.max_bytes
            && self.value.len() + c.len_utf8() > max_bytes
        {
            return;
        }
        let byte_index = self.byte_index(self.cursor);
        self.value.insert(byte_index, c);
        self.cursor += 1;
    }

    pub fn backspace(&mut self) {
        if self.cursor == 0 {
            return;
        }
        let start = self.byte_index(self.cursor - 1);
        let end = self.byte_index(self.cursor);
        self.value.replace_range(start..end, "");
        self.cursor -= 1;
    }

    pub fn delete(&mut self) {
        let len = self.value.chars().count();
        if self.cursor >= len {
            return;
        }
        let start = self.byte_index(self.cursor);
        let end = self.byte_index(self.cursor + 1);
        self.value.replace_range(start..end, "");
    }

    pub fn move_left(&mut self) {
        self.cursor = self.cursor.saturating_sub(1);
    }

    pub fn move_right(&mut self) {
        let len = self.value.chars().count();
        if self.cursor < len {
            self.cursor += 1;
        }
    }

    pub fn move_home(&mut self) {
        self.cursor = 0;
    }

    pub fn move_end(&mut self) {
        self.cursor = self.value.chars().count();
    }

    fn byte_index(&self, char_index: usize) -> usize {
        self.value
            .char_indices()
            .nth(char_index)
            .map(|(index, _)| index)
            .unwrap_or(self.value.len())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_places_the_cursor_at_the_end_of_the_prefilled_value() {
        let field = TextField::new("hello");
        assert_eq!(field.value(), "hello");
        assert_eq!(field.cursor(), 5);
    }

    #[test]
    fn insert_char_inserts_at_the_cursor_and_advances_it() {
        let mut field = TextField::new("helo");
        field.move_left();
        field.move_left();
        field.insert_char('l');
        assert_eq!(field.value(), "hello");
        assert_eq!(field.cursor(), 3);
    }

    #[test]
    fn backspace_removes_the_character_before_the_cursor() {
        let mut field = TextField::new("hello");
        field.backspace();
        assert_eq!(field.value(), "hell");
        assert_eq!(field.cursor(), 4);
    }

    #[test]
    fn backspace_at_the_start_does_nothing() {
        let mut field = TextField::new("hello");
        field.move_home();
        field.backspace();
        assert_eq!(field.value(), "hello");
        assert_eq!(field.cursor(), 0);
    }

    #[test]
    fn delete_removes_the_character_after_the_cursor() {
        let mut field = TextField::new("hello");
        field.move_home();
        field.delete();
        assert_eq!(field.value(), "ello");
        assert_eq!(field.cursor(), 0);
    }

    #[test]
    fn delete_at_the_end_does_nothing() {
        let mut field = TextField::new("hello");
        field.delete();
        assert_eq!(field.value(), "hello");
        assert_eq!(field.cursor(), 5);
    }

    #[test]
    fn cursor_movement_clamps_at_both_ends() {
        let mut field = TextField::new("ab");
        field.move_right();
        field.move_right();
        field.move_right();
        assert_eq!(field.cursor(), 2);

        field.move_left();
        field.move_left();
        field.move_left();
        assert_eq!(field.cursor(), 0);
    }

    #[test]
    fn home_and_end_jump_to_the_boundaries() {
        let mut field = TextField::new("hello");
        field.move_home();
        assert_eq!(field.cursor(), 0);
        field.move_end();
        assert_eq!(field.cursor(), 5);
    }

    #[test]
    fn insert_char_respects_a_multibyte_character_correctly() {
        let mut field = TextField::new("café");
        assert_eq!(field.cursor(), 4);
        field.backspace();
        assert_eq!(field.value(), "caf");
        field.insert_char('é');
        assert_eq!(field.value(), "café");
    }

    #[test]
    fn insert_char_is_a_no_op_once_the_byte_cap_is_reached() {
        let mut field = TextField::new("ab").with_max_bytes(3);
        field.insert_char('c');
        assert_eq!(field.value(), "abc");
        field.insert_char('d');
        assert_eq!(field.value(), "abc");
    }

    #[test]
    fn a_multibyte_char_that_would_exceed_the_cap_is_rejected() {
        // 'é' is 2 bytes in UTF-8; a 1-byte cap can never fit it.
        let mut field = TextField::default().with_max_bytes(1);
        field.insert_char('é');
        assert_eq!(field.value(), "");
    }
}
