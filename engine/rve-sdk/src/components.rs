//! Guest wrappers for scene component changes added in ABI v19: lights, mesh renderers, particle
//! emitters, environments, LOD groups, audio source settings and spatial clip options.
//!
//! Each method calls one host callback. A `false` or missing callback means the host refused the
//! change, so the world keeps its previous value.

use crate::abi;
use crate::{AudioError, Context, Object, audio_status};

/// Light kind for [`Object::light`] and [`Object::set_light`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LightKind {
    Directional,
    Point,
    Spot,
}

impl LightKind {
    fn code(self) -> u32 {
        match self {
            Self::Directional => abi::RVE_LIGHT_DIRECTIONAL,
            Self::Point => abi::RVE_LIGHT_POINT,
            Self::Spot => abi::RVE_LIGHT_SPOT,
        }
    }
}

/// Failure of a scene component change made by a guest script.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ComponentError {
    /// The host predates ABI v19 and provides no such callback.
    Unavailable,
    /// The host refused the change: the object lacks the component or a value is out of range.
    Rejected,
}

impl std::fmt::Display for ComponentError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::Unavailable => "component callback is unavailable in this host",
            Self::Rejected => "the host refused the component change",
        })
    }
}

fn component_status(ok: bool) -> Result<(), ComponentError> {
    if ok {
        Ok(())
    } else {
        Err(ComponentError::Rejected)
    }
}

/// Most LOD levels a script can read in one call; a scene with more levels reports `Rejected`.
pub const MAX_LOD_READ: usize = 16;

impl Object {
    /// Reads one light of the given kind, or `None` when the object has no such light.
    pub fn light(&self, ctx: &Context<'_>, kind: LightKind) -> Option<abi::RveLightState> {
        let callback = ctx.host.light_get?;
        let mut out = abi::RveLightState::default();
        // SAFETY: the output points to a live stack value for this synchronous callback.
        let found = unsafe { callback(ctx.host.call_ctx(), self.handle, kind.code(), &mut out) };
        found.then_some(out)
    }

    /// Writes one light of the given kind, including the object's enabled flag.
    pub fn set_light(
        &self,
        ctx: &mut Context<'_>,
        kind: LightKind,
        light: &abi::RveLightState,
    ) -> Result<(), ComponentError> {
        let callback = ctx.host.light_set.ok_or(ComponentError::Unavailable)?;
        // SAFETY: the value points to a live value for this synchronous callback.
        component_status(unsafe { callback(ctx.host.call_ctx(), self.handle, kind.code(), light) })
    }

    /// Whether the mesh renderer is drawn; `None` without a mesh renderer.
    pub fn mesh_visible(&self, ctx: &Context<'_>) -> Option<bool> {
        let callback = ctx.host.mesh_get_visible?;
        let mut visible = false;
        // SAFETY: the output points to a live stack value for this synchronous callback.
        unsafe { callback(ctx.host.call_ctx(), self.handle, &mut visible) }.then_some(visible)
    }

    /// Shows or hides the mesh renderer. Hidden meshes leave the frame.
    pub fn set_mesh_visible(
        &self,
        ctx: &mut Context<'_>,
        visible: bool,
    ) -> Result<(), ComponentError> {
        let callback = ctx
            .host
            .mesh_set_visible
            .ok_or(ComponentError::Unavailable)?;
        // SAFETY: host callback is valid during this script call.
        component_status(unsafe { callback(ctx.host.call_ctx(), self.handle, visible) })
    }

    /// Replaces the material factors that have their `has_*` flag set.
    pub fn set_mesh_material_params(
        &self,
        ctx: &mut Context<'_>,
        params: &abi::RveMeshMaterialParams,
    ) -> Result<(), ComponentError> {
        let callback = ctx
            .host
            .mesh_set_material_params
            .ok_or(ComponentError::Unavailable)?;
        // SAFETY: the params point to a live value for this synchronous callback.
        component_status(unsafe { callback(ctx.host.call_ctx(), self.handle, params) })
    }

    /// Replaces the material of every mesh part with the scene's `.rvemat` at `path`. The path must
    /// be declared in `[[assets]]`; otherwise the host refuses the call.
    pub fn set_material_path(
        &self,
        ctx: &mut Context<'_>,
        path: &str,
    ) -> Result<(), ComponentError> {
        let callback = ctx
            .host
            .mesh_set_material_path
            .ok_or(ComponentError::Unavailable)?;
        // SAFETY: the path string remains valid for this synchronous callback.
        component_status(unsafe {
            callback(
                ctx.host.call_ctx(),
                self.handle,
                abi::RveStr::from_str_ref(path),
            )
        })
    }

