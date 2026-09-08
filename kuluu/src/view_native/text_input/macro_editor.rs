use super::*;

use crate::macro_store::{char_key, MacroBooks};
use crate::view_native::text_input::macro_exec::{fire_macro, MacroSequencer};
use kuluu_render::hud::macro_editor::{MacroEditorState, MACRO_EDITOR_COLUMNS};
use kuluu_render::hud::macros::MACRO_LINES;
use kuluu_render::ActiveMacroPage;

/// Keep a locally-edited line bounded; retail's macro text box caps line length
/// the same way (any command this client executes is far shorter).
const MACRO_LINE_MAX_CHARS: usize = 256;

/// Drive the bespoke macro-page editor (`MenuKind::MacroPage`): slot navigation
/// on the 2 x 10 grid, Enter to fire the focused macro, Tab to open one-line
/// text entry, Esc to return to the book list. Commits write straight into
/// `MacroBooks`, so `persist_macros_on_change` saves the character's file.
#[allow(clippy::too_many_arguments)]
pub(super) fn handle_macro_editor_key(
    key: &Key,
    bindings: &Bindings,
    stack: &mut MenuStack,
    book: usize,
    page: usize,
    scene_state: &mut SceneState,
    editor: &mut MacroEditorState,
    macro_books: &mut MacroBooks,
    macro_sequencer: &mut MacroSequencer,
    active_macro_page: &mut ActiveMacroPage,
) -> Option<InputMode> {
    *active_macro_page = ActiveMacroPage { book, page };

    if editor.editing {
        return handle_line_edit_key(key, bindings, book, page, scene_state, editor, macro_books);
    }

    if bindings.matches_logical(Action::NavCancel, key) {
        editor.reset();
        return if stack.pop() {
            None
        } else {
            Some(InputMode::World)
        };
    }

    let axis: (i64, i64) = if bindings.matches_logical(Action::NavLeft, key) {
        (-1, 0)
    } else if bindings.matches_logical(Action::NavRight, key) {
        (1, 0)
    } else if bindings.matches_logical(Action::NavUp, key) {
        (0, -1)
    } else if bindings.matches_logical(Action::NavDown, key) {
        (0, 1)
    } else {
        (0, 0)
    };
    if axis != (0, 0) {
        editor.slot = step_slot(editor.slot, axis.0, axis.1);
        return None;
    }

    // Tab opens the focused slot's text entry.
    if key == &Key::Tab {
        start_editing(book, page, editor, macro_books, scene_state);
        return None;
    }

    // Enter (NavConfirm or ChatSubmit) fires the focused slot's macro.
    if bindings.matches_logical(Action::NavConfirm, key)
        || bindings.matches_logical(Action::ChatSubmit, key)
    {
        fire_slot(
            book,
            page,
            editor.slot,
            scene_state,
            macro_books,
            macro_sequencer,
        );
        return None;
    }

    None
}

/// Move a 2 x 10 grid cursor, wrapping in both axes.
fn step_slot(slot: usize, dx: i64, dy: i64) -> usize {
    let cols = MACRO_EDITOR_COLUMNS as i64;
    let col = (slot as i64 % cols + dx).rem_euclid(cols);
    let row = (slot as i64 / cols + dy).rem_euclid(2);
    (row * cols + col) as usize
}

/// Open text entry for the focused slot, preloading the first line.
fn start_editing(
    book: usize,
    page: usize,
    editor: &mut MacroEditorState,
    macro_books: &mut MacroBooks,
    scene_state: &mut SceneState,
) {
    editor.editing = true;
    editor.line = 0;
    load_line(book, page, editor, macro_books, scene_state);
}

/// Fire the focused slot's macro (retail: running from the editor and from the
/// palette share one sequencer, which continues executing while the menu is
/// open).
fn fire_slot(
    book: usize,
    page: usize,
    slot: usize,
    scene_state: &mut SceneState,
    macro_books: &mut MacroBooks,
    macro_sequencer: &mut MacroSequencer,
) {
    let key = char_key(scene_state.snapshot.self_char_id);
    let books = macro_books.get_or_default_mut(&key);
    let Some(book_ref) = books.books.get(book) else {
        return;
    };
    let Some(m) = book_ref.pages.get(page).and_then(|p| p.macros.get(slot)) else {
        return;
    };
    if m.is_empty() {
        push_system_chat_line(scene_state, "[macro] empty slot".into());
        return;
    }
    push_system_chat_line(scene_state, format!("[macro] fired slot {}", slot + 1));
    let snapshot_book = books.books.get(book).unwrap();
    fire_macro(slot, page, snapshot_book, macro_sequencer);
}

