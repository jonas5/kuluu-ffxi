use bevy::input::keyboard::KeyCode;
use bevy::picking::events::{Click, Pointer};
use bevy::picking::pointer::PointerButton;
use bevy::prelude::*;

use crate::macro_store::{char_key, MacroBooks};
use crate::view_native::text_input::macro_exec::MacroSequencer;
use kuluu_render::hud::macro_palette::{slot_label, MacroPaletteData, MacroPaletteSlotIndex};
use kuluu_render::hud::macros::{CTRL_MACRO_SLOTS, MACROS_PER_PAGE};
use kuluu_render::{ActiveMacroPage, InputMode, MenuKind, MenuStack, SceneState, UiClickSurface};

/// Populate `MacroPaletteData` from the character's books for the active
/// book/page. The palette is hidden by default and shows only while a
/// modifier key is held, in the world: Ctrl previews the top-row macros
/// (slots 0..9, fired as Ctrl+1..0), Alt the bottom row (slots 10..19, fired
/// as Alt+1..0). Ctrl wins when both are held, mirroring the hotkey system.
pub fn macro_palette_provider_system(
    state: Res<SceneState>,
    mode: Res<InputMode>,
    keys: Res<ButtonInput<KeyCode>>,
    active_page: Res<ActiveMacroPage>,
    mut macro_books: ResMut<MacroBooks>,
    sequencer: Res<MacroSequencer>,
    mut palette: ResMut<MacroPaletteData>,
) {
    let Some(char_id) = state.snapshot.self_char_id else {
        hide(&mut palette);
        return;
    };
    if !matches!(*mode, InputMode::World) {
        hide(&mut palette);
        return;
    }
    let ctrl = keys.pressed(KeyCode::ControlLeft) || keys.pressed(KeyCode::ControlRight);
    let alt = keys.pressed(KeyCode::AltLeft) || keys.pressed(KeyCode::AltRight);
    if !ctrl && !alt {
        hide(&mut palette);
        return;
    }
    let row_offset = if ctrl { 0 } else { CTRL_MACRO_SLOTS };

    let books = macro_books.get_or_default(&char_key(Some(char_id)));
    let Some(book) = books.books.get(active_page.book) else {
        return;
    };
    let Some(page) = book.pages.get(active_page.page) else {
        return;
    };

    let modifier = if ctrl { "Ctrl" } else { "Alt" };
    let header = format!(
        "Book {} / Page {} - {} macros (click a slot to edit)",
        active_page.book + 1,
        active_page.page + 1,
        modifier,
    );
    let slots: Vec<String> = page.macros[row_offset..row_offset + CTRL_MACRO_SLOTS]
        .iter()
        .map(|m| slot_label(&m.lines))
        .collect();
    let active = sequencer
        .active_slot
        .filter(|_| sequencer.is_busy())
        .and_then(|s| {
            (row_offset..row_offset + CTRL_MACRO_SLOTS)
                .contains(&s)
                .then_some(s - row_offset)
        })
        .filter(|i| slots.get(*i).is_none_or(|l| !l.is_empty()));

    if palette.header != header
        || palette.slots != slots
        || palette.active != active
        || palette.row_offset != row_offset
    {
        *palette = MacroPaletteData {
            header,
            slots,
            active,
            row_offset,
        };
    }
}

fn hide(palette: &mut MacroPaletteData) {
    if palette.visible() {
        *palette = MacroPaletteData::default();
    }
}

/// Click a visible palette slot to open the macro editor (`MenuKind::MacroPage`)
/// focused on that slot. The clicked positional index plus the palette's row
/// offset yields the real 0..20 page slot. `UiClickSurface` on the palette
/// root already stops `click_to_target_system` from treating the click as a
/// world click (`picking.rs`), so only the editor open happens here.
#[allow(clippy::too_many_arguments)]
pub fn macro_palette_click_system(
    mut clicks: MessageReader<Pointer<Click>>,
    q_index: Query<&MacroPaletteSlotIndex>,
    q_ui_surface: Query<&UiClickSurface>,
    q_parent: Query<&ChildOf>,
    palette: Res<MacroPaletteData>,
    mut active_page: ResMut<ActiveMacroPage>,
    mut editor_state: ResMut<kuluu_render::hud::macro_editor::MacroEditorState>,
    mut input_mode: ResMut<InputMode>,
) {
    if !matches!(*input_mode, InputMode::World) {
        return;
    }
    for ev in clicks.read() {
        if ev.button != PointerButton::Primary {
            continue;
        }
        let Ok(index) = q_index.get(ev.entity) else {
            continue;
        };
        debug_assert!(
            ui_surface_ancestor(ev.entity, &q_parent, &q_ui_surface),
            "palette slot not under the UiClickSurface root"
        );
        let slot = palette.row_offset + index.0;
        if slot >= MACROS_PER_PAGE {
            continue;
        }
        let book = active_page.book;
        let page = active_page.page;
        *active_page = ActiveMacroPage { book, page };
        editor_state.slot = slot;
        editor_state.line = 0;
        editor_state.draft.clear();
        editor_state.editing = false;
        let mut stack = MenuStack::root();
        stack.push(MenuKind::MacroPage { book, page });
        *input_mode = InputMode::Menu(stack);
        return;
    }
}

fn ui_surface_ancestor(
    hit: Entity,
    parent_q: &Query<&ChildOf>,
    ui_q: &Query<&UiClickSurface>,
) -> bool {
    let mut entity = hit;
    for _ in 0..16 {
        if ui_q.get(entity).is_ok() {
            return true;
        }
        match parent_q.get(entity) {
            Ok(parent) => entity = parent.0,
            Err(_) => return false,
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hidden() -> MacroPaletteData {
        MacroPaletteData::default()
    }

    #[test]
    fn default_is_hidden() {
        assert!(!hidden().visible());
        assert_eq!(hidden().row_offset, 0);
    }

    #[test]
    fn active_maps_row_offset_to_positional() {
        let row_offset = CTRL_MACRO_SLOTS;
        let active = Some(12).and_then(|s| {
            (row_offset..row_offset + CTRL_MACRO_SLOTS)
                .contains(&s)
                .then_some(s - row_offset)
        });
        assert_eq!(active, Some(2));
    }
}
