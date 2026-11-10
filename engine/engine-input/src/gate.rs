//! Focus gate for pad snapshots, shared by the runtime and the editor so both release pads alike.

use crate::mapper::{GamepadSnapshot, PadSnapshot, SLOT_COUNT};

/// Keeps pad input out of scripts while input is not accepted, and masks buttons that were held
/// when input was regained until they are released.
#[derive(Clone, Copy, Debug, Default)]
pub struct PadGate {
    masks: [u32; SLOT_COUNT],
    accepting: bool,
}

impl PadGate {
    /// Returns the snapshot to hand to scripts. Outside accepted input every pad keeps its
    /// connection state but reports no buttons and zero axes.
    pub fn apply(&mut self, accepting: bool, pads: &GamepadSnapshot) -> GamepadSnapshot {
        if accepting && !self.accepting {
            self.masks = std::array::from_fn(|slot| pads[slot].buttons);
        }
        self.accepting = accepting;
        std::array::from_fn(|slot| {
            let pad = pads[slot];
            if !accepting {
                return PadSnapshot {
                    connected: pad.connected,
                    ..PadSnapshot::default()
                };
            }
            self.masks[slot] &= pad.buttons;
            PadSnapshot {
                buttons: pad.buttons & !self.masks[slot],
                ..pad
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SOUTH: u32 = 1;

    fn pads(buttons: u32) -> GamepadSnapshot {
        let mut pads = [PadSnapshot::default(); SLOT_COUNT];
        pads[0] = PadSnapshot {
            connected: true,
            buttons,
            axes: [0.5; 6],
        };
        pads
    }

    #[test]
    fn unaccepted_input_reports_connected_pads_without_buttons_or_axes() {
        let mut gate = PadGate::default();
        let out = gate.apply(false, &pads(SOUTH));
        assert_eq!(
            out[0],
            PadSnapshot {
                connected: true,
                ..PadSnapshot::default()
            }
        );
    }

    #[test]
    fn buttons_held_when_input_is_regained_stay_masked_until_released() {
        let mut gate = PadGate::default();
        assert_eq!(gate.apply(true, &pads(0))[0].buttons, 0);
        assert_eq!(gate.apply(true, &pads(SOUTH))[0].buttons, SOUTH);

        gate.apply(false, &pads(SOUTH));
        assert_eq!(gate.apply(true, &pads(SOUTH))[0].buttons, 0);
        assert_eq!(gate.apply(true, &pads(0))[0].buttons, 0);
        assert_eq!(gate.apply(true, &pads(SOUTH))[0].buttons, SOUTH);
    }

    #[test]
    fn button_released_while_unaccepted_does_not_stick_after_regaining_input() {
        let mut gate = PadGate::default();
        gate.apply(true, &pads(0));
        gate.apply(true, &pads(SOUTH));
        gate.apply(false, &pads(0));
        assert_eq!(gate.apply(true, &pads(0))[0].buttons, 0);
    }
}
