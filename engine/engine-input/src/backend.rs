//! `gilrs` adapter: non-blocking event polling, the initial pad enumeration, and rumble.

use crate::mapper::{GamepadMapper, GamepadSnapshot, GamepadTuning, SLOT_COUNT};
use crate::translate::{MapperEvent, translate};
use gilrs::ff::{BaseEffect, BaseEffectType, Effect, EffectBuilder, Replay, Ticks};
use gilrs::{GamepadId, Gilrs};

/// Owns the `gilrs` context. Create it on the thread that polls it: the runtime and the editor
/// each keep one instance and call [`GilrsBackend::poll`] once per update.
pub struct GilrsBackend {
    gilrs: Option<Gilrs>,
    mapper: GamepadMapper,
    /// Rumble effects keyed by slot. Dropping an effect stops it, so replacing one ends the old.
    effects: Vec<(usize, Effect)>,
}

impl GilrsBackend {
    /// Opens `gilrs`. If it fails, logs a warning and runs with no gamepads: snapshots stay empty
    /// and rumble does nothing.
    pub fn new(tuning: GamepadTuning) -> Self {
        let mut mapper = GamepadMapper::new(tuning);
        let gilrs = match Gilrs::new() {
            Ok(gilrs) => {
                for (id, _) in gilrs.gamepads() {
                    mapper.handle(usize::from(id), MapperEvent::Connected);
                }
                Some(gilrs)
            }
            Err(error) => {
                log::warn!("gamepad input is unavailable: {error}");
                None
            }
        };
        Self {
            gilrs,
            mapper,
            effects: Vec::new(),
        }
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

    /// Returns the processed state of every slot.
    pub fn snapshot(&self) -> GamepadSnapshot {
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

    /// Stops every rumble effect, for example on focus loss or when Play stops.
    pub fn stop_rumble(&mut self) {
        self.effects.clear();
    }
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
        .gamepads(&[gamepad])
        .finish(gilrs)
}
