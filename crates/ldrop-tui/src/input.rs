// SPDX-License-Identifier: GPL-3.0-or-later
//! A one-line text field with the usual readline keys.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LineInput {
    text: String,
    /// Byte offset, always on a char boundary.
    cursor: usize,
}

impl LineInput {
    pub fn new(text: impl Into<String>) -> Self {
        let text = text.into();
        LineInput { cursor: text.len(), text }
    }

    pub fn text(&self) -> &str {
        &self.text
    }

    pub fn is_empty(&self) -> bool {
        self.text.is_empty()
    }

    /// Replaces the text and puts the cursor at the end.
    pub fn set(&mut self, text: impl Into<String>) {
        *self = LineInput::new(text);
    }

    pub fn clear(&mut self) {
        self.set("");
    }

    /// Applies an editing key. Returns false for keys a field doesn't handle.
    pub fn handle(&mut self, key: KeyEvent) -> bool {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Char('a') if ctrl => self.cursor = 0,
            KeyCode::Char('e') if ctrl => self.cursor = self.text.len(),
            KeyCode::Char('u') if ctrl => {
                self.text.drain(..self.cursor);
                self.cursor = 0;
            }
            KeyCode::Char('k') if ctrl => self.text.truncate(self.cursor),
            KeyCode::Char('w') if ctrl => {
                // Like readline's filename rubout: trailing separators go with the word.
                let before = self.text[..self.cursor].trim_end_matches([' ', '/']);
                let start = before.rfind([' ', '/']).map_or(0, |i| i + 1);
                self.text.drain(start..self.cursor);
                self.cursor = start;
            }
            KeyCode::Char(c) if !ctrl && !key.modifiers.contains(KeyModifiers::ALT) => {
                self.text.insert(self.cursor, c);
                self.cursor += c.len_utf8();
            }
            KeyCode::Backspace if self.cursor > 0 => {
                let start = self.prev(self.cursor);
                self.text.drain(start..self.cursor);
                self.cursor = start;
            }
            KeyCode::Delete if self.cursor < self.text.len() => {
                let end = self.next(self.cursor);
                self.text.drain(self.cursor..end);
            }
            KeyCode::Left => self.cursor = self.prev(self.cursor),
            KeyCode::Right => self.cursor = self.next(self.cursor),
            KeyCode::Home => self.cursor = 0,
            KeyCode::End => self.cursor = self.text.len(),
            KeyCode::Backspace | KeyCode::Delete => {}
            _ => return false,
        }
        true
    }

    fn prev(&self, i: usize) -> usize {
        self.text[..i].char_indices().next_back().map_or(0, |(j, _)| j)
    }

    fn next(&self, i: usize) -> usize {
        self.text[i..].chars().next().map_or(i, |c| i + c.len_utf8())
    }

    /// What fits into `width` columns, scrolled so the cursor stays visible, and the cursor's
    /// column within it.
    pub fn view(&self, width: usize) -> (String, u16) {
        let width = width.max(1);
        let mut start = 0;
        // Keep one column free for the cursor at the end.
        while self.text[start..self.cursor].width() >= width {
            start = self.next(start);
        }
        let cursor_col = self.text[start..self.cursor].width();
        let mut shown = String::new();
        let mut used = 0;
        for c in self.text[start..].chars() {
            let w = c.width().unwrap_or(0);
            if used + w > width {
                break;
            }
            used += w;
            shown.push(c);
        }
        (shown, cursor_col as u16)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn ctrl(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL)
    }

    fn typed(s: &str) -> LineInput {
        let mut input = LineInput::default();
        for c in s.chars() {
            input.handle(key(KeyCode::Char(c)));
        }
        input
    }

    #[test]
    fn editing() {
        let mut input = typed("grüße");
        input.handle(key(KeyCode::Backspace));
        assert_eq!(input.text(), "grüß");
        input.handle(key(KeyCode::Left));
        input.handle(key(KeyCode::Left));
        input.handle(key(KeyCode::Char('X')));
        assert_eq!(input.text(), "grXüß");
        input.handle(key(KeyCode::Delete));
        assert_eq!(input.text(), "grXß");
        input.handle(ctrl('a'));
        input.handle(key(KeyCode::Char('>')));
        assert_eq!(input.text(), ">grXß");
        input.handle(ctrl('k'));
        assert_eq!(input.text(), ">");
        assert!(!input.handle(key(KeyCode::Enter)));
    }

    #[test]
    fn delete_word() {
        let mut input = typed("~/Pictures/holiday");
        input.handle(ctrl('w'));
        assert_eq!(input.text(), "~/Pictures/");
        input.handle(ctrl('w'));
        assert_eq!(input.text(), "~/");
        input.handle(ctrl('u'));
        assert!(input.is_empty());
    }

    #[test]
    fn view_scrolls_to_cursor() {
        let input = LineInput::new("abcdefghij");
        assert_eq!(input.view(20), ("abcdefghij".into(), 10));
        assert_eq!(input.view(5), ("ghij".into(), 4));
        let mut input = input;
        input.handle(key(KeyCode::Home));
        assert_eq!(input.view(5), ("abcde".into(), 0));
    }
}
