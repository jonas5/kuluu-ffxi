use bevy::prelude::*;

use crate::macro_store::{char_key, MacroBooks};
use crate::view_native::text_input::macro_exec::MacroSequencer;
use kuluu_render::hud::macro_palette::{slot_label, MacroPaletteData};
use kuluu_render::{ActiveMacroPage, SceneState};

/// Populate `MacroPaletteData` from the character's books for the active
/// book/page. Runs before the render-side palette updater so freshly edited
/// macros appear same-frame.
pub fn macro_palette_provider_system(
    state: Res<SceneState>,
    active_page: Res<ActiveMacroPage>,
    mut macro_books: ResMut<MacroBooks>,
    sequencer: Res<MacroSequencer>,
    mut palette: ResMut<MacroPaletteData>,
) {
    let Some(char_id) = state.snapshot.self_char_id else {
        if !palette.header.is_empty() {
            *palette = MacroPaletteData::default();
        }
        return;
    };

    let books = macro_books.get_or_default(&char_key(Some(char_id)));
    let Some(book) = books.books.get(active_page.book) else {
        return;
    };
    let Some(page) = book.pages.get(active_page.page) else {
        return;
    };

    let header = format!(
        "Book {} / Page {}",
        active_page.book + 1,
        active_page.page + 1
    );
    let slots: Vec<String> = page.macros.iter().map(|m| slot_label(&m.lines)).collect();
    let active = sequencer
        .active_slot
        .filter(|_| sequencer.is_busy())
        .filter(|s| slots.get(*s).is_none_or(|l| !l.is_empty()));

    if palette.header != header || palette.slots != slots || palette.active != active {
        *palette = MacroPaletteData {
            header,
            slots,
            active,
        };
    }
}
