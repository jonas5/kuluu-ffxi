use bevy::prelude::*;

use crate::components::InGameEntity;
use crate::hud::style::{self, theme};

pub const SLOT_LABEL_MAX_CHARS: usize = 10;

#[derive(Component)]
pub struct MacroPaletteRoot;

#[derive(Component)]
pub struct MacroPaletteHeader;

#[derive(Component)]
pub struct MacroPaletteSlot;

#[derive(Component)]
pub struct MacroPaletteActive;

/// The active slot (0-based) whose macro is running, `None` when idle.
/// Written by the kuluu-side provider, consumed by `update_macro_palette`.
#[derive(Resource, Debug, Clone, Default)]
pub struct MacroPaletteData {
    pub header: String,
    pub slots: Vec<String>,
    pub active: Option<usize>,
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
            Node {
                position_type: PositionType::Absolute,
                bottom: Val::Px(4.0),
                left: Val::Percent(50.0),
                margin: UiRect::left(Val::Px(-260.0)),
                width: Val::Px(520.0),
                flex_direction: FlexDirection::Column,
                align_items: AlignItems::Stretch,
                row_gap: Val::Px(2.0),
                display: Display::None,
                ..default()
            },
        ))
        .with_children(|p| {
            p.spawn((
                MacroPaletteHeader,
                Text::new(""),
                style::text_font(11.0),
                TextColor(theme::TITLE),
            ));
            for _ in 0..2 {
                p.spawn(Node {
                    flex_direction: FlexDirection::Row,
                    column_gap: Val::Px(2.0),
                    ..default()
                })
                .with_children(|row| {
                    for _ in 0..10 {
                        row.spawn((
                            MacroPaletteSlot,
                            Text::new(""),
                            style::text_font(10.0),
                            TextColor(theme::TEXT),
                            Node {
                                height: Val::Px(14.0),
                                flex_basis: Val::Px(48.0),
                                flex_grow: 1.0,
                                align_content: AlignContent::Center,
                                border: UiRect::all(Val::Px(1.0)),
                                padding: UiRect::horizontal(Val::Px(3.0)),
                                ..default()
                            },
                            BackgroundColor(theme::CELL_BG),
                            BorderColor::all(theme::CELL_EDGE),
                        ));
                    }
                });
            }
        });
}

#[allow(clippy::type_complexity)]
pub fn update_macro_palette(
    data: Res<MacroPaletteData>,
    mut root_q: Query<&mut Node, (With<MacroPaletteRoot>, Without<MacroPaletteHeader>)>,
    mut header_q: Query<
        &mut Text,
        (With<MacroPaletteHeader>, Without<MacroPaletteSlot>),
    >,
    mut slot_q: Query<
        (&mut Text, &mut BackgroundColor),
        (With<MacroPaletteSlot>, Without<MacroPaletteHeader>),
    >,
    mut active_q: Query<
        (&mut BackgroundColor, &mut BorderColor),
        (
            With<MacroPaletteSlot>,
            Without<Text>,
            Without<MacroPaletteHeader>,
        ),
    >,
) {
    if !data.is_changed() {
        return;
    }
    if let Ok(mut node) = root_q.single_mut() {
        node.display = if data.visible() {
            Display::Flex
        } else {
            Display::None
        };
    }
    if let Ok(mut text) = header_q.single_mut() {
        if **text != data.header {
            **text = data.header.clone();
        }
    }
    let slots = &data.slots;
    for (i, (mut text, mut bg)) in slot_q.iter_mut().enumerate() {
        let label = slots.get(i).cloned().unwrap_or_default();
        if **text != label {
            **text = label;
        }
        let active = data.active == Some(i);
        bg.0 = if active {
            theme::CURSOR_BG
        } else {
            theme::CELL_BG
        };
    }
    let _ = &mut active_q;
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
