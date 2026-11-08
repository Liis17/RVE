//! Maps backend pad ids to engine slots, keeps raw pad state, and builds snapshots with the dead
//! zone and trigger threshold applied.

use crate::translate::MapperEvent;
use rve_sdk::abi::RVE_GAMEPAD_SLOTS;
use rve_sdk::{GamepadAxis, GamepadButton};
use std::collections::{HashMap, HashSet};

/// Number of gamepad slots, numbered 1..=4 in scripts.
pub const SLOT_COUNT: usize = RVE_GAMEPAD_SLOTS;

/// Stick dead zone and trigger threshold applied when snapshots are built.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct GamepadTuning {
    /// Radial stick dead zone in [0, 1).
    pub stick_deadzone: f32,
    /// Trigger value in (0, 1) above which the trigger counts as a button press (strict `>`).
    pub trigger_threshold: f32,
}

impl Default for GamepadTuning {
    fn default() -> Self {
        Self {
            stick_deadzone: 0.15,
            trigger_threshold: 0.5,
        }
    }
}

/// Processed state of one slot, ready for the script input snapshot.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct PadSnapshot {
    pub connected: bool,
    /// Button bits by [`GamepadButton`] code.
    pub buttons: u32,
    /// Axis values by [`GamepadAxis`] code.
    pub axes: [f32; 6],
}

/// Processed state of every slot, indexed by slot (0 is pad 1).
pub type GamepadSnapshot = [PadSnapshot; SLOT_COUNT];

const TRIGGER_AXES: [GamepadAxis; 2] = [GamepadAxis::LeftTrigger, GamepadAxis::RightTrigger];

#[derive(Clone, Copy, Debug, Default)]
struct RawPad {
    connected: bool,
    buttons: u32,
    /// Buttons pressed since the last snapshot, so a press shorter than one frame still reaches
    /// scripts. Cleared by [`GamepadMapper::snapshot`].
    latched: u32,
    axes: [f32; 6],
}

/// Owns the slot table. Every event goes through here, including connection changes outside
/// focus, so the table stays correct; focus gating happens later, when the runtime uses snapshots.
pub struct GamepadMapper {
    tuning: GamepadTuning,
    slots: [RawPad; SLOT_COUNT],
    slot_of_id: HashMap<usize, usize>,
    /// Pads refused because every slot was taken. Each is reported once until it disconnects.
    rejected: HashSet<usize>,
}

impl GamepadMapper {
    pub fn new(tuning: GamepadTuning) -> Self {
        Self {
            tuning,
            slots: [RawPad::default(); SLOT_COUNT],
            slot_of_id: HashMap::new(),
            rejected: HashSet::new(),
        }
    }

    /// Forgets every pad, slot and refused id. The tuning stays.
    pub fn reset(&mut self) {
        self.slots = [RawPad::default(); SLOT_COUNT];
        self.slot_of_id.clear();
        self.rejected.clear();
    }

    /// Applies one translated event from backend pad `id` and returns the slot it changed.
    /// Events from a pad that is not yet in a slot assign it one first.
    pub fn handle(&mut self, id: usize, event: MapperEvent) -> Option<usize> {
        match event {
            MapperEvent::Connected => self.connect(id),
            MapperEvent::Disconnected => {
                self.rejected.remove(&id);
                let slot = self.slot_of_id.remove(&id)?;
                self.slots[slot] = RawPad::default();
                Some(slot)
            }
            MapperEvent::Button { button, pressed } => {
                let slot = self.slot_for(id)?;
                press(&mut self.slots[slot], button, pressed);
                Some(slot)
            }
            MapperEvent::Axis { axis, value } => {
                let slot = self.slot_for(id)?;
                let raw = &mut self.slots[slot];
                raw.axes[axis as usize] = value;
                if let Some(button) = trigger_button(axis)
                    && value > self.tuning.trigger_threshold
                {
                    set_bit(&mut raw.latched, button, true);
                }
                Some(slot)
            }
            MapperEvent::DPad { horizontal, value } => {
                let slot = self.slot_for(id)?;
                let raw = &mut self.slots[slot];
                if horizontal {
                    press(raw, GamepadButton::DPadLeft, value < -0.5);
                    press(raw, GamepadButton::DPadRight, value > 0.5);
                } else {
                    press(raw, GamepadButton::DPadDown, value < -0.5);
                    press(raw, GamepadButton::DPadUp, value > 0.5);
                }
                Some(slot)
            }
        }
    }

