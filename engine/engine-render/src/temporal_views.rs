//! Состояние временного сглаживания и апскейлеров (TAA, FSR, DLSS) для каждого вида.

use crate::*;

#[cfg(any(windows, test))]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TemporalBackend {
    Taa,
    Fsr,
    Dlss,
}

#[cfg(any(windows, test))]
pub(crate) fn select_temporal_backend(
    anti_aliasing: AntiAliasing,
    fsr_available: bool,
    dlss_available: bool,
) -> TemporalBackend {
    match anti_aliasing {
        AntiAliasing::Fsr if fsr_available => TemporalBackend::Fsr,
        AntiAliasing::Dlss if dlss_available => TemporalBackend::Dlss,
        _ => TemporalBackend::Taa,
    }
}

#[derive(Default)]
pub(crate) struct TemporalView {
    pub(crate) state: TemporalState,
    pub(crate) history: Option<TemporalHistory>,
    pub(crate) last_submitted_at: Option<Instant>,
    /// Адаптация экспозиции вида; общая для всех кадровых слотов вида.
    pub(crate) exposure: ExposureHistory,
    /// Буфер экспозиции вида; создаётся при первом кадре вида.
    pub(crate) exposure_resources: Option<ExposureResources>,
    #[cfg(windows)]
    pub(crate) fsr: Option<Box<FsrView>>,
    #[cfg(windows)]
    pub(crate) fsr_attempted: bool,
    #[cfg(windows)]
    pub(crate) fsr_error: Option<String>,
    #[cfg(windows)]
    pub(crate) fsr_extents: Option<([u32; 2], [u32; 2])>,
    #[cfg(all(windows, feature = "dlss"))]
    pub(crate) dlss: Option<Box<DlssView>>,
    #[cfg(all(windows, feature = "dlss"))]
    pub(crate) dlss_attempted: bool,
    #[cfg(all(windows, feature = "dlss"))]
    pub(crate) dlss_error: Option<String>,
    #[cfg(all(windows, feature = "dlss"))]
    pub(crate) dlss_extents: Option<([u32; 2], [u32; 2])>,
}

pub(crate) struct TemporalHistory {
    pub(crate) extent: vk::Extent2D,
    pub(crate) images: [DeviceImage; 2],
    pub(crate) layouts: [vk::ImageLayout; 2],
}

#[cfg(windows)]
pub(crate) struct FsrView {
    pub(crate) context: fsr::FfxContext,
    pub(crate) output: DeviceImage,
    pub(crate) exposure: DeviceImage,
    pub(crate) render_extent: vk::Extent2D,
    pub(crate) output_extent: vk::Extent2D,
    pub(crate) output_layout: vk::ImageLayout,
    pub(crate) exposure_layout: vk::ImageLayout,
}

#[cfg(windows)]
impl FsrView {
    pub(crate) fn new(
        api: &Arc<fsr::FfxApi>,
        instance: &ash::Instance,
        device: &ash::Device,
        physical_device: vk::PhysicalDevice,
        render_extent: vk::Extent2D,
        output_extent: vk::Extent2D,
    ) -> Result<Self, RenderError> {
        let context = api
            .create_upscale_context(
                device.handle(),
                physical_device,
                instance.fp_v1_0().get_device_proc_addr,
                [render_extent.width, render_extent.height],
                [output_extent.width, output_extent.height],
            )
            .map_err(|error| RenderError::Unsupported(format!("FSR context: {error}")))?;
        let output = DeviceImage::new(
            instance,
            device,
            physical_device,
            output_extent,
            OFFSCREEN_FORMAT,
            vk::ImageUsageFlags::STORAGE | vk::ImageUsageFlags::SAMPLED,
            color_subresource_range(),
        )?;
        let exposure = DeviceImage::new(
            instance,
            device,
            physical_device,
            vk::Extent2D {
                width: 1,
                height: 1,
            },
            vk::Format::R32_SFLOAT,
            vk::ImageUsageFlags::TRANSFER_DST | vk::ImageUsageFlags::SAMPLED,
            color_subresource_range(),
        )?;
        Ok(Self {
            context,
            output,
            exposure,
            render_extent,
            output_extent,
            output_layout: vk::ImageLayout::UNDEFINED,
            exposure_layout: vk::ImageLayout::UNDEFINED,
        })
    }
}

