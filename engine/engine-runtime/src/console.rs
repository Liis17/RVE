use egui::TexturesDelta;
use egui::epaint::ClippedPrimitive;
use engine_console::{CommandCatalog, ConsoleBuffer, ConsoleLine};
use std::sync::{Arc, Mutex};

#[derive(Clone, Default)]
pub(crate) struct CommandCatalogSnapshot {
    pub generation: u64,
    pub catalog: CommandCatalog,
    /// Скрипт зарегистрировал команду `profile`; встроенная команда при этом не добавляется.
    pub profile_shadowed: bool,
}

#[derive(Default)]
pub(crate) struct ConsoleUiFrame {
    pub visible: bool,
    pub primitives: Vec<ClippedPrimitive>,
    pub textures_delta: TexturesDelta,
    pub pixels_per_point: f32,
}

impl ConsoleUiFrame {
    pub fn replace_with(&mut self, mut newer: Self) {
        let mut textures_delta = std::mem::take(&mut self.textures_delta);
        textures_delta.append(std::mem::take(&mut newer.textures_delta));
        newer.textures_delta = textures_delta;
        *self = newer;
    }
}

impl Drop for ConsoleUiFrame {
    fn drop(&mut self) {
        self.textures_delta.clear();
    }
}

#[derive(Default)]
struct ConsoleUiSlotState {
    pending: Option<ConsoleUiFrame>,
    notification_pending: bool,
}

#[derive(Default)]
pub(crate) struct ConsoleUiSlot(Mutex<ConsoleUiSlotState>);

impl ConsoleUiSlot {
    /// Returns true only when the caller should enqueue a wake-up for the render thread.
    pub fn publish(&self, mut frame: ConsoleUiFrame) -> bool {
        let mut state = self
            .0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(mut previous) = state.pending.take() {
            previous.replace_with(frame);
            frame = previous;
        }
        state.pending = Some(frame);
        if state.notification_pending {
            false
        } else {
            state.notification_pending = true;
            true
        }
    }

    pub fn take(&self) -> Option<ConsoleUiFrame> {
        let mut state = self
            .0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        state.notification_pending = false;
        state.pending.take()
    }

    pub fn discard_geometry_preserve_texture_delta(&self) {
        let mut state = self
            .0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(frame) = state.pending.as_mut() {
            frame.primitives.clear();
        }
    }
}

pub(crate) fn snapshot_lines(buffer: &Arc<Mutex<ConsoleBuffer>>) -> Vec<ConsoleLine> {
    buffer
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .snapshot()
}

#[cfg(test)]
mod tests {
    use super::{ConsoleUiFrame, ConsoleUiSlot};
    use egui::epaint::{ClippedPrimitive, ImageDelta, Mesh, Primitive};
    use egui::{Color32, ColorImage, ImageData, Rect, TextureId, TextureOptions, TexturesDelta};
    use std::sync::Arc;

    fn delta(id: u64, width: usize) -> TexturesDelta {
        let mut delta = TexturesDelta::default();
        delta.set.insert(
            TextureId::Managed(id),
            vec![ImageDelta::full(
                Arc::new(ColorImage::filled([width, 1], Color32::WHITE)),
                TextureOptions::LINEAR,
            )]
            .into(),
        );
        delta
    }

    fn patch_delta(id: u64, width: usize) -> TexturesDelta {
        let mut delta = TexturesDelta::default();
        delta.set.insert(
            TextureId::Managed(id),
            vec![ImageDelta {
                image: ImageData::Color(Arc::new(ColorImage::filled([width, 1], Color32::WHITE))),
                options: TextureOptions::LINEAR,
                pos: Some([0, 0]),
            }]
            .into(),
        );
        delta
    }

    fn geometry() -> Vec<ClippedPrimitive> {
        vec![ClippedPrimitive {
            clip_rect: Rect::EVERYTHING,
            primitive: Primitive::Mesh(Mesh::default()),
        }]
    }

    #[test]
    fn latest_ui_frame_merges_texture_deltas_in_order() {
        let slot = ConsoleUiSlot::default();
        let mut first = ConsoleUiFrame {
            visible: true,
            primitives: geometry(),
            textures_delta: delta(1, 2),
            pixels_per_point: 1.0,
        };
        first.textures_delta.free.insert(TextureId::Managed(7));
        assert!(slot.publish(first));

        let mut second = ConsoleUiFrame {
            visible: true,
            primitives: Vec::new(),
            textures_delta: patch_delta(1, 3),
            pixels_per_point: 2.0,
        };
        second.textures_delta.free.insert(TextureId::Managed(8));
        assert!(!slot.publish(second));

        let mut latest = slot.take().unwrap();
        assert!(latest.visible);
        assert!(latest.primitives.is_empty());
        assert_eq!(latest.pixels_per_point, 2.0);
        assert_eq!(latest.textures_delta.set[&TextureId::Managed(1)].len(), 2);
        assert!(latest.textures_delta.free.contains(&TextureId::Managed(7)));
        assert!(latest.textures_delta.free.contains(&TextureId::Managed(8)));
        latest.textures_delta.clear();
    }

    #[test]
    fn discarding_geometry_keeps_pending_texture_changes() {
        let slot = ConsoleUiSlot::default();
        let mut frame = ConsoleUiFrame {
            visible: true,
            primitives: geometry(),
            textures_delta: delta(3, 4),
            pixels_per_point: 1.0,
        };
        frame.textures_delta.free.insert(TextureId::Managed(9));
        assert!(slot.publish(frame));
        slot.discard_geometry_preserve_texture_delta();

        let mut latest = slot.take().unwrap();
        assert!(latest.visible);
        assert!(latest.primitives.is_empty());
        assert!(
            latest
                .textures_delta
                .set
                .contains_key(&TextureId::Managed(3))
        );
        assert!(latest.textures_delta.free.contains(&TextureId::Managed(9)));
        latest.textures_delta.clear();
    }
}