    /// Whether the emitter is spawning particles; `None` without a particle emitter.
    pub fn particles_playing(&self, ctx: &Context<'_>) -> Option<bool> {
        let callback = ctx.host.particles_get_playing?;
        let mut playing = false;
        // SAFETY: the output points to a live stack value for this synchronous callback.
        unsafe { callback(ctx.host.call_ctx(), self.handle, &mut playing) }.then_some(playing)
    }

    /// Starts or stops particle spawning; live particles keep simulating.
    pub fn set_particles_playing(
        &self,
        ctx: &mut Context<'_>,
        playing: bool,
    ) -> Result<(), ComponentError> {
        let callback = ctx
            .host
            .particles_set_playing
            .ok_or(ComponentError::Unavailable)?;
        // SAFETY: host callback is valid during this script call.
        component_status(unsafe { callback(ctx.host.call_ctx(), self.handle, playing) })
    }

    /// Restarts the emitter cycle at the end of the current frame.
    pub fn restart_particles(&self, ctx: &mut Context<'_>) -> Result<(), ComponentError> {
        self.particles_command(ctx, abi::RVE_PARTICLE_RESTART, 0)
    }

    /// Emits `count` particles at once (`1..=RVE_MAX_PARTICLE_BURST`).
    pub fn emit_particles(&self, ctx: &mut Context<'_>, count: u32) -> Result<(), ComponentError> {
        self.particles_command(ctx, abi::RVE_PARTICLE_BURST, count)
    }

    fn particles_command(
        &self,
        ctx: &mut Context<'_>,
        kind: u32,
        count: u32,
    ) -> Result<(), ComponentError> {
        let callback = ctx
            .host
            .particles_command
            .ok_or(ComponentError::Unavailable)?;
        // SAFETY: host callback is valid during this script call.
        component_status(unsafe { callback(ctx.host.call_ctx(), self.handle, kind, count) })
    }

    /// Reads the environment intensity and sky; `None` without an environment component.
    pub fn environment(&self, ctx: &Context<'_>) -> Option<abi::RveEnvironmentState> {
        let callback = ctx.host.environment_get?;
        let mut out = abi::RveEnvironmentState::default();
        // SAFETY: the output points to a live stack value for this synchronous callback.
        unsafe { callback(ctx.host.call_ctx(), self.handle, &mut out) }.then_some(out)
    }

    /// Writes the environment intensity and, when `has_sky` is set, the procedural sky.
    pub fn set_environment(
        &self,
        ctx: &mut Context<'_>,
        state: &abi::RveEnvironmentState,
    ) -> Result<(), ComponentError> {
        let callback = ctx
            .host
            .environment_set
            .ok_or(ComponentError::Unavailable)?;
        // SAFETY: the state points to a live value for this synchronous callback.
        component_status(unsafe { callback(ctx.host.call_ctx(), self.handle, state) })
    }

    /// Reads LOD distances (at most [`MAX_LOD_READ`] levels) and the cull distance.
    pub fn lod(&self, ctx: &Context<'_>) -> Option<(Vec<f32>, Option<f32>)> {
        let callback = ctx.host.lod_get?;
        let mut distances = [0.0_f32; MAX_LOD_READ];
        let mut count = 0_usize;
        let mut has_cull = false;
        let mut cull = 0.0_f32;
        // SAFETY: all outputs point to live stack values for this synchronous callback.
        let found = unsafe {
            callback(
                ctx.host.call_ctx(),
                self.handle,
                distances.as_mut_ptr(),
                MAX_LOD_READ,
                &mut count,
                &mut has_cull,
                &mut cull,
            )
        };
        if !found || count > MAX_LOD_READ {
            return None;
        }
        Some((distances[..count].to_vec(), has_cull.then_some(cull)))
    }

    /// Replaces the LOD distances (one per level) and the optional cull distance.
    pub fn set_lod(
        &self,
        ctx: &mut Context<'_>,
        distances: &[f32],
        cull_distance: Option<f32>,
    ) -> Result<(), ComponentError> {
        let callback = ctx.host.lod_set.ok_or(ComponentError::Unavailable)?;
        // SAFETY: the slice is valid for `distances.len()` reads during this synchronous callback.
        component_status(unsafe {
            callback(
                ctx.host.call_ctx(),
                self.handle,
                distances.as_ptr(),
                distances.len(),
                cull_distance.is_some(),
                cull_distance.unwrap_or(0.0),
            )
        })
    }

