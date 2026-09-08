use bevy::prelude::*;

use crate::components::InGameEntity;
use crate::hud::macros::MACRO_LINES;
use crate::hud::style::{self, theme};

pub const MACRO_EDITOR_COLUMNS: usize = 10;

/// Every editor dimension derives from this: the macro edit window is a
/// low-stress text surface, so it renders 2.5x the base cell (deliberate
/// tuning — retail ships no macro-editor layout file to scrape).
const EDITOR_SCALE: f32 = 2.5;
const EDITOR_WIDTH_100PX: f32 = 560.0;
const EDITOR_PAD_PX: f32 = 8.0;
const EDITOR_GAP_PX: f32 = 4.0;
const EDITOR_CELL_HEIGHT_PX: f32 = 14.0;
const EDITOR_CELL_BASIS_PX: f32 = 50.0;
const EDITOR_CELL_GAP_PX: f32 = 2.0;
const EDITOR_HEADER_FONT_PX: f32 = 12.0;
const EDITOR_CELL_FONT_PX: f32 = 10.0;

/// The editing caret ("_", retail's macro-text cursor) suffixed to the focused
/// draft while line entry is active.
const MACRO_CARET: &str = "_";

fn scaled_px(base: f32) -> f32 {
    base * EDITOR_SCALE
}

#[derive(Component)]
pub struct MacroEditorRoot;

#[derive(Component)]
pub struct MacroEditorHeader;

#[derive(Component)]
pub struct MacroEditorSlot;

/// Positional index (0..9) of a slot within the visible editor row. The click
/// system on the kuluu side uses it to resolve which slot was clicked.
#[derive(Component, Debug, Clone, Copy)]
pub struct MacroEditorSlotIndex(pub usize);

#[derive(Component)]
pub struct MacroEditorLine;

/// Positional index (0..5) of a line row within the focused macro's six lines.
#[derive(Component, Debug, Clone, Copy)]
pub struct MacroEditorLineIndex(pub usize);

/// Focus state for the bespoke macro-page editor (slot nav + one-line draft
/// entry). Lives in kuluu-render so both the input handler (kuluu) and the
/// HUD systems share one source of truth, like `MapScreenState`.
#[derive(Resource, Debug, Clone, Default)]
pub struct MacroEditorState {
    /// 0-based focused slot, absolute within the page (rows x 10: Ctrl row
    /// 0..9, Alt row 10..19).
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
                margin: UiRect::left(Val::Px(-scaled_px(EDITOR_WIDTH_100PX) / 2.0)),
                width: Val::Px(scaled_px(EDITOR_WIDTH_100PX)),
                flex_direction: FlexDirection::Column,
                align_items: AlignItems::Stretch,
                row_gap: Val::Px(scaled_px(EDITOR_GAP_PX)),
                padding: UiRect::all(Val::Px(scaled_px(EDITOR_PAD_PX))),
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
                style::text_font(scaled_px(EDITOR_HEADER_FONT_PX)),
                TextColor(theme::TITLE),
            ));
            p.spawn(Node {
                flex_direction: FlexDirection::Row,
                column_gap: Val::Px(scaled_px(EDITOR_CELL_GAP_PX)),
                ..default()
            })
            .with_children(|row| {
                for i in 0..MACRO_EDITOR_COLUMNS {
                    row.spawn((
                        MacroEditorSlot,
                        MacroEditorSlotIndex(i),
                        Text::new(""),
                        style::text_font(scaled_px(EDITOR_CELL_FONT_PX)),
                        TextColor(theme::TEXT),
                        Node {
                            height: Val::Px(scaled_px(EDITOR_CELL_HEIGHT_PX)),
                            flex_basis: Val::Px(scaled_px(EDITOR_CELL_BASIS_PX)),
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
            for i in 0..MACRO_LINES {
                p.spawn((
                    MacroEditorLine,
                    MacroEditorLineIndex(i),
                    Text::new(""),
                    style::text_font(scaled_px(EDITOR_CELL_FONT_PX)),
                    TextColor(theme::TEXT),
                    Node {
                        height: Val::Px(scaled_px(EDITOR_CELL_HEIGHT_PX)),
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
    let row_base = (data.state.slot / MACRO_EDITOR_COLUMNS) * MACRO_EDITOR_COLUMNS;
    let col = data.state.slot % MACRO_EDITOR_COLUMNS;
    if let Ok(mut text) = header_q.single_mut() {
        let modifier = if row_base == 0 { "Ctrl" } else { "Alt" };
        let key = if col == MACRO_EDITOR_COLUMNS - 1 {
            0
        } else {
            col + 1
        };
        let header = format!(
            "Book {} / Page {} - {} macro {}",
            data.book + 1,
            data.page + 1,
            modifier,
            key,
        );
        if **text != header {
            **text = header;
        }
    }
    for (i, (mut text, mut bg)) in slot_q.iter_mut().enumerate() {
        let label = data
            .slot_labels
            .get(row_base + i)
            .cloned()
            .unwrap_or_default();
        if **text != label {
            **text = label;
        }
        bg.0 = if i == col {
            theme::CURSOR_BG
        } else {
            theme::CELL_BG
        };
    }
    for (i, (mut text, mut bg)) in line_q.iter_mut().enumerate() {
        let focused = data.state.editing && i == data.state.line;
        let text_val = if focused {
            format!("{}{}", data.state.draft, MACRO_CARET)
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
