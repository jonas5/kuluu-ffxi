use bevy::prelude::*;

use crate::components::{InGameEntity, UiClickSurface};
use crate::hud::style::{self, theme};
use bevy::picking::hover::Hovered;

pub const SLOT_LABEL_MAX_CHARS: usize = 10;

/// Visible palette row: the 10 macros a held modifier step through. Positional
/// index `row_offset + i` is the real 0..20 page slot.
const PALETTE_SLOT_COUNT: usize = 10;

/// The palette is a glanceable overlay, so the slots are roughly four times the
/// editor's cramped cell size (deliberate tuning — retail ships no palette
/// layout file to scrape).
const SLOT_HEIGHT_PX: f32 = 44.0;
const SLOT_FONT_PX: f32 = 24.0;
const SLOT_GAP_PX: f32 = 6.0;

#[derive(Component)]
pub struct MacroPaletteRoot;

#[derive(Component)]
pub struct MacroPaletteHeader;

#[derive(Component)]
pub struct MacroPaletteSlot;

/// Positional index (0..9) of a slot within the visible palette row. The kuluu
/// side uses it with `MacroPaletteData::row_offset` to open the right macro.
#[derive(Component, Debug, Clone, Copy)]
pub struct MacroPaletteSlotIndex(pub usize);

/// The active slot (0-based) whose macro is running, `None` when idle.
/// Written by the kuluu-side provider, consumed by `update_macro_palette`.
#[derive(Resource, Debug, Clone, Default)]
pub struct MacroPaletteData {
    pub header: String,
    /// The visible row's 10 slot labels (positional 0..9), `[]` when hidden.
    pub slots: Vec<String>,
    /// Positional index of the macro currently executing, `None` when idle.
    pub active: Option<usize>,
    /// Offset of the visible row into `MacroPage::macros`: 0 for the Ctrl row,
    /// `CTRL_MACRO_SLOTS` for the Alt row.
    pub row_offset: usize,
}

impl MacroPaletteData {
    pub fn visible(&self) -> bool {
        !self.header.is_empty()
    }
}

/// Palette slot label: the macro's first non-blank line, ASCII-shrunk to
/// `SLOT_LABEL_MAX_CHARS`. Non-ASCII bytes are dropped because the palette
/// renders with Bevy's bundled FiraMono-subset font (UI-text ASCII rule).
pub fn slot_label(macro_lines: &[String; 6]) -> String {
    macro_lines
        .iter()
        .find(|l| !l.trim().is_empty())
        .map(|l| {
            let ascii: String = l
                .chars()
                .filter(|c| matches!(*c, '\u{20}'..='\u{7E}'))
                .take(SLOT_LABEL_MAX_CHARS)
                .collect();
            ascii.trim().to_string()
        })
        .unwrap_or_default()
}

pub fn spawn_macro_palette(mut commands: Commands) {
    commands
        .spawn((
            InGameEntity,
            MacroPaletteRoot,
            UiClickSurface,
            Node {
                position_type: PositionType::Absolute,
                bottom: Val::Px(6.0),
                left: Val::Percent(15.0),
                width: Val::Percent(70.0),
                flex_direction: FlexDirection::Column,
                align_items: AlignItems::Stretch,
                row_gap: Val::Px(2.0),
                display: Display::None,
                padding: UiRect::all(Val::Px(4.0)),
                border: UiRect::all(Val::Px(1.0)),
                border_radius: BorderRadius::all(Val::Px(4.0)),
                ..default()
            },
            BackgroundColor(theme::FRAME_BG),
            BorderColor::all(theme::CELL_EDGE),
        ))
        .with_children(|p| {
            p.spawn((
                MacroPaletteHeader,
                Text::new(""),
                style::text_font(13.0),
                TextColor(theme::TITLE),
            ));
            p.spawn(Node {
                flex_direction: FlexDirection::Row,
                column_gap: Val::Px(SLOT_GAP_PX),
                width: Val::Percent(100.0),
                ..default()
            })
            .with_children(|row| {
                for i in 0..PALETTE_SLOT_COUNT {
                    row.spawn((
                        MacroPaletteSlot,
                        MacroPaletteSlotIndex(i),
                        Hovered::default(),
                        Text::new(""),
                        style::text_font(SLOT_FONT_PX),
                        TextColor(theme::TEXT),
                        Node {
                            height: Val::Px(SLOT_HEIGHT_PX),
                            flex_grow: 1.0,
                            align_content: AlignContent::Center,
                            border: UiRect::all(Val::Px(2.0)),
                            padding: UiRect::horizontal(Val::Px(4.0)),
                            ..default()
                        },
                        BackgroundColor(theme::CELL_BG),
                        BorderColor::all(theme::CELL_EDGE),
                    ));
                }
            });
        });
}

#[allow(clippy::type_complexity)]
pub fn update_macro_palette(
    data: Res<MacroPaletteData>,
    mut root_q: Query<
        &mut Node,
        (
            With<MacroPaletteRoot>,
            Without<MacroPaletteHeader>,
            Without<MacroPaletteSlot>,
        ),
    >,
    mut header_q: Query<
        &mut Text,
        (
            With<MacroPaletteHeader>,
            Without<MacroPaletteRoot>,
            Without<MacroPaletteSlot>,
        ),
    >,
    mut slot_q: Query<
        (&mut Text, &mut BackgroundColor, &Hovered),
        (With<MacroPaletteSlot>, Without<MacroPaletteHeader>),
    >,
) {
    if let Ok(mut node) = root_q.single_mut() {
        let display = if data.visible() {
            Display::Flex
        } else {
            Display::None
        };
        if node.display != display {
            node.display = display;
        }
    }
    if let Ok(mut text) = header_q.single_mut() {
        if **text != data.header {
            **text = data.header.clone();
        }
    }
    // Hover runs per-frame (not gated on `data.is_changed()`), so the hover
    // highlight tracks the pointer even when the macro data itself is idle.
    for (i, (mut text, mut bg, hovered)) in slot_q.iter_mut().enumerate() {
        let label = data.slots.get(i).cloned().unwrap_or_default();
        if **text != label {
            **text = label;
        }
        let color = if hovered.get() {
            theme::CELL_HOVER_BG
        } else if data.active == Some(i) {
            theme::CURSOR_BG
        } else {
            theme::CELL_BG
        };
        if bg.0 != color {
            bg.0 = color;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn macro_lines(lines: [&str; 6]) -> [String; 6] {
        std::array::from_fn(|i| lines[i].into())
    }

    #[test]
    fn empty_macro_has_no_label() {
        assert_eq!(slot_label(&macro_lines(["", "", "", "", "", ""])), "");
    }

    #[test]
    fn label_is_first_non_blank_line() {
        let lines = macro_lines(["", "  /heal mp<me>", "bye", "", "", ""]);
        assert_eq!(slot_label(&lines), "/heal mp");
    }

    #[test]
    fn label_truncates_at_max_chars() {
        let lines = macro_lines(["aaaaaaaaaaaaaaaaaaaa", "", "", "", "", ""]);
        let label = slot_label(&lines);
        assert_eq!(label.len(), SLOT_LABEL_MAX_CHARS);
    }

    #[test]
    fn label_drops_non_ascii() {
        let lines = macro_lines(["/heal \u{2192} \u{00B7}", "", "", "", "", ""]);
        assert_eq!(slot_label(&lines), "/heal");
    }

    #[test]
    fn hidden_when_header_empty() {
        assert!(!MacroPaletteData::default().visible());
    }
}
