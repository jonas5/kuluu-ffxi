use std::collections::VecDeque;

use bevy::input::keyboard::KeyCode;
use bevy::input::ButtonInput;
use bevy::prelude::*;
use tokio::sync::mpsc::Sender;

use crate::macro_store::MacroBooks;
use crate::view_native::input::CommandTx;
use crate::view_native::slash_commands::{parse_slash, system_chat_line, SlashOutcome};
use kuluu_render::fishing_spot::FishingSpot;
use kuluu_render::{ActiveMacroPage, InputMode, MacroBook, SceneState};
use kuluu_session::state::AgentCommand;

#[derive(Resource, Debug, Default)]
pub struct MacroSequencer {
    pending: VecDeque<QueuedLine>,
    wait_remaining: f32,
    armed: bool,
    /// 0-based slot currently executing (highlighted in the palette),
    /// `None` when idle.
    pub active_slot: Option<usize>,
}

#[derive(Debug, Clone)]
enum QueuedLine {
    Chat(String),
    Wait(f32),
}

impl MacroSequencer {
    pub fn is_busy(&self) -> bool {
        self.armed || !self.pending.is_empty()
    }

    pub fn clear(&mut self) {
        self.pending.clear();
        self.wait_remaining = 0.0;
        self.armed = false;
        self.active_slot = None;
    }
}

pub fn parse_wait_delay(line: &str) -> Option<f32> {
    let rest = line
        .strip_prefix("/wait")
        .or_else(|| line.strip_prefix("/Wait"))?;
    let rest = rest.trim();
    let delay = rest.parse::<f32>().ok()?;
    Some(delay).filter(|d| d.is_finite() && *d > 0.0)
}

/// Queue the lines of `slot` on `page` of `book` into the sequencer,
/// interrupting any in-flight sequence (retail: firing a macro cancels the
/// running one).
pub fn fire_macro(slot: usize, page: usize, book: &MacroBook, sequencer: &mut MacroSequencer) {
    let Some(m) = book.pages.get(page).and_then(|p| p.macros.get(slot)) else {
        return;
    };
    if m.is_empty() {
        return;
    }

    sequencer.clear();
    sequencer.active_slot = Some(slot);
    for line in &m.lines {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if let Some(delay) = parse_wait_delay(trimmed) {
            sequencer.pending.push_back(QueuedLine::Wait(delay));
        } else {
            sequencer
                .pending
                .push_back(QueuedLine::Chat(trimmed.to_string()));
        }
    }
    sequencer.armed = true;
}

pub fn macro_hotkey_system(
    mode: Res<InputMode>,
    keys: Res<ButtonInput<KeyCode>>,
    state: Res<SceneState>,
    mut macro_books: ResMut<MacroBooks>,
    mut macro_sequencer: ResMut<MacroSequencer>,
    active_macro_page: Res<ActiveMacroPage>,
) {
    if !matches!(*mode, InputMode::World) {
        return;
    }

    let ctrl_held = keys.pressed(KeyCode::ControlLeft) || keys.pressed(KeyCode::ControlRight);
    let alt_held = keys.pressed(KeyCode::AltLeft) || keys.pressed(KeyCode::AltRight);
    if !ctrl_held && !alt_held {
        return;
    }

    let digit = if keys.just_pressed(KeyCode::Digit1) {
        Some(1u8)
    } else if keys.just_pressed(KeyCode::Digit2) {
        Some(2u8)
    } else if keys.just_pressed(KeyCode::Digit3) {
        Some(3u8)
    } else if keys.just_pressed(KeyCode::Digit4) {
        Some(4u8)
    } else if keys.just_pressed(KeyCode::Digit5) {
        Some(5u8)
    } else if keys.just_pressed(KeyCode::Digit6) {
        Some(6u8)
    } else if keys.just_pressed(KeyCode::Digit7) {
        Some(7u8)
    } else if keys.just_pressed(KeyCode::Digit8) {
        Some(8u8)
    } else if keys.just_pressed(KeyCode::Digit9) {
        Some(9u8)
    } else if keys.just_pressed(KeyCode::Digit0) {
        Some(0u8)
    } else {
        None
    };
    let Some(digit) = digit else {
        return;
    };
    let Some(slot) =
        kuluu_render::hud::macros::slot_from_modifier_digit(ctrl_held, alt_held, digit)
    else {
        return;
    };

    let key = crate::macro_store::char_key(state.snapshot.self_char_id);
    let books = macro_books.get_or_default(&key);
    let Some(book) = books.books.get(active_macro_page.book) else {
        return;
    };
    fire_macro(slot, active_macro_page.page, book, &mut macro_sequencer);
}

