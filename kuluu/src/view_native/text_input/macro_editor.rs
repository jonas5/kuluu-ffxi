use super::*;

use crate::macro_store::{char_key, MacroBooks};
use crate::view_native::text_input::macro_exec::{fire_macro, MacroSequencer};
use bevy::picking::events::{Click, Pointer};
use bevy::picking::pointer::PointerButton;
use kuluu_render::hud::macro_editor::{
    MacroEditorLineIndex, MacroEditorSlotIndex, MacroEditorState, MACRO_EDITOR_COLUMNS,
};
use kuluu_render::hud::macro_palette::SLOT_LABEL_MAX_CHARS;
use kuluu_render::hud::macros::{MACROS_PER_PAGE, MACRO_LINES, MACRO_PAGES};
use kuluu_render::ActiveMacroPage;

/// Keep a locally-edited line bounded; retail's macro text box caps line length
/// the same way (any command this client executes is far shorter).
const MACRO_LINE_MAX_CHARS: usize = 256;

/// The first line of a macro is its name: the palette bar truncates it to
/// `SLOT_LABEL_MAX_CHARS`, so the name field caps typing at the same width.
fn line_cap(line: usize) -> usize {
    if line == 0 {
        SLOT_LABEL_MAX_CHARS
    } else {
        MACRO_LINE_MAX_CHARS
    }
}

/// Drive the bespoke macro-page editor (`MenuKind::MacroPage`): slot navigation
/// across the palette's single row, Enter to fire the focused macro, Tab to
/// open one-line text entry, Esc to return to the book list. Commits write
/// straight into `MacroBooks`, so `persist_macros_on_change` saves the
/// character's file.
#[allow(clippy::too_many_arguments)]
pub(super) fn handle_macro_editor_key(
    key: &Key,
    bindings: &Bindings,
    stack: &mut MenuStack,
    book: usize,
    mut page: usize,
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

    if bindings.matches_logical(Action::NavLeft, key) {
        editor.slot = step_slot_in_row(editor.slot, -1);
        return None;
    }
    if bindings.matches_logical(Action::NavRight, key) {
        editor.slot = step_slot_in_row(editor.slot, 1);
        return None;
    }
    if bindings.matches_logical(Action::NavUp, key) {
        page = step_page(page, -1);
    } else if bindings.matches_logical(Action::NavDown, key) {
        page = step_page(page, 1);
    } else {
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

        return None;
    }

    // A page change rewrites the top menu level and the shared pointer so the
    // next frame's provider/rendering line up on the new page.
    if let Some(level) = stack.current_mut() {
        if let MenuKind::MacroPage { page: p, .. } = &mut level.kind {
            *p = page;
        }
    }
    *active_macro_page = ActiveMacroPage { book, page };
    None
}

/// Move a palette-row cursor, wrapping within the row (cols 0..9).
fn step_slot_in_row(slot: usize, dx: i64) -> usize {
    let cols = MACRO_EDITOR_COLUMNS as i64;
    let row_base = slot / MACRO_EDITOR_COLUMNS * MACRO_EDITOR_COLUMNS;
    let col = (slot as i64 % cols + dx).rem_euclid(cols);
    (row_base as i64 + col) as usize
}

/// Wrap a macro page 1..10 cursor (0-based).
fn step_page(page: usize, dy: i64) -> usize {
    (page as i64 + dy).rem_euclid(MACRO_PAGES as i64) as usize
}

/// Open text entry for the focused slot, preloading the first line.
fn start_editing(
    book: usize,
    page: usize,
    editor: &mut MacroEditorState,
    macro_books: &mut MacroBooks,
    scene_state: &mut SceneState,
) {
    begin_editing(book, page, editor, macro_books, scene_state, 0);
}