    /// Reads the audio source settings; `None` without an audio source component.
    pub fn audio_source(&self, ctx: &Context<'_>) -> Option<abi::RveAudioSourceState> {
        let callback = ctx.host.audio_get_source?;
        let mut out = abi::RveAudioSourceState::default();
        // SAFETY: the output points to a live stack value for this synchronous callback.
        unsafe { callback(ctx.host.call_ctx(), self.handle, &mut out) }.then_some(out)
    }

    /// Replaces the audio source settings; the clip and `[[assets]]` stay as the scene defines.
    pub fn set_audio_source(
        &self,
        ctx: &mut Context<'_>,
        settings: &abi::RveAudioSourceState,
    ) -> Result<(), ComponentError> {
        let callback = ctx
            .host
            .audio_set_source
            .ok_or(ComponentError::Unavailable)?;
        // SAFETY: the settings point to a live value for this synchronous callback.
        component_status(unsafe { callback(ctx.host.call_ctx(), self.handle, settings) })
    }
}

impl Object {
    /// Reads the audio source's streaming and fade-in settings; `None` without an audio source
    /// or on a host before ABI v26.
    pub fn audio_playback(&self, ctx: &Context<'_>) -> Option<abi::RveAudioPlayback> {
        let callback = ctx.host.audio_get_source_playback?;
        let mut out = abi::RveAudioPlayback::default();
        // SAFETY: the output points to a live stack value for this synchronous callback.
        unsafe { callback(ctx.host.call_ctx(), self.handle, &mut out) }.then_some(out)
    }

    /// Replaces the audio source's streaming and fade-in settings; they apply to its next start.
    pub fn set_audio_playback(
        &self,
        ctx: &mut Context<'_>,
        playback: &abi::RveAudioPlayback,
    ) -> Result<(), ComponentError> {
        let callback = ctx
            .host
            .audio_set_source_playback
            .ok_or(ComponentError::Unavailable)?;
        // SAFETY: the settings point to a live value for this synchronous callback.
        component_status(unsafe { callback(ctx.host.call_ctx(), self.handle, playback) })
    }
}

impl Context<'_> {
    /// Starts a clip like [`Context::play_clip_spatial`] with streaming and fade-in options.
    /// Needs ABI v26.
    pub fn play_clip_playback(
        &mut self,
        path: &str,
        params: &abi::RveAudioPlayExParams,
        playback: &abi::RveAudioPlayback,
    ) -> Result<u64, AudioError> {
        let callback = self
            .host
            .audio_play_clip_playback
            .ok_or(AudioError::Unavailable)?;
        let mut voice = 0;
        // SAFETY: path, params, playback and output storage remain valid during this script call.
        audio_status(unsafe {
            callback(
                self.host.call_ctx(),
                abi::RveStr::from_str_ref(path),
                params,
                playback,
                &mut voice,
            )
        })?;
        Ok(voice)
    }

    /// Starts a clip with spatial options: follow object, distances, blend and mono.
    pub fn play_clip_spatial(
        &mut self,
        path: &str,
        params: &abi::RveAudioPlayExParams,
    ) -> Result<u64, AudioError> {
        let callback = self
            .host
            .audio_play_clip_ex
            .ok_or(AudioError::Unavailable)?;
        let mut voice = 0;
        // SAFETY: path, params and output storage remain valid during this script call.
        audio_status(unsafe {
            callback(
                self.host.call_ctx(),
                abi::RveStr::from_str_ref(path),
                params,
                &mut voice,
            )
        })?;
        Ok(voice)
    }

    /// Pins a voice's emitter at `position` and stops following its object.
    pub fn set_voice_position(&mut self, voice: u64, position: [f32; 3]) -> Result<(), AudioError> {
        if !position.iter().all(|axis| axis.is_finite()) {
            return Err(AudioError::InvalidInput);
        }
        let callback = self
            .host
            .audio_set_voice_position
            .ok_or(AudioError::Unavailable)?;
        // SAFETY: the position points to a live value for this synchronous callback.
        audio_status(unsafe { callback(self.host.call_ctx(), voice, &position) })
    }
}