/// One-line text entry over `lines[line]` of the focused slot. Up/Down pick the
/// line; printing characters append to the draft; Enter commits the draft to
/// the character's books and advances the line; Backspace pops the last char;
/// Esc commits and returns to slot navigation.
fn handle_line_edit_key(
    key: &Key,
    bindings: &Bindings,
    book: usize,
    page: usize,
    scene_state: &mut SceneState,
    editor: &mut MacroEditorState,
    macro_books: &mut MacroBooks,
) -> Option<InputMode> {
    if bindings.matches_logical(Action::ChatBackspace, key) {
        editor.draft.pop();
        return None;
    }

    if bindings.matches_logical(Action::ChatExit, key)
        || bindings.matches_logical(Action::NavCancel, key)
    {
        // Commit the typed draft on exit so nothing is lost.
        commit_line(book, page, editor, macro_books, scene_state);
        editor.editing = false;
        editor.draft.clear();
        return None;
    }

    if bindings.matches_logical(Action::NavConfirm, key)
        || bindings.matches_logical(Action::ChatSubmit, key)
    {
        commit_line(book, page, editor, macro_books, scene_state);
        editor.line = (editor.line + 1) % MACRO_LINES;
        load_line(book, page, editor, macro_books, scene_state);
        return None;
    }

    if bindings.matches_logical(Action::NavUp, key)
        || bindings.matches_logical(Action::NavDown, key)
    {
        commit_line(book, page, editor, macro_books, scene_state);
        let step = usize::from(bindings.matches_logical(Action::NavUp, key));
        editor.line = match step {
            1 => (editor.line + MACRO_LINES - 1) % MACRO_LINES,
            _ => (editor.line + 1) % MACRO_LINES,
        };
        load_line(book, page, editor, macro_books, scene_state);
        return None;
    }

    match key {
        Key::Space => push_draft(editor, ' '),
        Key::Character(s) => {
            for c in s.chars().filter(|c| matches!(c, '\u{20}'..='\u{7E}')) {
                push_draft(editor, c);
            }
        }
        _ => {}
    }
    None
}

/// Caps the draft at `MACRO_LINE_MAX_CHARS`.
fn push_draft(editor: &mut MacroEditorState, c: char) {
    if editor.draft.chars().count() < MACRO_LINE_MAX_CHARS {
        editor.draft.push(c);
    }
}

/// Copy the stored `lines[line]` of the focused macro into the draft.
fn load_line(
    book: usize,
    page: usize,
    editor: &mut MacroEditorState,
    macro_books: &mut MacroBooks,
    scene_state: &mut SceneState,
) {
    let key = char_key(scene_state.snapshot.self_char_id);
    let Some(lines) = macro_books
        .get_or_default_mut(&key)
        .books
        .get(book)
        .and_then(|b| b.pages.get(page))
        .and_then(|p| p.macros.get(editor.slot))
        .map(|m| &m.lines)
    else {
        editor.draft.clear();
        return;
    };
    editor.draft = lines.get(editor.line).cloned().unwrap_or_default();
}

/// Overwrite the stored `lines[line]` of the focused macro with the draft,
/// skipping the write when the draft is unchanged.
fn commit_line(
    book: usize,
    page: usize,
    editor: &mut MacroEditorState,
    macro_books: &mut MacroBooks,
    scene_state: &mut SceneState,
) {
    let key = char_key(scene_state.snapshot.self_char_id);
    let Some(line) = macro_books
        .get_or_default_mut(&key)
        .books
        .get_mut(book)
        .and_then(|b| b.pages.get_mut(page))
        .and_then(|p| p.macros.get_mut(editor.slot))
        .map(|m| &mut m.lines[editor.line])
    else {
        return;
    };
    if *line != editor.draft {
        *line = editor.draft.clone();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn step_slot_wraps_horizontally() {
        assert_eq!(step_slot(0, -1, 0), 9, "left of col 0 wraps to col 9");
        assert_eq!(step_slot(9, 1, 0), 0, "right of col 9 wraps to col 0");
    }

    #[test]
    fn step_slot_wraps_vertically() {
        assert_eq!(step_slot(0, 0, -1), 10, "up from row 0 wraps to row 1");
        assert_eq!(step_slot(10, 0, 1), 0, "down from row 1 wraps to row 0");
    }

    #[test]
    fn step_slot_stays_in_bounds() {
        for slot in 0..20 {
            for (dx, dy) in [(1, 0), (-1, 0), (0, 1), (0, -1)] {
                assert!(step_slot(slot, dx, dy) < 20, "{slot} {dx} {dy}");
            }
        }
    }
}
