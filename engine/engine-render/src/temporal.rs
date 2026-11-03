use super::AntiAliasing;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Extent {
    pub width: u32,
    pub height: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct TemporalKey {
    pub render_extent: Extent,
    pub output_extent: Extent,
    pub anti_aliasing: AntiAliasing,
    pub debug_view: u32,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) struct TemporalPlan {
    pub key: TemporalKey,
    pub current_view_projection: [[f32; 4]; 4],
    pub previous_view_projection: [[f32; 4]; 4],
    pub jitter_pixels: [f32; 2],
    pub jitter_ndc: [f32; 2],
    pub jitter_render_pixels: [f32; 2],
    pub lod_bias: f32,
    pub history_valid: bool,
    pub reset_history: bool,
    pub phase_index: u32,
    pub phase_count: u32,
    pub read_history: usize,
    pub write_history: usize,
}

#[derive(Default)]
pub(super) struct TemporalState {
    key: Option<TemporalKey>,
    previous_view_projection: Option<[[f32; 4]; 4]>,
    history_valid: bool,
    missed: bool,
    phase_index: u32,
    next_history: usize,
}

impl TemporalState {
    pub fn plan(
        &self,
        key: TemporalKey,
        current_view_projection: [[f32; 4]; 4],
        explicit_reset: bool,
    ) -> TemporalPlan {
        let temporal = key.anti_aliasing.is_temporal();
        let phase_count = phase_count(key.render_extent, key.output_extent);
        let reset_history = explicit_reset
            || self.missed
            || !self.history_valid
            || self.key != Some(key)
            || self.previous_view_projection.is_none();
        let phase_index = if reset_history {
            0
        } else {
            self.phase_index % phase_count
        };
        let jitter_pixels = if temporal {
            jitter_pixels(phase_index, phase_count)
        } else {
            [0.0; 2]
        };
        let output_width = key.output_extent.width.max(1) as f32;
        let output_height = key.output_extent.height.max(1) as f32;
        let render_width = key.render_extent.width.max(1) as f32;
        let render_height = key.render_extent.height.max(1) as f32;
        let jitter_ndc = [
            2.0 * jitter_pixels[0] / output_width,
            -2.0 * jitter_pixels[1] / output_height,
        ];
        let jitter_render_pixels = ndc_jitter_to_screen_pixels(jitter_ndc, key.render_extent);
        let lod_ratio = ((render_width / output_width) * (render_height / output_height)).sqrt();
        let write_history = if reset_history { 0 } else { self.next_history };

        TemporalPlan {
            key,
            current_view_projection,
            previous_view_projection: if reset_history || !temporal {
                current_view_projection
            } else {
                self.previous_view_projection
                    .unwrap_or(current_view_projection)
            },
            jitter_pixels,
            jitter_ndc,
            jitter_render_pixels,
            lod_bias: if temporal { lod_ratio.log2() } else { 0.0 },
            history_valid: temporal && self.history_valid && !reset_history,
            reset_history,
            phase_index,
            phase_count,
            read_history: 1 - write_history,
            write_history,
        }
    }

    /// Advances phase, view matrix, and history only after a successful queue submission.
    pub fn submitted(&mut self, plan: TemporalPlan) {
        self.key = Some(plan.key);
        self.previous_view_projection = Some(plan.current_view_projection);
        self.missed = false;
        if plan.key.anti_aliasing.is_temporal() {
            self.history_valid = true;
            self.next_history = 1 - plan.write_history;
            self.phase_index = (plan.phase_index + 1) % plan.phase_count;
        } else {
            self.history_valid = false;
            self.phase_index = 0;
            self.next_history = 0;
        }
    }

    /// A skipped view or failed submit breaks temporal continuity.
    pub fn missed(&mut self) {
        self.history_valid = false;
        self.missed = true;
        self.phase_index = 0;
        self.next_history = 0;
    }
}

pub(super) fn phase_count(render: Extent, output: Extent) -> u32 {
    let render_area = render.width.max(1) as f64 * render.height.max(1) as f64;
    let output_area = output.width.max(1) as f64 * output.height.max(1) as f64;
    (8.0 * output_area / render_area).ceil().max(1.0) as u32
}

pub(super) fn halton(index: u32, base: u32) -> f32 {
    let mut index = index;
    let mut fraction = 1.0;
    let mut result = 0.0;
    while index > 0 {
        fraction /= base as f32;
        result += fraction * (index % base) as f32;
        index /= base;
    }
    result
}

pub(super) fn jitter_pixels(phase_index: u32, phases: u32) -> [f32; 2] {
    let index = phase_index % phases.max(1) + 1;
    [halton(index, 2) - 0.5, halton(index, 3) - 0.5]
}

pub(super) fn ndc_jitter_to_screen_pixels(ndc: [f32; 2], extent: Extent) -> [f32; 2] {
    [
        ndc[0] * extent.width as f32 * 0.5,
        -ndc[1] * extent.height as f32 * 0.5,
    ]
}

#[cfg(test)]
fn clip_to_top_left_uv(clip: [f32; 4]) -> [f32; 2] {
    [clip[0] / clip[3] * 0.5 + 0.5, 0.5 - clip[1] / clip[3] * 0.5]
}

#[cfg(test)]
fn motion_vector(current_clip: [f32; 4], previous_clip: [f32; 4]) -> [f32; 2] {
    let current_uv = clip_to_top_left_uv(current_clip);
    let previous_uv = clip_to_top_left_uv(previous_clip);
    [
        previous_uv[0] - current_uv[0],
        previous_uv[1] - current_uv[1],
    ]
}

#[cfg(test)]
mod tests {
    use super::{Extent, TemporalKey, TemporalState, halton, jitter_pixels, phase_count};
    use crate::AntiAliasing;

    const IDENTITY: [[f32; 4]; 4] = [
        [1.0, 0.0, 0.0, 0.0],
        [0.0, 1.0, 0.0, 0.0],
        [0.0, 0.0, 1.0, 0.0],
        [0.0, 0.0, 0.0, 1.0],
    ];

    fn key(render: [u32; 2], output: [u32; 2], anti_aliasing: AntiAliasing) -> TemporalKey {
        TemporalKey {
            render_extent: Extent {
                width: render[0],
                height: render[1],
            },
            output_extent: Extent {
                width: output[0],
                height: output[1],
            },
            anti_aliasing,
            debug_view: 0,
        }
    }

    #[test]
    fn phase_count_scales_quadratically_with_upscale_ratio() {
        assert_eq!(
            phase_count(
                key([1920, 1080], [1920, 1080], AntiAliasing::Taa).render_extent,
                Extent {
                    width: 1920,
                    height: 1080
                }
            ),
            8
        );
        assert_eq!(
            phase_count(
                key([960, 540], [1920, 1080], AntiAliasing::Taa).render_extent,
                Extent {
                    width: 1920,
                    height: 1080
                }
            ),
            32
        );
        assert_eq!(
            phase_count(
                Extent {
                    width: 100,
                    height: 100
                },
                Extent {
                    width: 125,
                    height: 125
                }
            ),
            13
        );
    }

    #[test]
    fn halton_sequence_and_jitter_use_output_pixels_and_y_down_ndc() {
        assert_eq!(halton(1, 2), 0.5);
        assert_eq!(halton(2, 2), 0.25);
        assert!((halton(2, 3) - 2.0 / 3.0).abs() < f32::EPSILON);
        assert!((jitter_pixels(0, 8)[1] + 1.0 / 6.0).abs() < 1e-6);

        let plan = TemporalState::default().plan(
            key([640, 360], [1280, 720], AntiAliasing::Taa),
            IDENTITY,
            false,
        );
        assert!(plan.jitter_pixels[0].abs() < 1e-6);
        assert!((plan.jitter_pixels[1] + 1.0 / 6.0).abs() < 1e-6);
        assert!(plan.jitter_ndc[0].abs() < 1e-6);
        assert!((plan.jitter_ndc[1] - 1.0 / 2160.0).abs() < 1e-8);
        assert!(plan.jitter_render_pixels[0].abs() < 1e-6);
        assert!((plan.jitter_render_pixels[1] + 1.0 / 12.0).abs() < 1e-6);
        assert_eq!(plan.lod_bias, -1.0);
    }

    #[test]
    fn clip_space_velocity_uses_top_left_uv_and_previous_minus_current() {
        let current = super::clip_to_top_left_uv([0.5, 0.5, 0.0, 1.0]);
        let previous = super::clip_to_top_left_uv([0.25, 0.75, 0.0, 1.0]);
        assert_eq!(current, [0.75, 0.25]);
        assert_eq!(previous, [0.625, 0.125]);
        assert_eq!(
            super::motion_vector([0.5, 0.5, 0.0, 1.0], [0.25, 0.75, 0.0, 1.0]),
            [-0.125, -0.125]
        );

        let screen = super::ndc_jitter_to_screen_pixels(
            [2.0 / 1280.0, -4.0 / 720.0],
            Extent {
                width: 640,
                height: 360,
            },
        );
        assert_eq!(screen, [0.5, 1.0]);
    }

    #[test]
    fn only_successfully_submitted_frames_advance_history_and_phase() {
        let key = key([640, 360], [1280, 720], AntiAliasing::Taa);
        let mut state = TemporalState::default();
        let first = state.plan(key, IDENTITY, false);
        assert!(first.reset_history);
        assert!(!first.history_valid);
        assert_eq!(first.phase_index, 0);
        assert_eq!((first.read_history, first.write_history), (1, 0));
        state.submitted(first);

        let mut moved = IDENTITY;
        moved[3][0] = 2.0;
        let second = state.plan(key, moved, false);
        assert!(second.history_valid);
        assert_eq!(second.previous_view_projection, IDENTITY);
        assert_eq!(second.phase_index, 1);
        assert_eq!((second.read_history, second.write_history), (0, 1));
        // A failed submission does not commit the current VP or advance the phase.
        state.missed();
        let retry = state.plan(key, moved, false);
        assert!(retry.reset_history);
        assert!(!retry.history_valid);
        assert_eq!(retry.phase_index, 0);
        assert_eq!(retry.previous_view_projection, moved);
    }

    #[test]
    fn extent_antialiasing_debug_and_explicit_changes_reset_history() {
        let mut state = TemporalState::default();
        let original = key([640, 360], [1280, 720], AntiAliasing::Taa);
        state.submitted(state.plan(original, IDENTITY, false));
        assert!(!state.plan(original, IDENTITY, false).reset_history);

        let changed_extent = key([800, 450], [1280, 720], AntiAliasing::Taa);
        assert!(state.plan(changed_extent, IDENTITY, false).reset_history);
        let changed_aa = key([640, 360], [1280, 720], AntiAliasing::Fxaa);
        assert!(state.plan(changed_aa, IDENTITY, false).reset_history);
        let mut changed_debug = original;
        changed_debug.debug_view = 2;
        assert!(state.plan(changed_debug, IDENTITY, false).reset_history);
        assert!(state.plan(original, IDENTITY, true).reset_history);

        state.submitted(state.plan(changed_aa, IDENTITY, false));
        let next = state.plan(changed_aa, IDENTITY, false);
        assert!(!next.history_valid);
        assert_eq!(next.jitter_pixels, [0.0; 2]);
        assert_eq!(next.lod_bias, 0.0);
    }

    #[test]
    fn missed_view_invalidates_history_without_advancing_previous_camera() {
        let mut state = TemporalState::default();
        let key = key([640, 360], [1280, 720], AntiAliasing::Taa);
        state.submitted(state.plan(key, IDENTITY, false));
        let before_miss = state.plan(key, IDENTITY, false);
        let mut new_camera = IDENTITY;
        new_camera[3][1] = 5.0;
        state.missed();
        let after_miss = state.plan(key, new_camera, false);
        assert!(after_miss.reset_history);
        assert_eq!(after_miss.previous_view_projection, new_camera);
        assert_eq!(before_miss.phase_index, 1);
        assert_eq!(after_miss.phase_index, 0);
    }
}