#[cfg(all(windows, feature = "dlss"))]
pub(crate) struct DlssView {
    pub(crate) output: DeviceImage,
    pub(crate) exposure: DeviceImage,
    pub(crate) render_extent: vk::Extent2D,
    pub(crate) output_extent: vk::Extent2D,
    pub(crate) output_layout: vk::ImageLayout,
    pub(crate) exposure_layout: vk::ImageLayout,
    pub(crate) feature: Option<usize>,
}

#[cfg(all(windows, feature = "dlss"))]
impl DlssView {
    pub(crate) fn new(
        instance: &ash::Instance,
        device: &ash::Device,
        physical_device: vk::PhysicalDevice,
        render_extent: vk::Extent2D,
        output_extent: vk::Extent2D,
    ) -> Result<Self, RenderError> {
        let output = DeviceImage::new(
            instance,
            device,
            physical_device,
            output_extent,
            OFFSCREEN_FORMAT,
            vk::ImageUsageFlags::STORAGE | vk::ImageUsageFlags::SAMPLED,
            color_subresource_range(),
        )?;
        let exposure = DeviceImage::new(
            instance,
            device,
            physical_device,
            vk::Extent2D {
                width: 1,
                height: 1,
            },
            vk::Format::R32_SFLOAT,
            vk::ImageUsageFlags::TRANSFER_DST | vk::ImageUsageFlags::SAMPLED,
            color_subresource_range(),
        )?;
        Ok(Self {
            output,
            exposure,
            render_extent,
            output_extent,
            output_layout: vk::ImageLayout::UNDEFINED,
            exposure_layout: vk::ImageLayout::UNDEFINED,
            feature: None,
        })
    }
}

#[cfg(all(windows, feature = "dlss"))]
impl Drop for DlssView {
    fn drop(&mut self) {
        if let Some(feature) = self.feature.take() {
            dlss::release_feature(feature);
        }
    }
}

impl TemporalHistory {
    pub(crate) fn new(
        instance: &ash::Instance,
        device: &ash::Device,
        physical_device: vk::PhysicalDevice,
        extent: vk::Extent2D,
    ) -> Result<Self, RenderError> {
        let images = [
            DeviceImage::new(
                instance,
                device,
                physical_device,
                extent,
                OFFSCREEN_FORMAT,
                vk::ImageUsageFlags::COLOR_ATTACHMENT | vk::ImageUsageFlags::SAMPLED,
                color_subresource_range(),
            )?,
            DeviceImage::new(
                instance,
                device,
                physical_device,
                extent,
                OFFSCREEN_FORMAT,
                vk::ImageUsageFlags::COLOR_ATTACHMENT | vk::ImageUsageFlags::SAMPLED,
                color_subresource_range(),
            )?,
        ];
        Ok(Self {
            extent,
            images,
            layouts: [vk::ImageLayout::UNDEFINED; 2],
        })
    }
}

#[derive(Clone, Copy)]
pub(crate) enum TemporalViewId {
    Screen,
    Viewport(ViewportId),
}

#[derive(Clone, Copy)]
pub(crate) struct TemporalSubmission {
    pub(crate) view: TemporalViewId,
    pub(crate) plan: TemporalPlan,
    /// Решение адаптации экспозиции кадра вида (`None` — ручной кадр); фиксируется после
    /// успешного submit.
    pub(crate) exposure: Option<ExposureFrame>,
    pub(crate) layouts: Option<[vk::ImageLayout; 2]>,
    #[cfg(windows)]
    pub(crate) fsr_used: bool,
    #[cfg(all(windows, feature = "dlss"))]
    pub(crate) dlss_used: bool,
    pub(crate) submitted_at: Instant,
}