    /// Builds the processed state of every slot, then clears the button latches: a press that
    /// happened since the previous snapshot is reported once, even if it was already released.
    pub fn snapshot(&mut self) -> GamepadSnapshot {
        let snapshot = self.slots.map(|raw| self.process(raw));
        for raw in &mut self.slots {
            raw.latched = 0;
        }
        snapshot
    }

    /// Returns the backend pad id that currently owns `slot`.
    pub fn id_for_slot(&self, slot: usize) -> Option<usize> {
        self.slot_of_id
            .iter()
            .find_map(|(id, owner)| (*owner == slot).then_some(*id))
    }

    fn connect(&mut self, id: usize) -> Option<usize> {
        if let Some(slot) = self.slot_of_id.get(&id) {
            return Some(*slot);
        }
        let Some(slot) = self.slots.iter().position(|raw| !raw.connected) else {
            if self.rejected.insert(id) {
                log::warn!("gamepad {id} ignored: all {SLOT_COUNT} gamepad slots are in use");
            }
            return None;
        };
        self.rejected.remove(&id);
        self.slots[slot] = RawPad {
            connected: true,
            ..RawPad::default()
        };
        self.slot_of_id.insert(id, slot);
        Some(slot)
    }

    fn slot_for(&mut self, id: usize) -> Option<usize> {
        match self.slot_of_id.get(&id) {
            Some(slot) => Some(*slot),
            None => self.connect(id),
        }
    }

    fn process(&self, raw: RawPad) -> PadSnapshot {
        let mut buttons = raw.buttons | raw.latched;
        for (axis, button) in TRIGGER_AXES
            .into_iter()
            .zip([GamepadButton::LeftTrigger, GamepadButton::RightTrigger])
        {
            if raw.axes[axis as usize] > self.tuning.trigger_threshold {
                set_bit(&mut buttons, button, true);
            }
        }

        let mut axes = [0.0; 6];
        let [left_x, left_y] = radial_deadzone(
            raw.axes[GamepadAxis::LeftX as usize],
            raw.axes[GamepadAxis::LeftY as usize],
            self.tuning.stick_deadzone,
        );
        let [right_x, right_y] = radial_deadzone(
            raw.axes[GamepadAxis::RightX as usize],
            raw.axes[GamepadAxis::RightY as usize],
            self.tuning.stick_deadzone,
        );
        axes[GamepadAxis::LeftX as usize] = left_x;
        axes[GamepadAxis::LeftY as usize] = left_y;
        axes[GamepadAxis::RightX as usize] = right_x;
        axes[GamepadAxis::RightY as usize] = right_y;
        for axis in TRIGGER_AXES {
            axes[axis as usize] = raw.axes[axis as usize].clamp(0.0, 1.0);
        }

        PadSnapshot {
            connected: raw.connected,
            buttons,
            axes,
        }
    }
}

/// Sets a digital button from a press or release. A press also latches the button.
fn press(raw: &mut RawPad, button: GamepadButton, pressed: bool) {
    set_bit(&mut raw.buttons, button, pressed);
    if pressed {
        set_bit(&mut raw.latched, button, true);
    }
}

fn trigger_button(axis: GamepadAxis) -> Option<GamepadButton> {
    match axis {
        GamepadAxis::LeftTrigger => Some(GamepadButton::LeftTrigger),
        GamepadAxis::RightTrigger => Some(GamepadButton::RightTrigger),
        _ => None,
    }
}

fn set_bit(bits: &mut u32, button: GamepadButton, on: bool) {
    let bit = 1_u32 << button as u32;
    if on {
        *bits |= bit;
    } else {
        *bits &= !bit;
    }
}

