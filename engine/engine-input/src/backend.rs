//! `gilrs` adapter: non-blocking event polling, the initial pad enumeration, and rumble.

use crate::mapper::{GamepadMapper, GamepadSnapshot, GamepadTuning, SLOT_COUNT};
use crate::translate::{MapperEvent, translate};
use gilrs::ff::{BaseEffect, BaseEffectType, Effect, EffectBuilder, Repeat, Replay, Ticks};
use gilrs::{GamepadId, Gilrs, GilrsBuilder};

/// Owns the `gilrs` context. Create it on the thread that polls it: the runtime and the editor
/// each keep one instance and call [`GilrsBackend::poll`] once per update.
pub struct GilrsBackend {
    gilrs: Option<Gilrs>,
    mapper: GamepadMapper,
    /// Rumble effects keyed by slot. Dropping an effect stops it, so replacing one ends the old.
    effects: Vec<(usize, Effect)>,
}

impl GilrsBackend {
    /// Opens `gilrs` without its default filters: the dead zone and the D-pad conversion are done
    /// by [`GamepadMapper`], so `[input.gamepad]` is the only dead zone. If opening fails, logs a
    /// warning and runs with no gamepads: snapshots stay empty and rumble does nothing.
    pub fn new(tuning: GamepadTuning) -> Self {
        let mut backend = Self::without_devices(tuning);
        match GilrsBuilder::new().with_default_filters(false).build() {
            Ok(gilrs) => {
                backend.gilrs = Some(gilrs);
                backend.register_connected();
            }
            Err(error) => log::warn!("gamepad input is unavailable: {error}"),
        }
        backend
    }

    /// Creates a backend without `gilrs`, for tests and hosts that must not touch devices. Its
    /// snapshots stay empty and rumble does nothing.
    pub fn without_devices(tuning: GamepadTuning) -> Self {
        Self {
            gilrs: None,
            mapper: GamepadMapper::new(tuning),
            effects: Vec::new(),
        }
    }

    /// Prepares the backend for a new Play session: discards queued events and pad state, applies
    /// `tuning`, and registers the pads that are already connected.
    pub fn begin_session(&mut self, tuning: GamepadTuning) {
        self.effects.clear();
        if let Some(gilrs) = self.gilrs.as_mut() {
            while gilrs.next_event().is_some() {}
        }
        self.mapper = GamepadMapper::new(tuning);
        self.register_connected();
    }

    /// Ends a Play session: stops rumble and releases every pad. The `gilrs` context stays open.
    pub fn end_session(&mut self) {
        self.stop_rumble();
        self.mapper.reset();
    }

    /// Drains pending backend events without blocking.
    pub fn poll(&mut self) {
        let Some(gilrs) = self.gilrs.as_mut() else {
            return;
        };
        while let Some(gilrs::Event { id, event, .. }) = gilrs.next_event() {
            if let Some(event) = translate(event) {
                self.mapper.handle(usize::from(id), event);
            }
        }
    }

    /// Returns the processed state of every slot and clears the button latches.
    pub fn snapshot(&mut self) -> GamepadSnapshot {
        self.mapper.snapshot()
    }

    /// Plays rumble on slot `pad` (1..=4), or on every slot for `pad` 0. Values must already be
    /// normalized: intensities in [0, 1] and seconds in [0, 5]. Pads without force feedback are
    /// skipped and logged at debug level. A new request replaces the previous one on the same slot.
    pub fn rumble(&mut self, pad: u8, strong: f32, weak: f32, seconds: f32) {
        let Some(gilrs) = self.gilrs.as_mut() else {
            return;
        };
        let slots: Vec<usize> = if pad == 0 {
            (0..SLOT_COUNT).collect()
        } else {
            vec![usize::from(pad) - 1]
        };
        self.effects.retain(|(slot, _)| !slots.contains(slot));

        let duration_ms = (seconds * 1000.0).round() as u32;
        if duration_ms == 0 || (strong <= 0.0 && weak <= 0.0) {
            return;
        }
        for slot in slots {
            let Some(id) = self.mapper.id_for_slot(slot) else {
                continue;
            };
            let Some(gamepad) = gilrs
                .gamepads()
                .find_map(|(gid, _)| (usize::from(gid) == id).then_some(gid))
            else {
                continue;
            };
            if !gilrs.gamepad(gamepad).is_ff_supported() {
                log::debug!("gamepad in slot {} has no force feedback", slot + 1);
                continue;
            }
            match build_effect(gilrs, gamepad, strong, weak, duration_ms) {
                Ok(effect) => {
                    if let Err(error) = effect.play() {
                        log::debug!("gamepad rumble failed to start: {error}");
                    }
                    self.effects.push((slot, effect));
                }
                Err(error) => log::debug!("gamepad rumble rejected: {error}"),
            }
        }
    }

