use bevy::prelude::*;

use crate::components::InGameEntity;
use crate::hud::macros::MACRO_LINES;
use crate::hud::style::{self, theme};

pub const MACRO_EDITOR_COLUMNS: usize = 10;
pub const MACRO_EDITOR_ROWS: usize = 2;

#[derive(Component)]
pub struct MacroEditorRoot;

#[derive(Component)]
pub struct MacroEditorHeader;

#[derive(Component)]
pub struct MacroEditorSlot;

#[derive(Component)]
pub struct MacroEditorLine;

/// Focus state for the bespoke macro-page editor (slot nav + one-line draft
/// entry). Lives in kuluu-render so both the input handler (kuluu) and the
/// HUD systems share one source of truth, like `MapScreenState`.
#[derive(Resource, Debug, Clone, Default)]
pub struct MacroEditorState {
    /// 0-based focused slot (row-major 2 x 10 grid, 0..20).
    pub slot: usize,

    /// 0..6 line cursor within the focused slot while editing.
    pub line: usize,

    /// Draft text being typed over `lines[line]` while editing; committed to
    /// the character's books on Enter/Backspace-out/Esc.
    pub draft: String,

    /// Line-edit submode active; slot navigation otherwise.
    pub editing: bool,
}

impl MacroEditorState {
    /// Return to slot navigation on the first slot, discarding any draft.
    pub fn reset(&mut self) {
        *self = Self::default();
    }
}

/// Page content snapshot written each frame by the kuluu-side provider while
/// the MacroPage editor is the top menu level, consumed by
/// `update_macro_editor`. Empty when the editor is closed.
#[derive(Resource, Debug, Clone, Default)]
pub struct MacroEditorData {
    pub book: usize,
    pub page: usize,
    pub slot_labels: Vec<String>,
    /// All 20 macros' lines, the focused line overlaid with `state.draft`
    /// while editing.
    pub lines: Vec<[String; MACRO_LINES]>,
    pub state: MacroEditorState,
}

impl MacroEditorData {
    pub fn visible(&self) -> bool {
        !self.slot_labels.is_empty()
    }
}

pub fn spawn_macro_editor(mut commands: Commands) {
    commands
        .spawn((
            InGameEntity,
            MacroEditorRoot,
            Node {
                position_type: PositionType::Absolute,
                top: Val::Percent(30.0),
                left: Val::Percent(50.0),
                margin: UiRect::left(Val::Px(-280.0)),
                width: Val::Px(560.0),
                flex_direction: FlexDirection::Column,
                align_items: AlignItems::Stretch,
                row_gap: Val::Px(4.0),
                padding: UiRect::all(Val::Px(8.0)),
                border: UiRect::all(Val::Px(1.0)),
                display: Display::None,
                ..default()
            },
            BackgroundColor(theme::FRAME_BG),
            BorderColor::all(theme::CELL_EDGE),
        ))
        .with_children(|p| {
            p.spawn((
                MacroEditorHeader,
                Text::new(""),
                style::text_font(12.0),
                TextColor(theme::TITLE),
            ));
            for _ in 0..MACRO_EDITOR_ROWS {
                p.spawn(Node {
                    flex_direction: FlexDirection::Row,
                    column_gap: Val::Px(2.0),
                    ..default()
                })
                .with_children(|row| {
                    for _ in 0..MACRO_EDITOR_COLUMNS {
                        row.spawn((
                            MacroEditorSlot,
                            Text::new(""),
                            style::text_font(10.0),
                            TextColor(theme::TEXT),
                            Node {
                                height: Val::Px(14.0),
                                flex_basis: Val::Px(50.0),
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
            for _ in 0..MACRO_LINES {
                p.spawn((
                    MacroEditorLine,
                    Text::new(""),
                    style::text_font(10.0),
                    TextColor(theme::TEXT),
                    Node {
                        height: Val::Px(14.0),
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

#[allow(clippy::type_complexity)]
pub fn update_macro_editor(
    data: Res<MacroEditorData>,
    mut root_q: Query<&mut Node, (With<MacroEditorRoot>, Without<MacroEditorLine>)>,
    mut header_q: Query<
        &mut Text,
        (
            With<MacroEditorHeader>,
            Without<MacroEditorSlot>,
            Without<MacroEditorLine>,
        ),
    >,
    mut slot_q: Query<
        (&mut Text, &mut BackgroundColor),
        (
            With<MacroEditorSlot>,
            Without<MacroEditorLine>,
            Without<MacroEditorHeader>,
        ),
    >,
    mut line_q: Query<
        (&mut Text, &mut BackgroundColor),
        (
            With<MacroEditorLine>,
            Without<MacroEditorSlot>,
            Without<MacroEditorHeader>,
        ),
    >,
) {
    let Ok(mut root) = root_q.single_mut() else {
        return;
    };
    if !data.visible() {
        if root.display != Display::None {
            root.display = Display::None;
        }
        return;
    }
    if root.display != Display::Flex {
        root.display = Display::Flex;
    }
    if let Ok(mut text) = header_q.single_mut() {
        let header = format!(
            "Book {} / Page {} - slot {}: row {}, col {}",
            data.book + 1,
            data.page + 1,
            data.state.slot + 1,
            data.state.slot / 10 + 1,
            data.state.slot % 10 + 1,
        );
        if **text != header {
            **text = header;
        }
    }
    for (i, (mut text, mut bg)) in slot_q.iter_mut().enumerate() {
        let label = data.slot_labels.get(i).cloned().unwrap_or_default();
        if **text != label {
            **text = label;
        }
        bg.0 = if i == data.state.slot {
            theme::CURSOR_BG
        } else {
            theme::CELL_BG
        };
    }
    for (i, (mut text, mut bg)) in line_q.iter_mut().enumerate() {
        let focused = data.state.editing && i == data.state.line;
        let text_val = if focused {
            data.state.draft.clone()
        } else {
            data.lines
                .get(data.state.slot)
                .and_then(|lines| lines.get(i))
                .cloned()
                .unwrap_or_default()
        };
        if **text != text_val {
            **text = text_val;
        }
        bg.0 = if focused {
            theme::CURSOR_BG
        } else {
            theme::CELL_BG
        };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hidden_when_no_slots() {
        assert!(!MacroEditorData::default().visible());
    }

    #[test]
    fn visible_when_slots_present() {
        let data = MacroEditorData {
            slot_labels: vec!["a".into()],
            ..Default::default()
        };
        assert!(data.visible());
    }
}