/// Enter line-edit submode on `line`, preloading its stored text into the draft.
fn begin_editing(
    book: usize,
    page: usize,
    editor: &mut MacroEditorState,
    macro_books: &mut MacroBooks,
    scene_state: &mut SceneState,
    line: usize,
) {
    editor.editing = true;
    editor.line = line;
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

/// Caps the draft at the line's width (name line is `SLOT_LABEL_MAX_CHARS`).
fn push_draft(editor: &mut MacroEditorState, c: char) {
    if editor.draft.chars().count() < line_cap(editor.line) {
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

/// Click the editor's visible palette row or a line row to drive the editor: a
/// slot click focuses that slot and opens the name (first line) for editing; a
/// line click opens that line. Either commit path discards the in-flight draft
/// into the previous slot/line first, like keyboard navigation does.
#[allow(clippy::too_many_arguments)]
pub fn macro_editor_click_system(
    mut clicks: MessageReader<Pointer<Click>>,
    q_slot: Query<&MacroEditorSlotIndex>,
    q_line: Query<&MacroEditorLineIndex>,
    mode: Res<InputMode>,
    mut editor: ResMut<MacroEditorState>,
    mut macro_books: ResMut<MacroBooks>,
    mut scene_state: ResMut<SceneState>,
) {
    let InputMode::Menu(stack) = &*mode else {
        return;
    };
    let Some(MenuKind::MacroPage { book, page }) = stack.current().map(|l| l.kind) else {
        return;
    };
    for ev in clicks.read() {
        if ev.button != PointerButton::Primary {
            continue;
        }
        if let Ok(idx) = q_slot.get(ev.entity) {
            if editor.editing {
                commit_line(book, page, &mut editor, &mut macro_books, &mut scene_state);
            }
            let row_base = (editor.slot / MACRO_EDITOR_COLUMNS) * MACRO_EDITOR_COLUMNS;
            let slot = row_base + idx.0;
            if slot < MACROS_PER_PAGE {
                editor.slot = slot;
                begin_editing(
                    book,
                    page,
                    &mut editor,
                    &mut macro_books,
                    &mut scene_state,
                    0,
                );
            }
            return;
        }
        if let Ok(idx) = q_line.get(ev.entity) {
            if editor.editing {
                commit_line(book, page, &mut editor, &mut macro_books, &mut scene_state);
            }
            begin_editing(
                book,
                page,
                &mut editor,
                &mut macro_books,
                &mut scene_state,
                idx.0,
            );
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn step_slot_in_row_wraps_horizontally() {
        assert_eq!(
            step_slot_in_row(0, -1),
            9,
            "left of col 0 wraps to col 9 in the same row"
        );
        assert_eq!(
            step_slot_in_row(9, 1),
            0,
            "right of col 9 wraps to col 0 in the same row"
        );
        assert_eq!(
            step_slot_in_row(10, -1),
            19,
            "alt row left of col 0 wraps to col 9"
        );
        assert_eq!(
            step_slot_in_row(19, 1),
            10,
            "alt row right of col 9 wraps to col 0"
        );
    }

    #[test]
    fn step_slot_in_row_stays_in_bounds() {
        for slot in 0..20 {
            for dx in [1, -1] {
                let next = step_slot_in_row(slot, dx);
                assert!(next < 20, "{slot} {dx} -> {next}");
                assert_eq!(
                    next / MACRO_EDITOR_COLUMNS,
                    slot / MACRO_EDITOR_COLUMNS,
                    "{slot} {dx} left the row"
                );
            }
        }
    }

    #[test]
    fn step_page_wraps_vertically() {
        assert_eq!(step_page(0, -1), MACRO_PAGES - 1, "page 1 up wraps to 10");
        assert_eq!(step_page(MACRO_PAGES - 1, 1), 0, "page 10 down wraps to 1");
        assert_eq!(step_page(0, 1), 1);
        assert_eq!(step_page(MACRO_PAGES - 1, -1), MACRO_PAGES - 2);
    }

    #[test]
    fn name_line_caps_at_palette_label_width() {
        assert_eq!(line_cap(0), SLOT_LABEL_MAX_CHARS);
    }

    #[test]
    fn body_lines_keep_full_length() {
        assert_eq!(line_cap(5), MACRO_LINE_MAX_CHARS);
    }

    #[test]
    fn push_draft_on_name_line_stops_short() {
        let mut editor = MacroEditorState {
            line: 0,
            ..default()
        };
        for _ in 0..(SLOT_LABEL_MAX_CHARS + 4) {
            push_draft(&mut editor, 'x');
        }
        assert_eq!(editor.draft.chars().count(), SLOT_LABEL_MAX_CHARS);
    }
}