    /// Delivers one frame's rumble requests as `(pad, strong, weak, seconds)` in script order.
    /// Only the last request for each slot is played; pad 0 covers every slot.
    pub fn rumble_frame(&mut self, requests: impl IntoIterator<Item = (u8, f32, f32, f32)>) {
        for (index, request) in latest_per_slot(requests).into_iter().enumerate() {
            if let Some((strong, weak, seconds)) = request {
                self.rumble(index as u8 + 1, strong, weak, seconds);
            }
        }
    }

    /// Stops every rumble effect, for example on focus loss or when Play stops.
    pub fn stop_rumble(&mut self) {
        self.effects.clear();
    }

    /// Assigns slots to pads that gilrs already reports as connected. gilrs sends no `Connected`
    /// event for them, so this is the only way they reach the mapper.
    fn register_connected(&mut self) {
        let Some(gilrs) = self.gilrs.as_ref() else {
            return;
        };
        for (id, _) in gilrs.gamepads() {
            self.mapper.handle(usize::from(id), MapperEvent::Connected);
        }
    }
}

/// Keeps the last request for each slot, in order. Pad 0 sets every slot; a pad outside 0..=4 is
/// ignored. A zero request is kept, so its slot still stops the previous effect.
fn latest_per_slot(
    requests: impl IntoIterator<Item = (u8, f32, f32, f32)>,
) -> [Option<(f32, f32, f32)>; SLOT_COUNT] {
    let mut latest = [None; SLOT_COUNT];
    for (pad, strong, weak, seconds) in requests {
        let request = Some((strong, weak, seconds));
        match usize::from(pad) {
            0 => latest = [request; SLOT_COUNT],
            slot @ 1..=SLOT_COUNT => latest[slot - 1] = request,
            _ => {}
        }
    }
    latest
}

fn build_effect(
    gilrs: &mut Gilrs,
    gamepad: GamepadId,
    strong: f32,
    weak: f32,
    duration_ms: u32,
) -> Result<Effect, gilrs::ff::Error> {
    let replay = Replay {
        play_for: Ticks::from_ms(duration_ms),
        ..Replay::default()
    };
    let magnitude = |value: f32| (value.clamp(0.0, 1.0) * f32::from(u16::MAX)).round() as u16;
    EffectBuilder::new()
        .add_effect(BaseEffect {
            kind: BaseEffectType::Strong {
                magnitude: magnitude(strong),
            },
            scheduling: replay,
            envelope: Default::default(),
        })
        .add_effect(BaseEffect {
            kind: BaseEffectType::Weak {
                magnitude: magnitude(weak),
            },
            scheduling: replay,
            envelope: Default::default(),
        })
        // gilrs repeats an effect forever by default; the effect must end after its duration.
        .repeat(Repeat::For(Ticks::from_ms(duration_ms)))
        .gamepads(&[gamepad])
        .finish(gilrs)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mapper::PadSnapshot;
    use crate::translate::MapperEvent;
    use rve_sdk::GamepadButton;

    const SOUTH: u32 = 1 << GamepadButton::South as u32;

    #[test]
    fn later_pad_zero_request_replaces_every_earlier_slot_request() {
        let latest = latest_per_slot([(2, 1.0, 1.0, 1.0), (0, 0.5, 0.25, 0.5)]);
        assert_eq!(latest, [Some((0.5, 0.25, 0.5)); SLOT_COUNT]);
    }

    #[test]
    fn slot_request_after_pad_zero_overrides_only_its_slot() {
        let latest = latest_per_slot([(0, 0.5, 0.5, 1.0), (3, 1.0, 0.0, 2.0)]);
        assert_eq!(
            latest,
            [
                Some((0.5, 0.5, 1.0)),
                Some((0.5, 0.5, 1.0)),
                Some((1.0, 0.0, 2.0)),
                Some((0.5, 0.5, 1.0)),
            ]
        );
    }

    #[test]
    fn zero_request_is_kept_so_its_slot_stops_the_previous_effect() {
        let latest = latest_per_slot([(1, 1.0, 1.0, 1.0), (1, 0.0, 0.0, 0.0)]);
        assert_eq!(latest[0], Some((0.0, 0.0, 0.0)));
    }

    #[test]
    fn pads_outside_the_slot_range_are_ignored() {
        assert_eq!(latest_per_slot([(5, 1.0, 1.0, 1.0)]), [None; SLOT_COUNT]);
    }

    #[test]
    fn ending_a_session_releases_pads_and_a_new_session_starts_clean() {
        let mut backend = GilrsBackend::without_devices(GamepadTuning::default());
        backend.mapper.handle(7, MapperEvent::Connected);
        backend.mapper.handle(
            7,
            MapperEvent::Button {
                button: GamepadButton::South,
                pressed: true,
            },
        );
        assert_eq!(backend.snapshot()[0].buttons, SOUTH);

        backend.end_session();
        assert_eq!(backend.snapshot(), [PadSnapshot::default(); SLOT_COUNT]);

        backend.mapper.handle(7, MapperEvent::Connected);
        backend.begin_session(GamepadTuning::default());
        assert!(backend.snapshot().iter().all(|pad| !pad.connected));
    }
}
