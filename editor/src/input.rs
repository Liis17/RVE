//! Перевод ввода окна в ввод скриптов Play-режима.

/// Pending native events can move the pointer before the next egui frame updates hover flags.
/// PointerGone must suppress the previous frame's position too.
pub fn latest_pointer(events: &[egui::Event], previous: Option<egui::Pos2>) -> Option<egui::Pos2> {
    events
        .iter()
        .rev()
        .find_map(|event| match event {
            egui::Event::PointerMoved(point) => Some(Some(*point)),
            egui::Event::PointerGone => Some(None),
            _ => None,
        })
        .unwrap_or(previous)
}

use engine_gui::input::TextInputEvent;
use engine_script as script;
use engine_window as window;

/// Text-edit events for the game UI. The editor only forwards them while Game owns keyboard focus.
pub fn ui_text_events(events: &[egui::Event], accepting: bool) -> Vec<TextInputEvent> {
    if !accepting {
        return Vec::new();
    }
    let mut out = Vec::new();
    for (index, event) in events.iter().enumerate() {
        match event {
            egui::Event::Text(text) if !text.is_empty() => {
                // egui integrations may expose a committed IME string through both event forms.
                let duplicated_commit = events.get(index + 1).is_some_and(|next| matches!(next, egui::Event::Ime(egui::ImeEvent::Commit(commit)) if commit == text))
                    || (index > 0 && matches!(&events[index - 1], egui::Event::Ime(egui::ImeEvent::Commit(commit)) if commit == text));
                if !duplicated_commit {
                    out.push(TextInputEvent::Text(text.clone()));
                }
            }
            egui::Event::Ime(egui::ImeEvent::Commit(text)) if !text.is_empty() => {
                out.push(TextInputEvent::Text(text.clone()));
            }
            egui::Event::Key {
                key, pressed: true, ..
            } => match key {
                egui::Key::Backspace => out.push(TextInputEvent::Backspace),
                egui::Key::Delete => out.push(TextInputEvent::Delete),
                egui::Key::ArrowLeft => out.push(TextInputEvent::Left),
                egui::Key::ArrowRight => out.push(TextInputEvent::Right),
                egui::Key::Home => out.push(TextInputEvent::Home),
                egui::Key::End => out.push(TextInputEvent::End),
                egui::Key::Enter => out.push(TextInputEvent::Enter),
                _ => {}
            },
            _ => {}
        }
    }
    out
}

macro_rules! map_enum {
    ($function:ident, $window:ty, $script:ty, [$($variant:ident),* $(,)?]) => {
        pub fn $function(value: $window) -> $script {
            match value {
                $(<$window>::$variant => <$script>::$variant,)*
            }
        }
    };
}

map_enum!(
    map_key,
    window::Key,
    script::Key,
    [
        A,
        B,
        C,
        D,
        E,
        F,
        G,
        H,
        I,
        J,
        K,
        L,
        M,
        N,
        O,
        P,
        Q,
        R,
        S,
        T,
        U,
        V,
        W,
        X,
        Y,
        Z,
        Digit0,
        Digit1,
        Digit2,
        Digit3,
        Digit4,
        Digit5,
        Digit6,
        Digit7,
        Digit8,
        Digit9,
        F1,
        F2,
        F3,
        F4,
        F5,
        F6,
        F7,
        F8,
        F9,
        F10,
        F11,
        F12,
        ArrowUp,
        ArrowDown,
        ArrowLeft,
        ArrowRight,
        Space,
        Enter,
        Escape,
        Tab,
        Backspace,
        ShiftLeft,
        ShiftRight,
        ControlLeft,
        ControlRight,
        AltLeft,
        AltRight,
        SuperLeft,
        SuperRight,
    ]
);

map_enum!(
    map_mouse_button,
    window::MouseButton,
    script::MouseButton,
    [Left, Right, Middle, Back, Forward]
);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_map_to_the_same_abi_code() {
        for (key, code) in [
            (window::Key::A, 0),
            (window::Key::Digit0, 26),
            (window::Key::F12, 47),
            (window::Key::Space, 52),
            (window::Key::SuperRight, 64),
        ] {
            assert_eq!(map_key(key) as u32, code);
        }
    }

    #[test]
    fn mouse_buttons_map_to_the_same_abi_code() {
        assert_eq!(map_mouse_button(window::MouseButton::Left) as u32, 0);
        assert_eq!(map_mouse_button(window::MouseButton::Forward) as u32, 4);
    }

    #[test]
    fn game_text_events_are_focus_gated_and_ime_commit_is_not_duplicated() {
        let events = vec![
            egui::Event::Text("é".into()),
            egui::Event::Ime(egui::ImeEvent::Commit("é".into())),
            egui::Event::Key {
                key: egui::Key::Backspace,
                physical_key: None,
                pressed: true,
                repeat: true,
                modifiers: egui::Modifiers::NONE,
            },
        ];
        assert!(ui_text_events(&events, false).is_empty());
        assert_eq!(
            ui_text_events(&events, true),
            [
                engine_gui::input::TextInputEvent::Text("é".into()),
                engine_gui::input::TextInputEvent::Backspace,
            ]
        );
    }
}

#[cfg(test)]
mod pointer_tests {
    use super::latest_pointer;
    use egui::{Event, pos2};
    #[test]
    fn pending_motion_changes_click_routing_before_a_frame_and_exit_clears_it() {
        let game = egui::Rect::from_min_max(pos2(100.0, 100.0), pos2(600.0, 500.0));
        let toolbar = Some(pos2(200.0, 20.0));
        let moved = [Event::PointerMoved(pos2(250.0, 150.0))];
        assert!(latest_pointer(&moved, toolbar).is_some_and(|p| game.contains(p)));
        assert!(
            !latest_pointer(
                &[Event::PointerMoved(pos2(200.0, 20.0))],
                Some(pos2(250.0, 150.0))
            )
            .is_some_and(|p| game.contains(p))
        );
        assert_eq!(
            latest_pointer(&[moved[0].clone(), Event::PointerGone], toolbar),
            None
        );
        assert_eq!(latest_pointer(&[], toolbar), toolbar);
    }
}