/// Removes the dead zone radially and rescales the remainder to [0, 1]. The output length never
/// exceeds 1, so a diagonal corner does not produce a longer vector than a full push.
fn radial_deadzone(x: f32, y: f32, deadzone: f32) -> [f32; 2] {
    let length = x.hypot(y);
    if length.is_nan() || length <= deadzone {
        return [0.0; 2];
    }
    let scaled = ((length - deadzone) / (1.0 - deadzone)).min(1.0);
    let factor = scaled / length;
    [x * factor, y * factor]
}

#[cfg(test)]
mod tests {
    use super::*;

    const SOUTH: u32 = 1 << GamepadButton::South as u32;

    fn mapper() -> GamepadMapper {
        GamepadMapper::new(GamepadTuning::default())
    }

    fn connect(mapper: &mut GamepadMapper, id: usize) -> Option<usize> {
        mapper.handle(id, MapperEvent::Connected)
    }

    #[test]
    fn connected_pads_take_the_lowest_free_slot_and_release_it_on_disconnect() {
        let mut mapper = mapper();
        assert_eq!(connect(&mut mapper, 10), Some(0));
        assert_eq!(connect(&mut mapper, 20), Some(1));
        assert_eq!(mapper.handle(10, MapperEvent::Disconnected), Some(0));
        assert_eq!(connect(&mut mapper, 30), Some(0));
        assert_eq!(mapper.id_for_slot(1), Some(20));
    }

    #[test]
    fn duplicate_connected_event_does_not_take_a_second_slot() {
        let mut mapper = mapper();
        assert_eq!(connect(&mut mapper, 7), Some(0));
        assert_eq!(connect(&mut mapper, 7), Some(0));
        assert_eq!(connect(&mut mapper, 8), Some(1));
    }

    #[test]
    fn fifth_pad_is_ignored_and_the_first_four_stay_connected() {
        let mut mapper = mapper();
        for id in 0..SLOT_COUNT {
            assert_eq!(connect(&mut mapper, id), Some(id));
        }
        assert_eq!(connect(&mut mapper, 99), None);
        assert!(mapper.snapshot().iter().all(|pad| pad.connected));
        assert_eq!(mapper.id_for_slot(0), Some(0));
    }

    #[test]
    fn disconnect_while_held_releases_buttons_and_zeroes_axes() {
        let mut mapper = mapper();
        connect(&mut mapper, 1);
        mapper.handle(
            1,
            MapperEvent::Button {
                button: GamepadButton::South,
                pressed: true,
            },
        );
        mapper.handle(
            1,
            MapperEvent::Axis {
                axis: GamepadAxis::LeftX,
                value: 0.8,
            },
        );
        assert_eq!(mapper.snapshot()[0].buttons, SOUTH);

        mapper.handle(1, MapperEvent::Disconnected);
        let pad = mapper.snapshot()[0];
        assert_eq!(pad, PadSnapshot::default());
    }

    #[test]
    fn radial_deadzone_removes_small_motion_and_rescales_the_rest() {
        let mut mapper = GamepadMapper::new(GamepadTuning {
            stick_deadzone: 0.2,
            trigger_threshold: 0.5,
        });
        connect(&mut mapper, 1);
        mapper.handle(
            1,
            MapperEvent::Axis {
                axis: GamepadAxis::LeftX,
                value: 0.1,
            },
        );
        mapper.handle(
            1,
            MapperEvent::Axis {
                axis: GamepadAxis::LeftY,
                value: 0.1,
            },
        );
        assert_eq!(mapper.snapshot()[0].axes[GamepadAxis::LeftX as usize], 0.0);

        mapper.handle(
            1,
            MapperEvent::Axis {
                axis: GamepadAxis::LeftX,
                value: 0.6,
            },
        );
        mapper.handle(
            1,
            MapperEvent::Axis {
                axis: GamepadAxis::LeftY,
                value: 0.0,
            },
        );
        let x = mapper.snapshot()[0].axes[GamepadAxis::LeftX as usize];
        assert!((x - 0.5).abs() < 1e-6, "{x}");
    }

