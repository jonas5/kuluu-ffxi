use bevy::prelude::Resource;
use serde::{Deserialize, Serialize};

pub const MACRO_BOOKS: usize = 40;
pub const MACRO_PAGES: usize = 10;
pub const MACROS_PER_PAGE: usize = 20;
pub const MACRO_LINES: usize = 6;

pub const CTRL_MACRO_SLOTS: usize = 10;
pub const ALT_MACRO_SLOTS: usize = 10;

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct MacroBook {
    pub name: String,
    pub pages: [MacroPage; MACRO_PAGES],
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct MacroPage {
    pub name: String,
    pub macros: [Macro; MACROS_PER_PAGE],
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Macro {
    pub lines: [String; MACRO_LINES],
}

#[derive(Resource, Debug, Clone, Copy, Default)]
pub struct ActiveMacroPage {
    pub book: usize,
    pub page: usize,
}

impl MacroBook {
    pub fn with_name(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            ..Default::default()
        }
    }
}

impl Macro {
    pub fn first_active_line(&self) -> Option<&str> {
        self.lines
            .iter()
            .find(|l| !l.trim().is_empty())
            .map(String::as_str)
    }

    pub fn is_empty(&self) -> bool {
        self.lines.iter().all(|l| l.trim().is_empty())
    }
}

pub fn default_book_name(index: usize) -> String {
    format!("Book {}", index + 1)
}

pub fn default_page_name(index: usize) -> String {
    format!("Page {}", index + 1)
}

pub fn default_macro_book(index: usize) -> MacroBook {
    let mut book = MacroBook::with_name(default_book_name(index));
    for (pi, page) in book.pages.iter_mut().enumerate() {
        page.name = default_page_name(pi);
    }
    book
}

/// Map a Ctrl/Alt + Digit1..Digit0 keypress to a 0-based macro slot on the
/// current page. Ctrl maps to slots 0..9 (top row), Alt to 10..19 (bottom
/// row). Returns `None` when the modifier is not held or the key is not a
/// digit.
pub fn slot_from_modifier_digit(ctrl: bool, alt: bool, digit: u8) -> Option<usize> {
    if !ctrl && !alt {
        return None;
    }
    let base = if ctrl { 0 } else { CTRL_MACRO_SLOTS };
    let slot_in_row = match digit {
        1..=9 => (digit - 1) as usize,
        0 => 9,
        _ => return None,
    };
    Some(base + slot_in_row)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_macro_is_empty() {
        let m = Macro::default();
        assert!(m.is_empty());
        assert!(m.first_active_line().is_none());
    }

    #[test]
    fn first_active_line_finds_first_nonblank() {
        let m = Macro {
            lines: [
                "".into(),
                "  ".into(),
                "/heal".into(),
                "".into(),
                "".into(),
                "".into(),
            ],
        };
        assert_eq!(m.first_active_line(), Some("/heal"));
    }

    #[test]
    fn slot_ctrl_digit_maps_to_0_through_9() {
        assert_eq!(slot_from_modifier_digit(true, false, 1), Some(0));
        assert_eq!(slot_from_modifier_digit(true, false, 5), Some(4));
        assert_eq!(slot_from_modifier_digit(true, false, 0), Some(9));
    }

    #[test]
    fn slot_alt_digit_maps_to_10_through_19() {
        assert_eq!(slot_from_modifier_digit(false, true, 1), Some(10));
        assert_eq!(slot_from_modifier_digit(false, true, 5), Some(14));
        assert_eq!(slot_from_modifier_digit(false, true, 0), Some(19));
    }

    #[test]
    fn slot_no_modifier_returns_none() {
        assert_eq!(slot_from_modifier_digit(false, false, 1), None);
    }

    #[test]
    fn slot_invalid_digit_returns_none() {
        assert_eq!(slot_from_modifier_digit(true, false, 10), None);
    }

    #[test]
    fn default_book_has_correct_names() {
        let book = default_macro_book(5);
        assert_eq!(book.name, "Book 6");
        for (i, page) in book.pages.iter().enumerate() {
            assert_eq!(page.name, default_page_name(i));
        }
        assert!(book.pages[0].macros[0].is_empty());
    }

    #[test]
    fn macro_book_json_roundtrip() {
        let book = default_macro_book(0);
        let json = serde_json::to_string(&book).unwrap();
        let loaded: MacroBook = serde_json::from_str(&json).unwrap();
        assert_eq!(loaded.name, book.name);
        assert_eq!(loaded.pages[0].name, book.pages[0].name);
    }
}