pub fn macro_step_system(
    time: Res<Time>,
    mut sequencer: ResMut<MacroSequencer>,
    mut scene_state: ResMut<SceneState>,
    cmd_tx: Res<CommandTx>,
    target: Res<kuluu_render::Target>,
    fishing_spot: Res<FishingSpot>,
) {
    if sequencer.pending.is_empty() && !sequencer.armed {
        return;
    }

    sequencer.armed = false;

    if sequencer.wait_remaining > 0.0 {
        sequencer.wait_remaining -= time.delta_secs();
        if sequencer.wait_remaining > 0.0 {
            return;
        }
        sequencer.wait_remaining = 0.0;
    }

    let Some(queued) = sequencer.pending.pop_front() else {
        return;
    };

    match queued {
        QueuedLine::Wait(delay) => {
            sequencer.wait_remaining = delay;
        }
        QueuedLine::Chat(text) => {
            let snapshot = &scene_state.snapshot;
            let entities = snapshot.entities.clone();
            if text.starts_with('/') {
                let outcome = parse_slash(
                    &text,
                    &entities,
                    snapshot.self_pos.pos,
                    target.id,
                    snapshot.zone_id,
                    snapshot.self_char_id,
                    &snapshot.party,
                    snapshot.myroom,
                    fishing_spot.0,
                );
                apply_outcome(outcome, &cmd_tx.0, &mut scene_state);
            } else {
                send(
                    &cmd_tx.0,
                    &mut scene_state,
                    AgentCommand::Chat {
                        kind: 0,
                        text: text.clone(),
                    },
                );
            }
        }
    }

    if sequencer.pending.is_empty() && sequencer.wait_remaining <= 0.0 {
        sequencer.active_slot = None;
    }
}

fn apply_outcome(
    outcome: SlashOutcome,
    cmd_tx: &Sender<AgentCommand>,
    scene_state: &mut SceneState,
) {
    match outcome {
        SlashOutcome::Command(cmd) => send(cmd_tx, scene_state, cmd),
        SlashOutcome::Commands(cmds) => {
            for cmd in cmds {
                send(cmd_tx, scene_state, cmd);
            }
        }
        SlashOutcome::CommandWithNotice { cmd, notice } => {
            scene_state.push_local_toast(system_chat_line(notice));
            send(cmd_tx, scene_state, cmd);
        }
        SlashOutcome::SystemMessage(msg) => {
            scene_state.push_local_toast(system_chat_line(msg));
        }
        _ => {
            scene_state.push_local_toast(system_chat_line(
                "[macro] command not supported in macros".into(),
            ));
        }
    }
}

fn send(cmd_tx: &Sender<AgentCommand>, scene_state: &mut SceneState, cmd: AgentCommand) {
    if let Err(e) = cmd_tx.try_send(cmd) {
        scene_state.push_local_toast(system_chat_line(format!("[macro] dispatch dropped: {e}")));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kuluu_render::hud::macros::default_macro_book;

    #[test]
    fn parse_wait_delay_recognises_integer() {
        assert_eq!(parse_wait_delay("/wait 3"), Some(3.0));
    }

    #[test]
    fn parse_wait_delay_recognises_float() {
        assert_eq!(parse_wait_delay("/wait 2.5"), Some(2.5));
    }

    #[test]
    fn parse_wait_delay_rejects_zero() {
        assert_eq!(parse_wait_delay("/wait 0"), None);
    }

    #[test]
    fn parse_wait_delay_rejects_negative() {
        assert_eq!(parse_wait_delay("/wait -1"), None);
    }

    #[test]
    fn parse_wait_delay_rejects_non_numeric() {
        assert_eq!(parse_wait_delay("/wait abc"), None);
    }

    #[test]
    fn fire_macro_skips_empty_slot() {
        let book = default_macro_book(0);
        let mut seq = MacroSequencer::default();
        fire_macro(0, 0, &book, &mut seq);
        assert!(!seq.is_busy());
    }

    #[test]
    fn fire_macro_queues_lines_and_waits() {
        let mut book = default_macro_book(0);
        book.pages[0].macros[0].lines = [
            "/heal".into(),
            "/wait 2".into(),
            "hello".into(),
            "".into(),
            "".into(),
            "".into(),
        ];
        let mut seq = MacroSequencer::default();
        fire_macro(0, 0, &book, &mut seq);
        assert!(seq.is_busy());
        assert_eq!(seq.pending.len(), 3);
    }

    #[test]
    fn fire_macro_interrupts_running_sequence() {
        let mut book = default_macro_book(0);
        book.pages[0].macros[0].lines[0] = "/heal".into();
        book.pages[0].macros[1].lines[0] = "/attack".into();
        let mut seq = MacroSequencer::default();
        fire_macro(0, 0, &book, &mut seq);
        assert!(seq.is_busy());
        fire_macro(1, 0, &book, &mut seq);
        assert_eq!(seq.pending.len(), 1);
    }

    #[test]
    fn sequencer_clears_pending() {
        let mut seq = MacroSequencer::default();
        seq.pending.push_back(QueuedLine::Chat("test".into()));
        seq.armed = true;
        seq.clear();
        assert!(!seq.is_busy());
    }
}