pub(crate) fn ensure_temporal_history(
    instance: &ash::Instance,
    device: &ash::Device,
    physical_device: vk::PhysicalDevice,
    temporal: &mut TemporalView,
    output_extent: vk::Extent2D,
    retired: &mut RetireQueue<RetiredResource>,
) -> Result<(), RenderError> {
    if temporal
        .history
        .as_ref()
        .is_some_and(|history| history.extent == output_extent)
    {
        return Ok(());
    }
    let replacement = TemporalHistory::new(instance, device, physical_device, output_extent)?;
    if let Some(previous) = temporal.history.replace(replacement) {
        retired.retire(RetiredResource::TemporalHistory(Box::new(previous)), 0);
    }
    Ok(())
}

#[cfg(windows)]
#[allow(clippy::too_many_arguments)]
pub(crate) fn ensure_fsr_view(
    instance: &ash::Instance,
    device: &ash::Device,
    physical_device: vk::PhysicalDevice,
    temporal: &mut TemporalView,
    api: &Arc<fsr::FfxApi>,
    render_extent: vk::Extent2D,
    output_extent: vk::Extent2D,
    retired: &mut RetireQueue<RetiredResource>,
) -> Result<(), String> {
    let render_size = [render_extent.width, render_extent.height];
    let output_size = [output_extent.width, output_extent.height];
    if temporal.fsr_extents == Some((render_size, output_size)) {
        if temporal.fsr.is_some() {
            return Ok(());
        }
        if temporal.fsr_attempted {
            return Err(temporal
                .fsr_error
                .clone()
                .unwrap_or_else(|| "FSR context creation failed".to_owned()));
        }
    }
    if temporal.fsr_extents != Some((render_size, output_size)) {
        temporal.fsr_extents = Some((render_size, output_size));
        temporal.fsr_attempted = false;
        temporal.fsr_error = None;
    }
    if temporal
        .fsr
        .as_ref()
        .is_some_and(|fsr| fsr.render_extent == render_extent && fsr.output_extent == output_extent)
    {
        return Ok(());
    }
    if let Some(previous) = temporal.fsr.take() {
        retired.retire(RetiredResource::FsrView(previous), 0);
    }
    temporal.fsr_attempted = true;
    temporal.fsr_error = None;
    match FsrView::new(
        api,
        instance,
        device,
        physical_device,
        render_extent,
        output_extent,
    ) {
        Ok(fsr) => {
            temporal.fsr = Some(Box::new(fsr));
            Ok(())
        }
        Err(error) => {
            let error = error.to_string();
            temporal.fsr_error = Some(error.clone());
            temporal.fsr_attempted = true;
            Err(error)
        }
    }
}

#[cfg(all(windows, feature = "dlss"))]
pub(crate) fn ensure_dlss_view(
    instance: &ash::Instance,
    device: &ash::Device,
    physical_device: vk::PhysicalDevice,
    temporal: &mut TemporalView,
    render_extent: vk::Extent2D,
    output_extent: vk::Extent2D,
    retired: &mut RetireQueue<RetiredResource>,
) -> Result<(), String> {
    let render_size = [render_extent.width, render_extent.height];
    let output_size = [output_extent.width, output_extent.height];
    if temporal.dlss_extents == Some((render_size, output_size)) {
        if temporal.dlss.is_some() {
            return Ok(());
        }
        if temporal.dlss_attempted {
            return Err(temporal
                .dlss_error
                .clone()
                .unwrap_or_else(|| "DLSS view creation failed".to_owned()));
        }
    }
    if temporal.dlss_extents != Some((render_size, output_size)) {
        temporal.dlss_extents = Some((render_size, output_size));
        temporal.dlss_attempted = false;
        temporal.dlss_error = None;
    }
    if temporal.dlss.as_ref().is_some_and(|view| {
        view.render_extent == render_extent && view.output_extent == output_extent
    }) {
        return Ok(());
    }
    if let Some(previous) = temporal.dlss.take() {
        retired.retire(RetiredResource::DlssView(previous), 0);
    }
    temporal.dlss_attempted = true;
    temporal.dlss_error = None;
    match DlssView::new(
        instance,
        device,
        physical_device,
        render_extent,
        output_extent,
    ) {
        Ok(view) => {
            temporal.dlss = Some(Box::new(view));
            Ok(())
        }
        Err(error) => {
            let error = error.to_string();
            temporal.dlss_error = Some(error.clone());
            Err(error)
        }
    }
}