    #[test]
    fn diagonal_corner_stick_length_stays_within_one() {
        let mut mapper = mapper();
        connect(&mut mapper, 1);
        for axis in [GamepadAxis::RightX, GamepadAxis::RightY] {
            mapper.handle(1, MapperEvent::Axis { axis, value: 1.0 });
        }
        let pad = mapper.snapshot()[0];
        let length =
            pad.axes[GamepadAxis::RightX as usize].hypot(pad.axes[GamepadAxis::RightY as usize]);
        assert!((length - 1.0).abs() < 1e-6, "{length}");
    }

    #[test]
    fn trigger_threshold_is_strict_and_drives_the_trigger_button_bit() {
        let mut mapper = mapper();
        connect(&mut mapper, 1);
        let trigger_bit = 1_u32 << GamepadButton::LeftTrigger as u32;

        mapper.handle(
            1,
            MapperEvent::Axis {
                axis: GamepadAxis::LeftTrigger,
                value: 0.5,
            },
        );
        assert_eq!(mapper.snapshot()[0].buttons & trigger_bit, 0);
        assert_eq!(
            mapper.snapshot()[0].axes[GamepadAxis::LeftTrigger as usize],
            0.5
        );

        mapper.handle(
            1,
            MapperEvent::Axis {
                axis: GamepadAxis::LeftTrigger,
                value: 0.51,
            },
        );
        assert_ne!(mapper.snapshot()[0].buttons & trigger_bit, 0);
    }

    #[test]
    fn bumpers_and_dpad_axes_drive_their_button_bits() {
        let mut mapper = mapper();
        connect(&mut mapper, 1);
        let left = 1_u32 << GamepadButton::DPadLeft as u32;
        let right = 1_u32 << GamepadButton::DPadRight as u32;
        let bumper = 1_u32 << GamepadButton::LeftBumper as u32;

        mapper.handle(
            1,
            MapperEvent::Button {
                button: GamepadButton::LeftBumper,
                pressed: true,
            },
        );
        mapper.handle(
            1,
            MapperEvent::DPad {
                horizontal: true,
                value: -1.0,
            },
        );
        let buttons = mapper.snapshot()[0].buttons;
        assert_eq!(buttons & (bumper | left), bumper | left);
        assert_eq!(buttons & right, 0);

        mapper.handle(
            1,
            MapperEvent::DPad {
                horizontal: true,
                value: 0.0,
            },
        );
        assert_eq!(mapper.snapshot()[0].buttons & left, 0);
    }

    #[test]
    fn tap_between_two_snapshots_is_reported_once_then_released() {
        let mut mapper = mapper();
        connect(&mut mapper, 1);
        mapper.handle(
            1,
            MapperEvent::Button {
                button: GamepadButton::South,
                pressed: true,
            },
        );
        mapper.handle(
            1,
            MapperEvent::Button {
                button: GamepadButton::South,
                pressed: false,
            },
        );
        assert_eq!(mapper.snapshot()[0].buttons, SOUTH);
        assert_eq!(mapper.snapshot()[0].buttons, 0);
    }

    #[test]
    fn trigger_tap_above_threshold_is_latched_until_the_next_snapshot() {
        let mut mapper = mapper();
        connect(&mut mapper, 1);
        let trigger_bit = 1_u32 << GamepadButton::LeftTrigger as u32;
        for value in [0.9, 0.0] {
            mapper.handle(
                1,
                MapperEvent::Axis {
                    axis: GamepadAxis::LeftTrigger,
                    value,
                },
            );
        }
        assert_ne!(mapper.snapshot()[0].buttons & trigger_bit, 0);
        assert_eq!(mapper.snapshot()[0].buttons & trigger_bit, 0);
    }

    #[test]
    fn refused_fifth_pad_is_remembered_once_until_it_disconnects() {
        let mut mapper = mapper();
        for id in 0..SLOT_COUNT {
            connect(&mut mapper, id);
        }
        assert_eq!(connect(&mut mapper, 99), None);
        assert_eq!(connect(&mut mapper, 99), None);
        assert_eq!(mapper.rejected.len(), 1);
        assert!(mapper.rejected.contains(&99));

        mapper.handle(99, MapperEvent::Disconnected);
        assert!(mapper.rejected.is_empty());
    }
}
