use bevy::prelude::*;

use crate::macro_store::{char_key, MacroBooks};
use kuluu_render::hud::macro_editor::{MacroEditorData, MacroEditorState};
use kuluu_render::hud::macro_palette::slot_label;
use kuluu_render::hud::macros::MACRO_LINES;
use kuluu_render::{InputMode, MenuKind, SceneState};

/// Populate `MacroEditorData` while the MacroPage editor is the top menu level.
/// The focused slot's lines are overlaid with the in-flight draft (kept out of
/// `MacroBooks` until the line is committed, so keystrokes don't rewrite the
/// macros file). Clears the data when the editor closes so the screen hides.
pub fn macro_editor_provider_system(
    mode: Res<InputMode>,
    state: Res<SceneState>,
    mut macro_books: ResMut<MacroBooks>,
    editor_state: Res<MacroEditorState>,
    mut data: ResMut<MacroEditorData>,
) {
    let editor_open = match &*mode {
        InputMode::Menu(stack) => matches!(
            stack.current().map(|l| l.kind),
            Some(MenuKind::MacroPage { .. })
        ),
        _ => false,
    };
    if !editor_open {
        if !data.slot_labels.is_empty() {
            *data = MacroEditorData::default();
        }
        return;
    }

    let Some((book, page)) = (match &*mode {
        InputMode::Menu(stack) => match stack.current().map(|l| l.kind) {
            Some(MenuKind::MacroPage { book, page }) => Some((book, page)),
            _ => None,
        },
        _ => None,
    }) else {
        *data = MacroEditorData::default();
        return;
    };
    let Some(char_id) = state.snapshot.self_char_id else {
        *data = MacroEditorData::default();
        return;
    };

    let books = macro_books.get_or_default(&char_key(Some(char_id)));
    let Some(page_ref) = books.books.get(book).and_then(|b| b.pages.get(page)) else {
        *data = MacroEditorData::default();
        return;
    };

    let mut slot_labels = Vec::with_capacity(page_ref.macros.len());
    let mut all_lines: Vec<[String; MACRO_LINES]> = Vec::with_capacity(page_ref.macros.len());
    for m in &page_ref.macros {
        slot_labels.push(slot_label(&m.lines));
        all_lines.push(std::array::from_fn(|i| m.lines[i].clone()));
    }
    if editor_state.editing {
        if let Some(entry) = all_lines.get_mut(editor_state.slot) {
            if let Some(line) = entry.get_mut(editor_state.line) {
                *line = editor_state.draft.clone();
            }
        }
    }

    let next = MacroEditorData {
        book,
        page,
        slot_labels,
        lines: all_lines,
        state: editor_state.clone(),
    };
    if data.book != next.book
        || data.page != next.page
        || data.slot_labels != next.slot_labels
        || data.lines != next.lines
        || data.state.slot != next.state.slot
        || data.state.line != next.state.line
        || data.state.editing != next.state.editing
        || data.state.draft != next.state.draft
    {
        *data = next;
    }
}