pub(crate) fn temporal_frame_delta_ms(previous: Option<Instant>, now: Instant) -> f32 {
    previous
        .and_then(|previous| now.checked_duration_since(previous))
        .map_or(1000.0 / 60.0, |elapsed| {
            (elapsed.as_secs_f32() * 1000.0).clamp(1.0, 1000.0)
        })
}

pub(crate) fn warn_temporal_fallback(backend: &str, warning_emitted: &mut bool, reason: &str) {
    if !*warning_emitted {
        eprintln!("{backend} unavailable; falling back to TAA: {reason}");
        *warning_emitted = true;
    }
}

/// Плоскости отсечения камеры из матрицы проекции для линеаризации глубины в DoF.
/// Формулы совпадают с `fsr_camera_parameters`; при неверной матрице берутся значения по умолчанию.
pub(crate) fn camera_near_far(projection: [[f32; 4]; 4]) -> [f32; 2] {
    let depth_scale = projection[2][2];
    let depth_offset = projection[3][2];
    let near = depth_offset / depth_scale;
    let far = depth_offset / (depth_scale + 1.0);
    if near.is_finite() && far.is_finite() && near > 0.0 && far > near {
        [near, far]
    } else {
        [0.1, 1000.0]
    }
}

#[cfg(windows)]
pub(crate) fn fsr_camera_parameters(projection: [[f32; 4]; 4]) -> [f32; 3] {
    let depth_scale = projection[2][2];
    let depth_offset = projection[3][2];
    let fov_scale = projection[1][1];
    let near = depth_offset / depth_scale;
    let far = depth_offset / (depth_scale + 1.0);
    let fov_y = 2.0 * (1.0 / fov_scale).atan();
    if near.is_finite()
        && far.is_finite()
        && fov_y.is_finite()
        && near > 0.0
        && far > near
        && fov_y > 0.0
        && fov_y < std::f32::consts::PI
    {
        [near, far, fov_y]
    } else {
        [0.1, 1000.0, std::f32::consts::FRAC_PI_3]
    }
}

pub(crate) fn temporal_key(
    render_extent: vk::Extent2D,
    output_extent: vk::Extent2D,
    anti_aliasing: AntiAliasing,
    debug_view: u32,
) -> TemporalKey {
    TemporalKey {
        render_extent: TemporalExtent {
            width: render_extent.width,
            height: render_extent.height,
        },
        output_extent: TemporalExtent {
            width: output_extent.width,
            height: output_extent.height,
        },
        anti_aliasing,
        debug_view,
    }
}

pub(crate) fn effective_view_settings(
    mut view: ViewSettings,
    debug_view: DebugView,
) -> ViewSettings {
    if debug_view != DebugView::Lit {
        view.anti_aliasing = AntiAliasing::None;
    }
    if debug_view.has_linear_output() {
        view.post.exposure.ev = 0.0;
        view.post.tonemapper = Tonemapper::None;
    }
    view
}

pub(crate) fn validate_debug_view(
    debug_view: DebugView,
    wireframe_supported: bool,
) -> Result<(), RenderError> {
    if debug_view == DebugView::Wireframe && !wireframe_supported {
        return Err(RenderError::InvalidConfig(
            "wireframe debug view requires fillModeNonSolid support",
        ));
    }
    Ok(())
}
