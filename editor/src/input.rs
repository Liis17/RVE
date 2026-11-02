//! Перевод ввода окна в ввод скриптов Play-режима.

use engine_script as script;
use engine_window as window;

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
}
