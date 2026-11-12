use super::ScreenShaders;
use super::culling::{Frustum, LocalBounds};
use super::{
    AcquireAction, AlphaMode, AntiAliasing, CLUSTER_BOUNDS_BUFFER_SIZE, CLUSTER_BOUNDS_GPU_SIZE,
    CLUSTER_BUFFER_SIZE, CLUSTER_COMPUTE_LOCAL_SIZE, CLUSTER_COMPUTE_SHADER, CLUSTER_COUNT,
    CLUSTER_GRID_X, CLUSTER_GRID_Y, CLUSTER_GRID_Z, CLUSTER_RECORD_GPU_SIZE,
    DEFAULT_UPLOAD_BATCH_CAPACITY, DebugView, DeviceFacts, DeviceReport, DirectionalLight,
    DrawCommand, FRAME_UNIFORMS_SIZE, FULLSCREEN_VERTEX_SHADER, Frame, FrameCamera, GpuLocalLight,
    IDENTITY_MATRIX, LDR_FORMAT, LOCAL_LIGHT_GPU_SIZE, LOCAL_SHADOW_MAP_RESOLUTION,
    LOCAL_SHADOW_MATRIX_BUFFER_SIZE, LocalLight, LocalLightShadow, LocalLightShadowMatrices,
    MATERIAL_UNIFORMS_SIZE, MAX_CLUSTER_LIGHTS, MAX_LOCAL_SHADOW_LAYERS, MAX_RENDERED_LIGHTS,
    MESH_BLEND_FRAGMENT_SHADER, MESH_MASK_FRAGMENT_SHADER, MESH_OPAQUE_FRAGMENT_SHADER,
    MESH_VERTEX_SHADER, Material, MaterialId, MeshId, NO_GRAPHICS_COMPUTE_QUEUE_FAMILY_REASON,
    OFFSCREEN_FORMAT, OUTPUT_FRAGMENT_SHADER, OVERLAY_FRAGMENT_SHADER, OVERLAY_VERTEX_SHADER,
    OverlayVertex, PIPELINE_CACHE_HEADER_SIZE, PUSH_CONSTANTS_SIZE, PipelineCacheIdentity,
    PresentMode, ProbeInstanceFailure, RenderError, Renderer, RendererConfig, SHADOW_CASCADE_COUNT,
    SHADOW_FRAGMENT_SHADER, SHADOW_MAP_RESOLUTION, SHADOW_VERTEX_SHADER, SWAPCHAIN_USAGE,
    TONEMAP_FRAGMENT_SHADER, TemporalBackend, TextureColorSpace, TextureId, Tonemapper,
    UI_FRAGMENT_SHADER, UI_VERTEX_SHADER, VELOCITY_FORMAT, Vertex, ViewSettings,
    align_upload_offset, best_device_index, check_extensions, classify_acquire_result,
    classify_probe_instance_failure, cluster_device_limit_reasons, deduplicate_names,
    derivative_tangent_basis, draw_rasterization_state, effective_view_settings, evaluate_device,
    frame_uniform_bytes, index_bytes, index_local_light_shadows, inverse_extent, local_light_bytes,
    local_light_bytes_for_selected, local_light_contribution, local_shadow_matrix_bytes,
    material_sampler_info, material_uniform_bytes, mesh_upload_layout, mip_extent, mip_level_count,
    missing_extensions, missing_offscreen_format_features, missing_post_format_features,
    missing_texture_format_features, missing_velocity_format_features, normal_matrix,
    offscreen_previous_use, offscreen_sampled_stages, overlay_vertex_bytes,
    point_shadow_face_index, push_constant_bytes, sampler_anisotropy, scaled_extent,
    scene_viewport, select_composite_alpha, select_local_light_entries, select_local_lights,
    select_local_shadow_slots, select_present_mode, select_surface_extent, select_temporal_backend,
    shadow_array_limits_suitable, shadow_caster_is_outside, shadow_casters, shadow_layout_source,
    shadow_subresource_range, texture_format, texture_format_reasons, texture_mip_blit_region,
    texture_mip_destination_shader_barrier, texture_mip_shader_barrier, texture_mip_source_barrier,
    texture_staging_size, texture_upload_barrier, timestamp_duration, transform_tangent_normal,
    upload_batch_capacity, upload_batch_flush_required, upload_data_layout, usable_surface_extent,
    validate_debug_view, validate_draws, validate_material, validate_mesh, validate_overlay,
    validate_pipeline_cache, validate_render_scale, validate_texture, validate_view_settings,
    vertex_bytes,
};
use super::{Exposure, PostProcess};
use super::{
    GpuPass, GpuProfileLayout, GpuViewTimings, LocalShadowKind, MAX_PROFILED_VIEWS,
    OUTPUT_QUERY_FIRST, ProfiledView, SelectedLocalLight, TIMESTAMP_QUERY_COUNT, VIEW_QUERY_COUNT,
    ViewportId, available_timestamps, decode_output, decode_view,
};
use super::{
    INSTANCE_STRIDE, MESH_INSTANCE_BINDING, MESH_INSTANCE_VECTORS, SHADOW_INSTANCE_BINDING,
    batch_draws, instance_attributes, instance_binding, pack_instances,
};
use ash::vk;
use std::time::{Duration, Instant};
use std::{
    collections::HashSet,
    ffi::{CStr, CString},
};

fn assert_send<T: Send>() {}

#[test]
fn renderer_is_send() {
    assert_send::<Renderer>();
}

#[test]
fn unavailable_temporal_backends_fall_back_to_taa() {
    assert_eq!(
        select_temporal_backend(AntiAliasing::Fsr, false, false),
        TemporalBackend::Taa
    );
    assert_eq!(
        select_temporal_backend(AntiAliasing::Fsr, true, false),
        TemporalBackend::Fsr
    );
    assert_eq!(
        select_temporal_backend(AntiAliasing::Dlss, false, false),
        TemporalBackend::Taa
    );
    assert_eq!(
        select_temporal_backend(AntiAliasing::Dlss, false, true),
        TemporalBackend::Dlss
    );
}

#[test]
fn fsr_delta_time_uses_last_submitted_frame_and_clamps_invalid_intervals() {
    let now = Instant::now();
    assert!((super::temporal_frame_delta_ms(None, now) - 1000.0 / 60.0).abs() < 1e-5);
    assert_eq!(
        super::temporal_frame_delta_ms(Some(now), now + Duration::from_millis(16)),
        16.0
    );
    assert_eq!(
        super::temporal_frame_delta_ms(Some(now), now + Duration::from_micros(100)),
        1.0
    );
    assert_eq!(
        super::temporal_frame_delta_ms(Some(now), now + Duration::from_secs(2)),
        1000.0
    );
}

#[cfg(windows)]
#[test]
fn fsr_camera_parameters_decode_vulkan_zero_to_one_projection() {
    let near = 0.1_f32;
    let far = 100.0_f32;
    let fov = std::f32::consts::FRAC_PI_3;
    let projection = [
        [1.0, 0.0, 0.0, 0.0],
        [0.0, 1.0 / (fov * 0.5).tan(), 0.0, 0.0],
        [0.0, 0.0, far / (near - far), -1.0],
        [0.0, 0.0, far * near / (near - far), 0.0],
    ];
    let parameters = super::fsr_camera_parameters(projection);
    assert!((parameters[0] - near).abs() < 1e-5);
    assert!((parameters[1] - far).abs() < 1e-3);
    assert!((parameters[2] - fov).abs() < 1e-5);
}

#[test]
fn debug_view_shader_codes_are_stable_and_distinct() {
    assert_eq!(
        [
            DebugView::Lit.code(),
            DebugView::Wireframe.code(),
            DebugView::Normals.code(),
            DebugView::Overdraw.code(),
            DebugView::LodLevels.code(),
            DebugView::ShadowCascades.code(),
        ],
        [0, 1, 2, 3, 4, 5]
    );
    let extent = vk::Extent2D {
        width: 640,
        height: 360,
    };
    assert_eq!(
        super::temporal_key(extent, extent, AntiAliasing::Taa, DebugView::Lit.code()).debug_view,
        DebugView::Lit.code()
    );
    assert_eq!(
        super::temporal_key(
            extent,
            extent,
            AntiAliasing::None,
            DebugView::ShadowCascades.code()
        )
        .debug_view,
        DebugView::ShadowCascades.code()
    );
}

#[test]
fn non_lit_debug_views_disable_aa_and_raw_views_bypass_tonemapping() {
    let settings = ViewSettings {
        render_scale: 0.5,
        post: PostProcess {
            exposure: Exposure {
                ev: 2.0,
                ..Exposure::default()
            },
            ..PostProcess::default()
        },
        anti_aliasing: AntiAliasing::Taa,
        screen_shaders: ScreenShaders::default(),
    };
    assert_eq!(effective_view_settings(settings, DebugView::Lit), settings);
    let wireframe = effective_view_settings(settings, DebugView::Wireframe);
    assert_eq!(wireframe.anti_aliasing, AntiAliasing::None);
    assert_eq!(wireframe.post.exposure.ev, 2.0);
    assert_eq!(wireframe.post.tonemapper, Tonemapper::Aces);
    for debug_view in [
        DebugView::Normals,
        DebugView::Overdraw,
        DebugView::LodLevels,
        DebugView::ShadowCascades,
    ] {
        let view = effective_view_settings(settings, debug_view);
        assert_eq!(view.anti_aliasing, AntiAliasing::None);
        assert_eq!(view.post.exposure.ev, 0.0);
        assert_eq!(view.post.tonemapper, Tonemapper::None);
    }
}

#[test]
fn wireframe_view_requires_optional_device_feature() {
    assert!(validate_debug_view(DebugView::Lit, false).is_ok());
    assert!(validate_debug_view(DebugView::Wireframe, true).is_ok());
    assert!(matches!(
        validate_debug_view(DebugView::Wireframe, false),
        Err(super::RenderError::InvalidConfig(_))
    ));
}

#[test]
fn present_mode_selection_uses_requested_mode_or_fifo_for_every_supported_set() {
    let modes = [
        (PresentMode::Fifo, vk::PresentModeKHR::FIFO),
        (PresentMode::Mailbox, vk::PresentModeKHR::MAILBOX),
        (PresentMode::Immediate, vk::PresentModeKHR::IMMEDIATE),
    ];
    for mask in 0..(1 << modes.len()) {
        let supported = modes
            .iter()
            .enumerate()
            .filter_map(|(index, (_, mode))| (mask & (1 << index) != 0).then_some(*mode))
            .collect::<Vec<_>>();
        for (requested, vulkan_mode) in modes {
            let expected = if supported.contains(&vulkan_mode) {
                vulkan_mode
            } else {
                vk::PresentModeKHR::FIFO
            };
            assert_eq!(select_present_mode(requested, &supported), expected);
        }
    }
    assert_eq!(
        select_present_mode(PresentMode::Immediate, &[]),
        vk::PresentModeKHR::FIFO
    );
}

#[test]
fn pipeline_cache_header_rejects_truncated_foreign_and_unsupported_data() {
    let identity = PipelineCacheIdentity {
        vendor_id: 0x1234,
        device_id: 0x5678,
        pipeline_cache_uuid: [0xabu8; vk::UUID_SIZE],
    };
    let mut cache = Vec::new();
    cache.extend_from_slice(&(PIPELINE_CACHE_HEADER_SIZE as u32).to_ne_bytes());
    cache.extend_from_slice(&vk::PipelineCacheHeaderVersion::ONE.as_raw().to_ne_bytes());
    cache.extend_from_slice(&identity.vendor_id.to_ne_bytes());
    cache.extend_from_slice(&identity.device_id.to_ne_bytes());
    cache.extend_from_slice(&identity.pipeline_cache_uuid);
    cache.extend_from_slice(b"driver data");

    assert!(validate_pipeline_cache(&[], identity).is_ok());
    assert!(validate_pipeline_cache(&cache, identity).is_ok());
    assert_eq!(super::compatible_pipeline_cache(&cache, identity), cache);
    assert!(super::compatible_pipeline_cache(&cache[..12], identity).is_empty());
    let foreign_identity = PipelineCacheIdentity {
        vendor_id: identity.vendor_id + 1,
        ..identity
    };
    assert!(super::compatible_pipeline_cache(&cache, foreign_identity).is_empty());
    assert!(validate_pipeline_cache(&cache[..PIPELINE_CACHE_HEADER_SIZE - 1], identity).is_err());

    let mut wrong_size = cache.clone();
    wrong_size[0..4].copy_from_slice(&0_u32.to_ne_bytes());
    assert!(validate_pipeline_cache(&wrong_size, identity).is_err());

    let mut wrong_version = cache.clone();
    wrong_version[4..8].copy_from_slice(&2_i32.to_ne_bytes());
    assert!(validate_pipeline_cache(&wrong_version, identity).is_err());

    for offset in [8, 12, 16] {
        let mut foreign = cache.clone();
        foreign[offset] ^= 1;
        assert!(validate_pipeline_cache(&foreign, identity).is_err());
    }
}

#[test]
fn texture_mip_chains_cover_square_odd_npot_and_thin_images() {
    let cases = [
        (8, 8, vec![(8, 8), (4, 4), (2, 2), (1, 1)]),
        (7, 5, vec![(7, 5), (3, 2), (1, 1)]),
        (13, 9, vec![(13, 9), (6, 4), (3, 2), (1, 1)]),
        (1, 1, vec![(1, 1)]),
        (1, 9, vec![(1, 9), (1, 4), (1, 2), (1, 1)]),
        (9, 1, vec![(9, 1), (4, 1), (2, 1), (1, 1)]),
    ];

    for (width, height, expected) in cases {
        let levels = mip_level_count(width, height);
        let actual = (0..levels)
            .map(|level| {
                let extent = mip_extent(width, height, level);
                (extent.width, extent.height)
            })
            .collect::<Vec<_>>();
        assert_eq!(levels, expected.len() as u32, "{width}x{height}");
        assert_eq!(actual, expected, "{width}x{height}");
    }
}

#[test]
fn texture_mip_blit_region_uses_adjacent_levels_and_floor_extents() {
    let region = texture_mip_blit_region(
        1,
        vk::Extent2D {
            width: 6,
            height: 4,
        },
        vk::Extent2D {
            width: 3,
            height: 2,
        },
    );

    assert_eq!(region.src_subresource.mip_level, 1);
    assert_eq!(region.dst_subresource.mip_level, 2);
    assert_eq!(region.src_subresource.layer_count, 1);
    assert_eq!(region.dst_subresource.layer_count, 1);
    assert_eq!(region.src_offsets[1].x, 6);
    assert_eq!(region.src_offsets[1].y, 4);
    assert_eq!(region.dst_offsets[1].x, 3);
    assert_eq!(region.dst_offsets[1].y, 2);
}

#[test]
fn texture_mip_barriers_cover_the_chain_and_publish_each_level() {
    let image = vk::Image::null();
    let levels = mip_level_count(16, 1);
    let initial = texture_upload_barrier(image, levels);
    assert_eq!(initial.image, image);
    assert_eq!(initial.subresource_range.base_mip_level, 0);
    assert_eq!(initial.subresource_range.level_count, levels);
    assert_eq!(initial.old_layout, vk::ImageLayout::UNDEFINED);
    assert_eq!(initial.new_layout, vk::ImageLayout::TRANSFER_DST_OPTIMAL);
    assert_eq!(initial.src_stage_mask, vk::PipelineStageFlags2::NONE);
    assert_eq!(initial.src_access_mask, vk::AccessFlags2::NONE);
    assert_eq!(
        initial.dst_stage_mask,
        vk::PipelineStageFlags2::COPY | vk::PipelineStageFlags2::BLIT
    );
    assert_eq!(initial.dst_access_mask, vk::AccessFlags2::TRANSFER_WRITE);
    assert_eq!(initial.src_queue_family_index, vk::QUEUE_FAMILY_IGNORED);
    assert_eq!(initial.dst_queue_family_index, vk::QUEUE_FAMILY_IGNORED);

    for mip_level in 0..levels - 1 {
        let range = super::color_mip_subresource_range(mip_level, 1);
        let source = texture_mip_source_barrier(image, range);
        assert_eq!(source.subresource_range.base_mip_level, mip_level);
        assert_eq!(source.subresource_range.level_count, 1);
        assert_eq!(source.old_layout, vk::ImageLayout::TRANSFER_DST_OPTIMAL);
        assert_eq!(source.new_layout, vk::ImageLayout::TRANSFER_SRC_OPTIMAL);
        assert_eq!(
            source.src_stage_mask,
            vk::PipelineStageFlags2::COPY | vk::PipelineStageFlags2::BLIT
        );
        assert_eq!(source.src_access_mask, vk::AccessFlags2::TRANSFER_WRITE);
        assert_eq!(source.dst_stage_mask, vk::PipelineStageFlags2::BLIT);
        assert_eq!(source.dst_access_mask, vk::AccessFlags2::TRANSFER_READ);
        assert_eq!(source.src_queue_family_index, vk::QUEUE_FAMILY_IGNORED);
        assert_eq!(source.dst_queue_family_index, vk::QUEUE_FAMILY_IGNORED);

        let sampled = texture_mip_shader_barrier(image, range);
        assert_eq!(sampled.subresource_range.base_mip_level, mip_level);
        assert_eq!(sampled.subresource_range.level_count, 1);
        assert_eq!(sampled.old_layout, vk::ImageLayout::TRANSFER_SRC_OPTIMAL);
        assert_eq!(
            sampled.new_layout,
            vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL
        );
        assert_eq!(sampled.src_stage_mask, vk::PipelineStageFlags2::BLIT);
        assert_eq!(sampled.src_access_mask, vk::AccessFlags2::TRANSFER_READ);
        assert_eq!(
            sampled.dst_stage_mask,
            vk::PipelineStageFlags2::FRAGMENT_SHADER
        );
        assert_eq!(
            sampled.dst_access_mask,
            vk::AccessFlags2::SHADER_SAMPLED_READ
        );
        assert_eq!(sampled.src_queue_family_index, vk::QUEUE_FAMILY_IGNORED);
        assert_eq!(sampled.dst_queue_family_index, vk::QUEUE_FAMILY_IGNORED);
    }

    let final_level = texture_mip_destination_shader_barrier(
        image,
        super::color_mip_subresource_range(levels - 1, 1),
    );
    assert_eq!(final_level.subresource_range.base_mip_level, levels - 1);
    assert_eq!(final_level.subresource_range.level_count, 1);
    assert_eq!(
        final_level.old_layout,
        vk::ImageLayout::TRANSFER_DST_OPTIMAL
    );
    assert_eq!(
        final_level.new_layout,
        vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL
    );
    assert_eq!(
        final_level.src_stage_mask,
        vk::PipelineStageFlags2::COPY | vk::PipelineStageFlags2::BLIT
    );
    assert_eq!(
        final_level.src_access_mask,
        vk::AccessFlags2::TRANSFER_WRITE
    );
    assert_eq!(
        final_level.dst_stage_mask,
        vk::PipelineStageFlags2::FRAGMENT_SHADER
    );
    assert_eq!(
        final_level.dst_access_mask,
        vk::AccessFlags2::SHADER_SAMPLED_READ
    );
    assert_eq!(final_level.src_queue_family_index, vk::QUEUE_FAMILY_IGNORED);
    assert_eq!(final_level.dst_queue_family_index, vk::QUEUE_FAMILY_IGNORED);
}

#[test]
fn output_attachment_transition_is_ordered_at_color_output() {
    let image = vk::Image::null();
    let barrier = super::output_attachment_barrier(image);

    assert_eq!(barrier.image, image);
    assert_eq!(
        barrier.src_stage_mask,
        vk::PipelineStageFlags2::COLOR_ATTACHMENT_OUTPUT
    );
    assert_eq!(barrier.src_access_mask, vk::AccessFlags2::NONE);
    assert_eq!(
        barrier.dst_stage_mask,
        vk::PipelineStageFlags2::COLOR_ATTACHMENT_OUTPUT
    );
    assert_eq!(
        barrier.dst_access_mask,
        vk::AccessFlags2::COLOR_ATTACHMENT_WRITE
    );
    assert_eq!(barrier.old_layout, vk::ImageLayout::UNDEFINED);
    assert_eq!(
        barrier.new_layout,
        vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL
    );
}

#[test]
fn material_sampler_enables_mip_filtering_and_optional_anisotropy() {
    let cases = [
        (false, 16.0, None),
        (true, 8.0, Some(8.0)),
        (true, 32.0, Some(16.0)),
    ];

    for (supported, device_limit, expected_anisotropy) in cases {
        let anisotropy = sampler_anisotropy(supported, device_limit);
        assert_eq!(anisotropy, expected_anisotropy);
        let info = material_sampler_info(anisotropy);
        assert_eq!(info.mag_filter, vk::Filter::LINEAR);
        assert_eq!(info.min_filter, vk::Filter::LINEAR);
        assert_eq!(info.mipmap_mode, vk::SamplerMipmapMode::LINEAR);
        assert_eq!(info.min_lod, 0.0);
        assert_eq!(info.max_lod, vk::LOD_CLAMP_NONE);
        assert_eq!(
            info.anisotropy_enable == vk::TRUE,
            expected_anisotropy.is_some()
        );
        assert_eq!(info.max_anisotropy, expected_anisotropy.unwrap_or(1.0));
    }
}

#[test]
fn texture_device_requirements_report_each_missing_format_feature() {
    let features = [
        (vk::FormatFeatureFlags::SAMPLED_IMAGE, "SAMPLED_IMAGE"),
        (
            vk::FormatFeatureFlags::SAMPLED_IMAGE_FILTER_LINEAR,
            "SAMPLED_IMAGE_FILTER_LINEAR",
        ),
        (vk::FormatFeatureFlags::TRANSFER_SRC, "TRANSFER_SRC"),
        (vk::FormatFeatureFlags::TRANSFER_DST, "TRANSFER_DST"),
        (vk::FormatFeatureFlags::BLIT_SRC, "BLIT_SRC"),
        (vk::FormatFeatureFlags::BLIT_DST, "BLIT_DST"),
    ];

    let required_features = features
        .iter()
        .fold(vk::FormatFeatureFlags::empty(), |required, (feature, _)| {
            required | *feature
        });
    assert!(missing_texture_format_features(required_features).is_empty());
    for (feature, name) in features {
        assert_eq!(
            missing_texture_format_features(required_features & !feature),
            [name]
        );
    }
}

#[test]
fn post_format_requires_attachment_sampling_linear_filtering_and_readback() {
    let required = vk::FormatFeatureFlags::COLOR_ATTACHMENT
        | vk::FormatFeatureFlags::SAMPLED_IMAGE
        | vk::FormatFeatureFlags::TRANSFER_SRC
        | vk::FormatFeatureFlags::SAMPLED_IMAGE_FILTER_LINEAR;
    assert_eq!(LDR_FORMAT, vk::Format::R8G8B8A8_SRGB);
    assert!(missing_post_format_features(required).is_empty());
    for (feature, name) in [
        (vk::FormatFeatureFlags::COLOR_ATTACHMENT, "COLOR_ATTACHMENT"),
        (vk::FormatFeatureFlags::SAMPLED_IMAGE, "SAMPLED_IMAGE"),
        (vk::FormatFeatureFlags::TRANSFER_SRC, "TRANSFER_SRC"),
        (
            vk::FormatFeatureFlags::SAMPLED_IMAGE_FILTER_LINEAR,
            "SAMPLED_IMAGE_FILTER_LINEAR",
        ),
    ] {
        assert_eq!(missing_post_format_features(required & !feature), [name]);
    }
}

#[test]
fn hdr_scene_format_requires_blending_and_linear_sampling() {
    let required = vk::FormatFeatureFlags::COLOR_ATTACHMENT
        | vk::FormatFeatureFlags::COLOR_ATTACHMENT_BLEND
        | vk::FormatFeatureFlags::SAMPLED_IMAGE
        | vk::FormatFeatureFlags::SAMPLED_IMAGE_FILTER_LINEAR
        | vk::FormatFeatureFlags::STORAGE_IMAGE;
    assert_eq!(OFFSCREEN_FORMAT, vk::Format::R16G16B16A16_SFLOAT);
    assert!(missing_offscreen_format_features(required).is_empty());
    for (feature, name) in [
        (vk::FormatFeatureFlags::COLOR_ATTACHMENT, "COLOR_ATTACHMENT"),
        (
            vk::FormatFeatureFlags::COLOR_ATTACHMENT_BLEND,
            "COLOR_ATTACHMENT_BLEND",
        ),
        (vk::FormatFeatureFlags::SAMPLED_IMAGE, "SAMPLED_IMAGE"),
        (
            vk::FormatFeatureFlags::SAMPLED_IMAGE_FILTER_LINEAR,
            "SAMPLED_IMAGE_FILTER_LINEAR",
        ),
        (vk::FormatFeatureFlags::STORAGE_IMAGE, "STORAGE_IMAGE"),
    ] {
        assert_eq!(
            missing_offscreen_format_features(required & !feature),
            [name]
        );
    }
}

#[test]
fn velocity_format_requires_color_attachment_and_sampling() {
    let required = vk::FormatFeatureFlags::COLOR_ATTACHMENT | vk::FormatFeatureFlags::SAMPLED_IMAGE;
    assert_eq!(VELOCITY_FORMAT, vk::Format::R16G16_SFLOAT);
    assert!(missing_velocity_format_features(required).is_empty());
    for (feature, name) in [
        (vk::FormatFeatureFlags::COLOR_ATTACHMENT, "COLOR_ATTACHMENT"),
        (vk::FormatFeatureFlags::SAMPLED_IMAGE, "SAMPLED_IMAGE"),
    ] {
        assert_eq!(
            missing_velocity_format_features(required & !feature),
            [name]
        );
    }
}

#[test]
fn device_report_explains_missing_hdr_blend_support() {
    let facts = DeviceFacts {
        api_version: vk::API_VERSION_1_3,
        dynamic_rendering: true,
        synchronization2: true,
        extended_dynamic_state: true,
        supported_extensions: HashSet::new(),
        graphics_queue_families: vec![true],
        compute_queue_families: vec![true],
        storage_image_extended_formats: true,
        offscreen_format_features: vk::FormatFeatureFlags::COLOR_ATTACHMENT
            | vk::FormatFeatureFlags::SAMPLED_IMAGE
            | vk::FormatFeatureFlags::SAMPLED_IMAGE_FILTER_LINEAR
            | vk::FormatFeatureFlags::STORAGE_IMAGE,
        velocity_format_features: vk::FormatFeatureFlags::COLOR_ATTACHMENT
            | vk::FormatFeatureFlags::SAMPLED_IMAGE,
        max_push_constants_size: 256,
    };
    let evaluation = evaluate_device(&facts, &[]);
    assert_eq!(evaluation.reasons.len(), 1);
    assert!(evaluation.reasons[0].contains("R16G16B16A16_SFLOAT"));
    assert!(evaluation.reasons[0].contains("COLOR_ATTACHMENT_BLEND"));
}

#[test]
fn device_report_rejects_push_constant_limit_below_draw_layout() {
    let facts = DeviceFacts {
        api_version: vk::API_VERSION_1_3,
        dynamic_rendering: true,
        synchronization2: true,
        extended_dynamic_state: true,
        supported_extensions: HashSet::new(),
        graphics_queue_families: vec![true],
        compute_queue_families: vec![true],
        storage_image_extended_formats: true,
        offscreen_format_features: vk::FormatFeatureFlags::COLOR_ATTACHMENT
            | vk::FormatFeatureFlags::COLOR_ATTACHMENT_BLEND
            | vk::FormatFeatureFlags::SAMPLED_IMAGE
            | vk::FormatFeatureFlags::SAMPLED_IMAGE_FILTER_LINEAR
            | vk::FormatFeatureFlags::STORAGE_IMAGE,
        velocity_format_features: vk::FormatFeatureFlags::COLOR_ATTACHMENT
            | vk::FormatFeatureFlags::SAMPLED_IMAGE,
        max_push_constants_size: 15,
    };
    let reasons = evaluate_device(&facts, &[]).reasons;
    assert_eq!(reasons.len(), 1);
    assert!(reasons[0].contains("maxPushConstantsSize"));
    assert!(reasons[0].contains("16"));
}

#[test]
fn cluster_buffers_and_dispatch_fit_the_checked_device_limits() {
    assert_eq!(
        (CLUSTER_GRID_X, CLUSTER_GRID_Y, CLUSTER_GRID_Z),
        (16, 9, 24)
    );
    assert_eq!(CLUSTER_COUNT, 3_456);
    assert_eq!(MAX_CLUSTER_LIGHTS, 128);
    assert_eq!(CLUSTER_RECORD_GPU_SIZE, 516);
    assert_eq!(CLUSTER_BUFFER_SIZE, 1_783_296);
    assert_eq!(CLUSTER_BOUNDS_GPU_SIZE, 32);
    assert_eq!(CLUSTER_BOUNDS_BUFFER_SIZE, 110_592);
    assert_eq!(std::mem::size_of::<super::clusters::ClusterBounds>(), 32);
    assert_eq!(std::mem::size_of::<GpuLocalLight>(), 112);

    let mut limits = vk::PhysicalDeviceLimits {
        max_compute_work_group_invocations: CLUSTER_COMPUTE_LOCAL_SIZE,
        max_compute_work_group_size: [CLUSTER_COMPUTE_LOCAL_SIZE, 1, 1],
        max_compute_work_group_count: [54, 1, 1],
        max_storage_buffer_range: CLUSTER_BUFFER_SIZE as u32,
        max_per_stage_descriptor_storage_buffers: 3,
        max_descriptor_set_storage_buffers: 4,
        ..vk::PhysicalDeviceLimits::default()
    };
    assert!(cluster_device_limit_reasons(&limits).is_empty());

    limits.max_compute_work_group_count[0] -= 1;
    assert!(
        cluster_device_limit_reasons(&limits)
            .iter()
            .any(|reason| reason.contains("workgroup"))
    );
    limits.max_compute_work_group_count[0] += 1;
    limits.max_storage_buffer_range -= 1;
    assert!(
        cluster_device_limit_reasons(&limits)
            .iter()
            .any(|reason| reason.contains("maxStorageBufferRange"))
    );
    limits.max_storage_buffer_range += 1;
    limits.max_per_stage_descriptor_storage_buffers -= 1;
    assert!(
        cluster_device_limit_reasons(&limits)
            .iter()
            .any(|reason| reason.contains("storage buffer descriptors"))
    );
    limits.max_per_stage_descriptor_storage_buffers += 1;
    limits.max_descriptor_set_storage_buffers -= 1;
    assert!(
        cluster_device_limit_reasons(&limits)
            .iter()
            .any(|reason| reason.contains("storage buffer descriptors"))
    );
}

#[test]
fn shadow_array_device_limits_require_layer_count_and_directional_extent() {
    let mut properties = vk::ImageFormatProperties {
        max_extent: vk::Extent3D {
            width: SHADOW_MAP_RESOLUTION,
            height: SHADOW_MAP_RESOLUTION,
            depth: 1,
        },
        max_array_layers: MAX_LOCAL_SHADOW_LAYERS as u32,
        ..vk::ImageFormatProperties::default()
    };
    assert!(shadow_array_limits_suitable(&properties));
    properties.max_array_layers -= 1;
    assert!(!shadow_array_limits_suitable(&properties));
    properties.max_array_layers += 1;
    properties.max_extent.width -= 1;
    assert!(!shadow_array_limits_suitable(&properties));
}

#[test]
fn device_evaluation_requires_compute_on_the_selected_graphics_queue_and_extended_formats() {
    let mut facts = DeviceFacts {
        api_version: vk::API_VERSION_1_3,
        dynamic_rendering: true,
        synchronization2: true,
        extended_dynamic_state: true,
        supported_extensions: HashSet::new(),
        graphics_queue_families: vec![true],
        compute_queue_families: vec![false],
        storage_image_extended_formats: false,
        offscreen_format_features: vk::FormatFeatureFlags::COLOR_ATTACHMENT
            | vk::FormatFeatureFlags::COLOR_ATTACHMENT_BLEND
            | vk::FormatFeatureFlags::SAMPLED_IMAGE
            | vk::FormatFeatureFlags::SAMPLED_IMAGE_FILTER_LINEAR
            | vk::FormatFeatureFlags::STORAGE_IMAGE,
        velocity_format_features: vk::FormatFeatureFlags::COLOR_ATTACHMENT
            | vk::FormatFeatureFlags::SAMPLED_IMAGE,
        max_push_constants_size: 256,
    };
    let reasons = evaluate_device(&facts, &[]).reasons;
    assert!(reasons.contains(&NO_GRAPHICS_COMPUTE_QUEUE_FAMILY_REASON.to_owned()));
    assert!(reasons.contains(&"не поддерживается shaderStorageImageExtendedFormats".to_owned()));
    assert!(
        !reasons
            .iter()
            .any(|reason| reason.contains("STORAGE_IMAGE"))
    );

    facts.compute_queue_families[0] = true;
    facts.storage_image_extended_formats = true;
    facts.offscreen_format_features &= !vk::FormatFeatureFlags::STORAGE_IMAGE;
    let reasons = evaluate_device(&facts, &[]).reasons;
    assert!(!reasons.contains(&NO_GRAPHICS_COMPUTE_QUEUE_FAMILY_REASON.to_owned()));
    assert!(
        reasons
            .iter()
            .any(|reason| reason.contains("STORAGE_IMAGE"))
    );
}

#[test]
fn postprocess_formats_and_swapchain_usage_match_the_passes() {
    assert_eq!(OFFSCREEN_FORMAT, vk::Format::R16G16B16A16_SFLOAT);
    assert_eq!(LDR_FORMAT, vk::Format::R8G8B8A8_SRGB);
    assert_eq!(SWAPCHAIN_USAGE, vk::ImageUsageFlags::COLOR_ATTACHMENT);
}

#[test]
fn texture_formats_require_sampled_blit_and_transfer_support_in_both_spaces() {
    let required = vk::FormatFeatureFlags::SAMPLED_IMAGE
        | vk::FormatFeatureFlags::SAMPLED_IMAGE_FILTER_LINEAR
        | vk::FormatFeatureFlags::TRANSFER_SRC
        | vk::FormatFeatureFlags::TRANSFER_DST
        | vk::FormatFeatureFlags::BLIT_SRC
        | vk::FormatFeatureFlags::BLIT_DST;
    assert_eq!(
        texture_format(TextureColorSpace::Srgb),
        vk::Format::R8G8B8A8_SRGB
    );
    assert_eq!(
        texture_format(TextureColorSpace::Linear),
        vk::Format::R8G8B8A8_UNORM
    );
    assert!(texture_format_reasons(required, required).is_empty());
    let srgb_missing =
        texture_format_reasons(required & !vk::FormatFeatureFlags::BLIT_SRC, required);
    assert_eq!(srgb_missing.len(), 1);
    assert!(srgb_missing[0].contains("R8G8B8A8_SRGB"));
    let linear_missing =
        texture_format_reasons(required, required & !vk::FormatFeatureFlags::TRANSFER_DST);
    assert_eq!(linear_missing.len(), 1);
    assert!(linear_missing[0].contains("R8G8B8A8_UNORM"));
}

#[test]
fn offscreen_barrier_source_follows_the_previous_use_of_the_target() {
    assert_eq!(
        offscreen_previous_use(vk::ImageLayout::UNDEFINED),
        (vk::PipelineStageFlags2::NONE, vk::AccessFlags2::NONE)
    );
    assert_eq!(
        offscreen_previous_use(vk::ImageLayout::TRANSFER_SRC_OPTIMAL),
        (
            vk::PipelineStageFlags2::TRANSFER,
            vk::AccessFlags2::TRANSFER_READ
        )
    );
    assert_eq!(
        offscreen_previous_use(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL),
        (
            vk::PipelineStageFlags2::FRAGMENT_SHADER | vk::PipelineStageFlags2::COMPUTE_SHADER,
            vk::AccessFlags2::SHADER_SAMPLED_READ
        )
    );
    assert_eq!(
        offscreen_sampled_stages(),
        vk::PipelineStageFlags2::FRAGMENT_SHADER | vk::PipelineStageFlags2::COMPUTE_SHADER
    );
}

#[test]
fn device_evaluation_accepts_all_shared_requirements() {
    let facts = DeviceFacts {
        api_version: vk::make_api_version(0, 1, 3, 0),
        dynamic_rendering: true,
        synchronization2: true,
        extended_dynamic_state: true,
        supported_extensions: HashSet::from([
            "VK_KHR_swapchain".to_owned(),
            "VK_KHR_portability_subset".to_owned(),
        ]),
        graphics_queue_families: vec![false, true],
        compute_queue_families: vec![false, true],
        storage_image_extended_formats: true,
        offscreen_format_features: vk::FormatFeatureFlags::COLOR_ATTACHMENT
            | vk::FormatFeatureFlags::COLOR_ATTACHMENT_BLEND
            | vk::FormatFeatureFlags::SAMPLED_IMAGE
            | vk::FormatFeatureFlags::SAMPLED_IMAGE_FILTER_LINEAR
            | vk::FormatFeatureFlags::STORAGE_IMAGE,
        velocity_format_features: vk::FormatFeatureFlags::COLOR_ATTACHMENT
            | vk::FormatFeatureFlags::SAMPLED_IMAGE,
        max_push_constants_size: 256,
    };
    let required = [
        CString::new("VK_KHR_swapchain").unwrap(),
        CString::new("VK_KHR_portability_subset").unwrap(),
    ];

    let evaluation = evaluate_device(&facts, &required);

    assert!(evaluation.reasons.is_empty());
    assert_eq!(evaluation.graphics_queue_family_indices, [1]);
}

#[test]
fn device_evaluation_accepts_api_major_version_two() {
    let facts = DeviceFacts {
        api_version: vk::make_api_version(0, 2, 0, 0),
        dynamic_rendering: true,
        synchronization2: true,
        extended_dynamic_state: true,
        supported_extensions: HashSet::new(),
        graphics_queue_families: vec![true],
        compute_queue_families: vec![true],
        storage_image_extended_formats: true,
        offscreen_format_features: vk::FormatFeatureFlags::COLOR_ATTACHMENT
            | vk::FormatFeatureFlags::COLOR_ATTACHMENT_BLEND
            | vk::FormatFeatureFlags::SAMPLED_IMAGE
            | vk::FormatFeatureFlags::SAMPLED_IMAGE_FILTER_LINEAR
            | vk::FormatFeatureFlags::STORAGE_IMAGE,
        velocity_format_features: vk::FormatFeatureFlags::COLOR_ATTACHMENT
            | vk::FormatFeatureFlags::SAMPLED_IMAGE,
        max_push_constants_size: 256,
    };

    let evaluation = evaluate_device(&facts, &[]);

    assert!(evaluation.reasons.is_empty());
    assert_eq!(evaluation.graphics_queue_family_indices, [0]);
}

#[test]
fn probe_instance_failure_distinguishes_missing_driver_from_other_errors() {
    assert_eq!(
        classify_probe_instance_failure(vk::Result::ERROR_INCOMPATIBLE_DRIVER),
        ProbeInstanceFailure::NoDriver
    );
    assert_eq!(
        classify_probe_instance_failure(vk::Result::ERROR_INITIALIZATION_FAILED),
        ProbeInstanceFailure::NoDriver
    );
    assert_eq!(
        classify_probe_instance_failure(vk::Result::ERROR_UNKNOWN),
        ProbeInstanceFailure::Vulkan(vk::Result::ERROR_UNKNOWN)
    );
}

#[test]
fn device_evaluation_reports_each_missing_shared_requirement() {
    let facts = DeviceFacts {
        api_version: vk::make_api_version(0, 1, 2, 0),
        dynamic_rendering: false,
        synchronization2: false,
        extended_dynamic_state: false,
        supported_extensions: HashSet::new(),
        graphics_queue_families: vec![false, false],
        compute_queue_families: vec![false, false],
        storage_image_extended_formats: true,
        offscreen_format_features: vk::FormatFeatureFlags::COLOR_ATTACHMENT
            | vk::FormatFeatureFlags::COLOR_ATTACHMENT_BLEND
            | vk::FormatFeatureFlags::SAMPLED_IMAGE
            | vk::FormatFeatureFlags::SAMPLED_IMAGE_FILTER_LINEAR
            | vk::FormatFeatureFlags::STORAGE_IMAGE,
        velocity_format_features: vk::FormatFeatureFlags::COLOR_ATTACHMENT
            | vk::FormatFeatureFlags::SAMPLED_IMAGE,
        max_push_constants_size: 256,
    };
    let required = [
        CString::new("VK_KHR_swapchain").unwrap(),
        CString::new("VK_KHR_portability_subset").unwrap(),
    ];

    let evaluation = evaluate_device(&facts, &required);

    assert_eq!(
        evaluation.reasons,
        [
            "Vulkan API ниже 1.3",
            "не поддерживается dynamicRendering",
            "не поддерживается synchronization2",
            "не поддерживается extendedDynamicState",
            "device extension недоступен: VK_KHR_swapchain",
            "device extension недоступен: VK_KHR_portability_subset",
            "нет graphics queue family",
        ]
    );
    assert!(evaluation.graphics_queue_family_indices.is_empty());
}

#[test]
fn best_device_index_prefers_discrete_suitable_devices_and_keeps_first_tie() {
    let reports = [
        DeviceReport {
            name: "integrated".to_owned(),
            vendor_id: 1,
            api_version: 0,
            discrete: false,
            suitable: true,
            reasons: Vec::new(),
        },
        DeviceReport {
            name: "integrated tie".to_owned(),
            vendor_id: 2,
            api_version: 0,
            discrete: false,
            suitable: true,
            reasons: Vec::new(),
        },
        DeviceReport {
            name: "discrete".to_owned(),
            vendor_id: 3,
            api_version: 0,
            discrete: true,
            suitable: true,
            reasons: Vec::new(),
        },
        DeviceReport {
            name: "discrete tie".to_owned(),
            vendor_id: 4,
            api_version: 0,
            discrete: true,
            suitable: true,
            reasons: Vec::new(),
        },
        DeviceReport {
            name: "unsupported discrete".to_owned(),
            vendor_id: 5,
            api_version: 0,
            discrete: true,
            suitable: false,
            reasons: vec!["unsupported".to_owned()],
        },
    ];
    assert_eq!(
        best_device_index(&reports, |report| report.suitable, |report| report.discrete),
        Some(2)
    );
    assert_eq!(
        best_device_index(
            &reports[..2],
            |report| report.suitable,
            |report| report.discrete
        ),
        Some(0)
    );
    assert_eq!(
        best_device_index(
            &reports[2..4],
            |report| report.suitable,
            |report| report.discrete
        ),
        Some(0)
    );
    assert_eq!(
        best_device_index(
            &reports[4..],
            |report| report.suitable,
            |report| report.discrete
        ),
        None
    );
}

#[test]
fn acquire_result_classification_preserves_success_suboptimal_and_skip_actions() {
    assert_eq!(
        classify_acquire_result(Ok((3, false))),
        AcquireAction::Acquired {
            image_index: 3,
            suboptimal: false,
        }
    );
    assert_eq!(
        classify_acquire_result(Ok((2, true))),
        AcquireAction::Acquired {
            image_index: 2,
            suboptimal: true,
        }
    );
    assert_eq!(
        classify_acquire_result(Err(vk::Result::ERROR_OUT_OF_DATE_KHR)),
        AcquireAction::OutOfDate
    );
    for result in [vk::Result::TIMEOUT, vk::Result::NOT_READY] {
        assert_eq!(classify_acquire_result(Err(result)), AcquireAction::Skipped);
    }
    assert_eq!(
        classify_acquire_result(Err(vk::Result::ERROR_DEVICE_LOST)),
        AcquireAction::Failed(vk::Result::ERROR_DEVICE_LOST)
    );
}

#[test]
fn embedded_shaders_are_spirv_words() {
    for shader in [
        MESH_VERTEX_SHADER,
        MESH_OPAQUE_FRAGMENT_SHADER,
        MESH_MASK_FRAGMENT_SHADER,
        MESH_BLEND_FRAGMENT_SHADER,
        SHADOW_VERTEX_SHADER,
        SHADOW_FRAGMENT_SHADER,
        OVERLAY_VERTEX_SHADER,
        OVERLAY_FRAGMENT_SHADER,
        UI_VERTEX_SHADER,
        UI_FRAGMENT_SHADER,
        FULLSCREEN_VERTEX_SHADER,
        TONEMAP_FRAGMENT_SHADER,
        OUTPUT_FRAGMENT_SHADER,
        CLUSTER_COMPUTE_SHADER,
    ] {
        let words = ash::util::read_spv(&mut std::io::Cursor::new(shader)).unwrap();
        assert_eq!(words[0], 0x0723_0203);
    }
}

#[test]
fn completed_uploads_release_staging_without_a_frame() {
    use std::rc::Rc;
    let staging = Rc::new(());
    let mut batches = vec![
        Rc::clone(&staging),
        Rc::clone(&staging),
        Rc::clone(&staging),
    ];
    let completed =
        super::completed_upload_count([Ok::<_, ()>(true), Ok(true), Ok(false)]).unwrap();
    drop(batches.drain(..completed));
    assert_eq!(Rc::strong_count(&staging), 2);
    let completed = super::completed_upload_count([Ok::<_, ()>(true)]).unwrap();
    drop(batches.drain(..completed));
    assert_eq!(Rc::strong_count(&staging), 1);
    assert_eq!(super::completed_upload_count([Ok(false), Err(())]), Ok(0));
    assert_eq!(super::completed_upload_count([Ok(true), Err(())]), Err(()));
}

#[test]
fn back_face_reverses_the_complete_nonflat_mapped_normal() {
    let geometric = [0.0, 0.0, 1.0];
    let tangent_sample = [0.6, 0.0, 0.8];
    let front =
        transform_tangent_normal([1.0, 0.0, 0.0], [0.0, 1.0, 0.0], geometric, tangent_sample);
    let back = front.map(|component| -component);
    assert!((back[0] + 0.6).abs() < 1e-6);
    assert!((back[2] + 0.8).abs() < 1e-6);
    // Tie the numeric reference to the shader's operation order: the full
    // mapped normal and geometric fallback must both flip after TBN mapping.
    let shader = include_str!("../shaders/mesh.frag");
    let mapping = shader.find("vec3 normal = geometric_normal;").unwrap();
    let default_surface = shader.find("Surface rve_default_surface()").unwrap();
    assert!(default_surface > mapping);
    let geometric_flip = shader[default_surface..]
        .find("geometric_normal = -geometric_normal;")
        .unwrap();
    // Both flips are absolute offsets in the shader and must follow the TBN mapping they reverse.
    let perturb = shader.find("vec3 rve_perturb_normal(").unwrap();
    let perturb_mapping = perturb + shader[perturb..].find("normal = safe_normalize(").unwrap();
    let normal_flip = perturb + shader[perturb..].find("normal = -normal;").unwrap();
    assert!(normal_flip > perturb_mapping);
    let surface_mapping = shader
        .find("normal = rve_perturb_normal(tangent_normal);")
        .unwrap();
    let geometric_flip = default_surface + geometric_flip;
    assert!(geometric_flip > surface_mapping);
}

fn ggx_shader_has_analytic_peak(source: &str) -> bool {
    let normalized = source
        .chars()
        .filter(|character| !character.is_whitespace())
        .collect::<String>();
    let Some(function_start) = normalized.find("floatdistribution_ggx(") else {
        return false;
    };
    let Some(body_offset) = normalized[function_start..].find('{') else {
        return false;
    };
    let body_start = function_start + body_offset;
    let Some(body_length) = normalized[body_start..].find('}') else {
        return false;
    };
    let body = &normalized[body_start..=body_start + body_length];
    [
            "floatnormal_dot_half=clamp(normal_dot_half_input,0.0,1.0);",
            "floatalpha_squared=alpha*alpha;",
            "floatdenominator=(1.0-normal_dot_half*normal_dot_half)+alpha_squared*normal_dot_half*normal_dot_half;",
            "denominator=max(denominator,alpha_squared);",
            "returnalpha_squared/(PI*denominator*denominator);",
        ]
        .iter()
        .all(|expression| body.contains(expression))
            && !body.contains("max(PI*denominator*denominator,1e-7)")
}

#[test]
fn ggx_shader_preserves_the_analytic_low_roughness_peak() {
    const PI: f32 = std::f32::consts::PI;
    let shader = include_str!("../shaders/mesh.frag");
    let shader_lf = shader.replace("\r\n", "\n");
    assert!(ggx_shader_has_analytic_peak(shader));
    assert!(ggx_shader_has_analytic_peak(&shader_lf));

    let function_lf = "float distribution_ggx(float normal_dot_half_input, float alpha) {\n    float normal_dot_half = clamp(normal_dot_half_input, 0.0, 1.0);\n    float alpha_squared = alpha * alpha;\n    float denominator = (1.0 - normal_dot_half * normal_dot_half) + alpha_squared * normal_dot_half * normal_dot_half;\n    denominator = max(denominator, alpha_squared);\n    return alpha_squared / (PI * denominator * denominator);\n}";
    let function_crlf = function_lf.replace('\n', "\r\n");
    assert!(ggx_shader_has_analytic_peak(function_lf));
    assert!(ggx_shader_has_analytic_peak(&function_crlf));

    let shader_whitespace_variant = function_lf
        .replace("float distribution_ggx(", "float \t distribution_ggx (")
        .replace(
            "normal_dot_half * normal_dot_half",
            "normal_dot_half \t *\r\n normal_dot_half",
        );
    assert!(ggx_shader_has_analytic_peak(&shader_whitespace_variant));

    let clipped_shader = "float distribution_ggx(float normal_dot_half_input, float alpha) {\n    float normal_dot_half = clamp(normal_dot_half_input, 0.0, 1.0);\n    float alpha_squared = alpha * alpha;\n    float denominator = (1.0 - normal_dot_half * normal_dot_half) + alpha_squared * normal_dot_half * normal_dot_half;\n    denominator = max(denominator, alpha_squared);\n    return alpha_squared / max(PI * denominator * denominator, 1e-7);\n}";
    assert!(!ggx_shader_has_analytic_peak(clipped_shader));

    let distribution = |normal_dot_half: f32, roughness: f32| {
        let normal_dot_half = normal_dot_half.clamp(0.0, 1.0);
        let alpha = roughness * roughness;
        let alpha_squared = alpha * alpha;
        let denominator = ((1.0 - normal_dot_half * normal_dot_half)
            + alpha_squared * normal_dot_half * normal_dot_half)
            .max(alpha_squared);
        alpha_squared / (PI * denominator * denominator)
    };
    let roughnesses = [0.045, 0.1, 0.25, 1.0];
    let mut peaks = Vec::with_capacity(roughnesses.len());
    for roughness in roughnesses {
        let alpha = roughness * roughness;
        let alpha_squared = alpha * alpha;
        let expected_peak = 1.0 / (PI * alpha_squared);
        let peak = distribution(1.0, roughness);
        assert!(
            (peak / expected_peak - 1.0).abs() < 2e-5,
            "{roughness}: {peak}"
        );
        assert!(distribution(0.0, roughness).is_finite());
        assert!(distribution(-0.5, roughness).is_finite());
        assert!(distribution(1.5, roughness).is_finite());
        peaks.push(peak);
    }

    assert!(peaks[0] > peaks[1] && peaks[1] > peaks[2] && peaks[2] > peaks[3]);
    let low_roughness_shoulder = distribution(0.99, roughnesses[0]) / peaks[0];
    let next_shoulder = distribution(0.99, roughnesses[1]) / peaks[1];
    assert!(low_roughness_shoulder < next_shoulder);
    assert_eq!(
        distribution(-0.5, roughnesses[0]),
        distribution(0.0, roughnesses[0])
    );
    assert_eq!(
        distribution(1.5, roughnesses[0]),
        distribution(1.0, roughnesses[0])
    );
}

#[test]
fn mesh_validation_accepts_triangle_lists_inside_vertex_range() {
    assert!(validate_mesh(3, &[0, 1, 2]).is_ok());
    assert!(validate_mesh(4, &[0, 1, 2, 2, 3, 0]).is_ok());
}

#[test]
fn mesh_validation_rejects_empty_partial_and_out_of_range_input() {
    for (vertex_count, indices) in [
        (0, vec![0, 0, 0]),
        (3, vec![]),
        (3, vec![0, 1]),
        (3, vec![0, 1, 2, 0]),
        (3, vec![0, 1, 3]),
        (3, vec![0, 1, u32::MAX]),
    ] {
        assert!(
            matches!(
                validate_mesh(vertex_count, &indices),
                Err(RenderError::InvalidMesh(_))
            ),
            "{vertex_count} {indices:?}"
        );
    }
}

#[test]
fn upload_offsets_are_at_least_sixteen_and_multiple_of_device_alignment() {
    let layout = mesh_upload_layout(0, 44, 12, 256).unwrap();
    assert_eq!(layout.vertex_offset, 256);
    assert_eq!(layout.index_offset, 512);
    assert!(layout.vertex_offset >= 16 && layout.vertex_offset.is_multiple_of(256));
    assert!(layout.index_offset >= 16 && layout.index_offset.is_multiple_of(256));

    let non_power_of_two = align_upload_offset(0, 24).unwrap();
    assert_eq!(non_power_of_two, 48);
    assert!(non_power_of_two.is_multiple_of(24));
}

#[test]
fn upload_layout_rejects_offset_and_size_overflow() {
    assert_eq!(align_upload_offset(usize::MAX, 256), None);
    assert_eq!(mesh_upload_layout(0, usize::MAX, 4, 16), None);
    assert_eq!(upload_batch_capacity(usize::MAX, 16), None);
}

#[test]
fn upload_batches_flush_on_capacity_and_isolate_oversized_meshes() {
    assert_eq!(
        upload_batch_flush_required(16, 44, 12, 16, 128),
        Some(false)
    );
    assert_eq!(upload_batch_flush_required(80, 44, 12, 16, 128), Some(true));

    let oversized = mesh_upload_layout(0, DEFAULT_UPLOAD_BATCH_CAPACITY + 1, 12, 16).unwrap();
    let capacity = upload_batch_capacity(oversized.end, 16).unwrap();
    assert!(capacity > DEFAULT_UPLOAD_BATCH_CAPACITY);
    assert_eq!(
        upload_batch_flush_required(oversized.end, 44, 12, 16, capacity),
        Some(true)
    );
}

#[test]
fn texture_upload_budget_contains_only_base_level_rgba8_source_data() {
    let width = 4;
    let height = 4;
    let mip_levels = mip_level_count(width, height);
    let base_level_bytes = texture_staging_size(width, height).unwrap();
    let whole_chain_bytes = (0..mip_levels)
        .map(|mip| {
            let extent = mip_extent(width, height, mip);
            extent.width as usize * extent.height as usize * 4
        })
        .sum::<usize>();
    assert_eq!(base_level_bytes, 4 * 4 * 4);
    assert_eq!(whole_chain_bytes, 84);
    assert_eq!(base_level_bytes, 64);
    assert!(whole_chain_bytes > base_level_bytes);
    assert_eq!(upload_data_layout(0, base_level_bytes, 16), Some((16, 80)));
    assert_eq!(texture_staging_size(u32::MAX, u32::MAX), None);
}

const IDENTITY: [[f32; 4]; 4] = [
    [1.0, 0.0, 0.0, 0.0],
    [0.0, 1.0, 0.0, 0.0],
    [0.0, 0.0, 1.0, 0.0],
    [0.0, 0.0, 0.0, 1.0],
];

fn floats(bytes: &[u8]) -> Vec<f32> {
    bytes
        .as_chunks::<4>()
        .0
        .iter()
        .map(|chunk| f32::from_ne_bytes(*chunk))
        .collect()
}

fn perspective_projection() -> [[f32; 4]; 4] {
    let near = 0.1;
    let far = 20.0;
    [
        [1.0, 0.0, 0.0, 0.0],
        [0.0, 1.0, 0.0, 0.0],
        [0.0, 0.0, far / (near - far), -1.0],
        [0.0, 0.0, near * far / (near - far), 0.0],
    ]
}

fn unproject(projection: [[f32; 4]; 4], ndc: [f32; 3]) -> [f32; 3] {
    let inverse = super::environment::inverse4x4(projection).unwrap();
    let point = std::array::from_fn::<_, 4, _>(|row| {
        inverse[0][row] * ndc[0]
            + inverse[1][row] * ndc[1]
            + inverse[2][row] * ndc[2]
            + inverse[3][row]
    });
    [
        point[0] / point[3],
        point[1] / point[3],
        point[2] / point[3],
    ]
}

fn project(projection: [[f32; 4]; 4], point: [f32; 3]) -> [f32; 3] {
    let clip = std::array::from_fn::<_, 4, _>(|row| {
        projection[0][row] * point[0]
            + projection[1][row] * point[1]
            + projection[2][row] * point[2]
            + projection[3][row]
    });
    [clip[0] / clip[3], clip[1] / clip[3], clip[2] / clip[3]]
}

fn camera(position: [f32; 3]) -> FrameCamera {
    FrameCamera {
        view: IDENTITY,
        projection: perspective_projection(),
        position,
    }
}

fn point(position: [f32; 3], color_intensity: [f32; 3], range: f32) -> LocalLight {
    LocalLight::Point {
        position,
        color_intensity,
        range,
    }
}

fn spot(
    position: [f32; 3],
    direction: [f32; 3],
    range: f32,
    cos_inner_angle: f32,
    cos_outer_angle: f32,
) -> LocalLight {
    LocalLight::Spot {
        position,
        direction,
        color_intensity: [1.0; 3],
        range,
        cos_inner_angle,
        cos_outer_angle,
    }
}

#[test]
fn triangle_clip_space_camera_without_local_lights_can_prepare_a_frame() {
    for (width, height) in [(1600.0_f32, 900.0_f32), (900.0, 1600.0)] {
        // sandbox::scene::triangle_view_projection scales the wider axis.
        let aspect = width / height;
        let mut projection = IDENTITY_MATRIX;
        projection[0][0] = if aspect >= 1.0 { 1.0 / aspect } else { 1.0 };
        projection[1][1] = if aspect < 1.0 { aspect } else { 1.0 };
        let frame = Frame {
            particles: &[],
            particle_step: 0.0,
            clear_color: [0.0; 4],
            camera: FrameCamera {
                view: IDENTITY_MATRIX,
                projection,
                position: [0.0; 3],
            },
            environment: None,
            shadow_cascades: &[],
            light: DirectionalLight::default(),
            lights: &[],
            local_light_shadows: &[],
            draws: &[],
            overlay: &[],
            ui: &[],
            debug_view: DebugView::Lit,
            view: ViewSettings::default(),
            reset_history: false,

            joint_palettes: &[],
        };
        assert!(frame_uniform_bytes(&frame).is_ok());
    }
}

#[test]
fn scaled_camera_view_is_rejected_before_cluster_sphere_packing() {
    let mut view = IDENTITY_MATRIX;
    view[0][0] = 2.0;
    view[1][1] = 2.0;
    view[2][2] = 2.0;
    for light in [
        point([0.0, 0.0, -3.0], [1.0; 3], 1.0),
        spot([0.0, 0.0, -3.0], [0.0, 0.0, -1.0], 1.0, 0.8, 0.5),
    ] {
        assert!(matches!(
            local_light_bytes(&[light], view),
            Err(RenderError::InvalidConfig(_))
        ));
    }
    assert!(local_light_bytes(&[], view).is_ok());
}

#[test]
fn local_light_ssbo_uses_the_tested_std430_layout_and_valid_empty_binding() {
    assert_eq!(std::mem::size_of::<GpuLocalLight>(), 112);
    assert_eq!(std::mem::align_of::<GpuLocalLight>(), 16);
    assert_eq!(std::mem::offset_of!(GpuLocalLight, position_range), 0);
    assert_eq!(std::mem::offset_of!(GpuLocalLight, direction_type), 16);
    assert_eq!(std::mem::offset_of!(GpuLocalLight, color_intensity), 32);
    assert_eq!(std::mem::offset_of!(GpuLocalLight, cone_params), 48);
    assert_eq!(std::mem::offset_of!(GpuLocalLight, view_position_range), 64);
    assert_eq!(std::mem::offset_of!(GpuLocalLight, view_direction), 80);
    assert_eq!(std::mem::offset_of!(GpuLocalLight, shadow_params), 96);
    assert_eq!(local_light_bytes(&[], IDENTITY_MATRIX).unwrap().len(), 112);
    assert_eq!(
        floats(&local_light_bytes(&[], IDENTITY_MATRIX).unwrap()),
        [0.0; 28]
    );

    let packed = local_light_bytes(
        &[
            point([1.0, 2.0, 3.0], [4.0, 5.0, 6.0], 7.0),
            spot([8.0, 9.0, 10.0], [0.0, 0.0, -1.0], 11.0, 0.8, 0.5),
        ],
        IDENTITY_MATRIX,
    )
    .unwrap();
    assert_eq!(packed.len(), 2 * 112);
    let values = floats(&packed);
    assert_eq!(
        &values[0..16],
        &[
            1.0, 2.0, 3.0, 7.0, 0.0, 0.0, 0.0, 0.0, 4.0, 5.0, 6.0, 0.0, 0.0, 0.0, 0.0, 0.0
        ]
    );
    assert_eq!(
        &values[28..44],
        &[
            8.0, 9.0, 10.0, 11.0, 0.0, 0.0, -1.0, 1.0, 1.0, 1.0, 1.0, 0.0, 0.8, 0.5, 0.0, 0.0
        ]
    );
    assert_eq!(&values[16..24], &[1.0, 2.0, 3.0, 7.0, 0.0, 0.0, 0.0, 0.0]);
    assert_eq!(
        &values[44..52],
        &[8.0, 9.0, 10.0, 11.0, 0.0, 0.0, -1.0, 0.0]
    );
}

#[test]
fn local_shadow_slot_budget_is_distance_ordered_and_stably_maps_point_faces_and_spots() {
    let lights = (0..6)
        .map(|index| {
            let distance = match index {
                0 | 1 => 1.0,
                other => other as f32,
            };
            point([distance, 0.0, 0.0], [1.0; 3], 10.0)
        })
        .chain((6..9).map(|index| {
            let distance = if index < 8 { 5.5 } else { 6.0 };
            spot([distance, 0.0, 0.0], [0.0, 0.0, -1.0], 10.0, 0.8, 0.5)
        }))
        .collect::<Vec<_>>();
    let marker = |value: f32| [[value; 4]; 4];
    let mut requests = (0..6)
        .map(|index| LocalLightShadow {
            light_index: index,
            matrices: LocalLightShadowMatrices::Point {
                matrices: std::array::from_fn(|face| {
                    marker(100.0 + index as f32 * 10.0 + face as f32)
                }),
            },
        })
        .collect::<Vec<_>>();
    requests.extend((6..9).map(|index| LocalLightShadow {
        light_index: index,
        matrices: LocalLightShadowMatrices::Spot {
            matrix: marker(200.0 + index as f32),
        },
    }));
    let indexed = index_local_light_shadows(&lights, &requests).unwrap();
    let selected = lights
        .iter()
        .copied()
        .enumerate()
        .map(|(source_index, light)| SelectedLocalLight {
            source_index,
            light,
        })
        .collect::<Vec<_>>();
    let plan = select_local_shadow_slots(&selected, &indexed, [0.0; 3]);

    for (source_index, layer_base) in [(0, 0), (1, 6), (2, 12), (3, 18), (4, 24)] {
        let slot = plan.assignments[source_index].unwrap();
        assert_eq!(slot.layer_base, layer_base);
        assert_eq!(slot.kind, LocalShadowKind::Point);
    }
    assert!(plan.assignments[5].is_none());
    for (source_index, layer_base) in [(6, 30), (7, 31)] {
        let slot = plan.assignments[source_index].unwrap();
        assert_eq!(slot.layer_base, layer_base);
        assert_eq!(slot.kind, LocalShadowKind::Spot);
    }
    assert!(plan.assignments[8].is_none());
    assert_eq!(plan.matrices.len(), MAX_LOCAL_SHADOW_LAYERS);
    assert_eq!(plan.matrices[0][0][0], 100.0);
    assert_eq!(plan.matrices[6][0][0], 110.0);
    assert_eq!(plan.matrices[30][0][0], 206.0);
    assert_eq!(plan.matrices[31][0][0], 207.0);
}

#[test]
fn local_shadow_flags_follow_light_indices_after_contribution_sort_and_cap() {
    let lights = (0..=MAX_RENDERED_LIGHTS)
        .map(|index| point([0.0, 0.0, -2.0], [index as f32 + 1.0; 3], 8.0))
        .collect::<Vec<_>>();
    let requests = [0, MAX_RENDERED_LIGHTS].map(|light_index| LocalLightShadow {
        light_index,
        matrices: LocalLightShadowMatrices::Point {
            matrices: [IDENTITY; 6],
        },
    });
    let indexed = index_local_light_shadows(&lights, &requests).unwrap();
    let selected = select_local_light_entries(camera([0.0; 3]), &lights, [0.0; 2]).unwrap();
    assert_eq!(selected.len(), MAX_RENDERED_LIGHTS);
    assert_eq!(selected[0].source_index, MAX_RENDERED_LIGHTS);
    assert_eq!(selected[MAX_RENDERED_LIGHTS - 1].source_index, 1);

    let plan = select_local_shadow_slots(&selected, &indexed, [0.0; 3]);
    assert_eq!(plan.assignments[0].unwrap().layer_base, 0);
    assert!(plan.assignments.iter().skip(1).all(Option::is_none));
    assert_eq!(plan.matrices.len(), 6);
    let bytes = local_light_bytes_for_selected(&selected, &plan.assignments, IDENTITY).unwrap();
    let first_shadow_params = std::array::from_fn(|index| {
        u32::from_ne_bytes(bytes[96 + index * 4..100 + index * 4].try_into().unwrap())
    });
    assert_eq!(first_shadow_params, [0, 1, 1, 0]);
    let last_offset = (MAX_RENDERED_LIGHTS - 1) * LOCAL_LIGHT_GPU_SIZE + 96;
    let last_shadow_params = std::array::from_fn(|index| {
        let start = last_offset + index * 4;
        u32::from_ne_bytes(bytes[start..start + 4].try_into().unwrap())
    });
    assert_eq!(last_shadow_params, [0; 4]);
}

#[test]
fn local_shadow_request_indices_reject_duplicates_type_mismatches_and_nonfinite_matrices() {
    let lights = [point([0.0; 3], [1.0; 3], 2.0)];
    let point_shadow = LocalLightShadow {
        light_index: 0,
        matrices: LocalLightShadowMatrices::Point {
            matrices: [IDENTITY; 6],
        },
    };
    assert!(index_local_light_shadows(&lights, &[point_shadow]).is_ok());
    assert!(index_local_light_shadows(&lights, &[point_shadow, point_shadow]).is_err());
    assert!(
        index_local_light_shadows(
            &lights,
            &[LocalLightShadow {
                light_index: 1,
                ..point_shadow
            }]
        )
        .is_err()
    );
    assert!(
        index_local_light_shadows(
            &lights,
            &[LocalLightShadow {
                matrices: LocalLightShadowMatrices::Spot { matrix: IDENTITY },
                ..point_shadow
            }]
        )
        .is_err()
    );
    let mut invalid = IDENTITY;
    invalid[0][0] = f32::NAN;
    assert!(
        index_local_light_shadows(
            &lights,
            &[LocalLightShadow {
                matrices: LocalLightShadowMatrices::Point {
                    matrices: [invalid; 6],
                },
                ..point_shadow
            }]
        )
        .is_err()
    );
}

#[test]
fn local_shadow_matrix_storage_keeps_64_byte_layer_stride_and_finite_padding() {
    assert_eq!(LOCAL_SHADOW_MAP_RESOLUTION, 1024);
    assert_eq!(LOCAL_SHADOW_MATRIX_BUFFER_SIZE, 32 * 64);
    let first = [[2.0; 4]; 4];
    let second = [[3.0; 4]; 4];
    let bytes = local_shadow_matrix_bytes(&[first, second]).unwrap();
    let values = floats(&bytes);
    assert_eq!(&values[..16], &[2.0; 16]);
    assert_eq!(&values[16..32], &[3.0; 16]);
    assert_eq!(&values[32..48], IDENTITY.as_flattened());
    assert!(local_shadow_matrix_bytes(&vec![IDENTITY; MAX_LOCAL_SHADOW_LAYERS + 1]).is_err());
    let mut invalid = IDENTITY;
    invalid[3][0] = f32::INFINITY;
    assert!(local_shadow_matrix_bytes(&[invalid]).is_err());
}

#[test]
fn point_shadow_face_selection_matches_shader_axis_and_tie_order() {
    for (direction, expected) in [
        ([1.0, 0.0, 0.0], 0),
        ([-1.0, 0.0, 0.0], 1),
        ([0.0, 1.0, 0.0], 2),
        ([0.0, -1.0, 0.0], 3),
        ([0.0, 0.0, 1.0], 4),
        ([0.0, 0.0, -1.0], 5),
        ([1.0, 1.0, 0.0], 0),
        ([0.0, 1.0, 1.0], 2),
        ([1.0, 0.0, 1.0], 0),
    ] {
        assert_eq!(point_shadow_face_index(direction), expected);
    }
    let shader = include_str!("../shaders/mesh.frag");
    assert!(shader.contains("magnitude.x >= magnitude.y && magnitude.x >= magnitude.z"));
    assert!(shader.contains("if (magnitude.y >= magnitude.z)"));
}

#[test]
fn local_shadow_matrix_and_light_layouts_match_all_shader_stages() {
    let fragment = include_str!("../shaders/mesh.frag");
    let compute = include_str!("../shaders/cluster.comp");
    let local_vertex = include_str!("../shaders/shadow.vert");
    for shader in [fragment, compute] {
        assert!(shader.contains("uvec4 shadow_params;"));
    }
    assert!(fragment.contains("layout(set = 0, binding = 6, std430)"));
    assert!(fragment.contains("layout(set = 0, binding = 7) uniform texture2DArray"));
    assert!(local_vertex.contains("layout(set = 0, binding = 6, std430)"));
    assert!(local_vertex.contains("mat4 matrices[MAX_LOCAL_SHADOW_LAYERS]"));
    assert_eq!(std::mem::offset_of!(GpuLocalLight, shadow_params), 96);
    assert_eq!(std::mem::size_of::<GpuLocalLight>(), 112);
}

#[test]
fn local_light_ssbo_preserves_world_shading_data_and_adds_view_space_culling_data() {
    let view = [
        [0.0, 1.0, 0.0, 0.0],
        [-1.0, 0.0, 0.0, 0.0],
        [0.0, 0.0, 1.0, 0.0],
        [-4.0, 2.0, 1.0, 1.0],
    ];
    let bytes = local_light_bytes(
        &[
            point([1.0, 2.0, 3.0], [1.0; 3], 4.0),
            spot([8.0, 9.0, 10.0], [1.0, 0.0, 0.0], 5.0, 0.8, 0.5),
        ],
        view,
    )
    .unwrap();
    let values = floats(&bytes);
    assert_eq!(&values[0..4], &[1.0, 2.0, 3.0, 4.0]);
    assert_eq!(&values[16..20], &[-6.0, 3.0, 4.0, 4.0]);
    assert_eq!(&values[28..32], &[8.0, 9.0, 10.0, 5.0]);
    assert_eq!(&values[44..48], &[-13.0, 10.0, 11.0, 5.0]);
    assert_eq!(&values[48..52], &[0.0, 1.0, 0.0, 0.0]);
}

#[test]
fn local_light_selection_uses_actual_projection_boundaries_and_both_shapes() {
    let camera = camera([0.0; 3]);
    let boundary = unproject(camera.projection, [1.0, 0.0, 0.5]);
    let projected_boundary = project(camera.projection, boundary);
    assert!((projected_boundary[0] - 1.0).abs() < 1e-5);
    assert!((projected_boundary[2] - 0.5).abs() < 1e-5);

    let point_on_boundary = point(boundary, [1.0; 3], 0.02);
    assert_eq!(
        select_local_lights(camera, &[point_on_boundary])
            .unwrap()
            .len(),
        1
    );
    let point_outside = point(
        [boundary[0] + 0.2, boundary[1], boundary[2]],
        [1.0; 3],
        0.02,
    );
    assert!(
        select_local_lights(camera, &[point_outside])
            .unwrap()
            .is_empty()
    );

    let camera_inside_sphere = point([0.0, 0.0, -0.5], [1.0; 3], 1.0);
    assert_eq!(
        select_local_lights(camera, &[camera_inside_sphere])
            .unwrap()
            .len(),
        1
    );
    let camera_inside_cone = spot([0.0, 0.0, -2.0], [0.0, 0.0, 1.0], 4.0, 0.9, 0.8);
    assert_eq!(
        select_local_lights(camera, &[camera_inside_cone])
            .unwrap()
            .len(),
        1
    );

    let outside_apex = [boundary[0] + 0.08, boundary[1], boundary[2]];
    let cone_reaches_boundary = spot(
        outside_apex,
        [-0.6, 0.0, -0.8],
        0.5,
        10.0_f32.to_radians().cos(),
        10.0_f32.to_radians().cos(),
    );
    assert_eq!(
        select_local_lights(camera, &[cone_reaches_boundary])
            .unwrap()
            .len(),
        1
    );
    let wide_cone_reaches_boundary = spot(outside_apex, [-1.0, 0.0, 0.0], 0.2, 0.0, 0.0);
    assert_eq!(
        select_local_lights(camera, &[wide_cone_reaches_boundary])
            .unwrap()
            .len(),
        1
    );
    let cone_points_away = spot(outside_apex, [1.0, 0.0, 0.0], 0.5, 0.99, 0.99);
    assert!(
        select_local_lights(camera, &[cone_points_away])
            .unwrap()
            .is_empty()
    );

    let wide_cone = spot(
        [0.0, 0.0, -1.0],
        [0.0, 0.0, -1.0],
        3.0,
        45.0_f32.to_radians().cos(),
        0.0,
    );
    assert_eq!(select_local_lights(camera, &[wide_cone]).unwrap().len(), 1);
    let rounded_90_degree_cone = spot(
        [0.0, 0.0, -1.0],
        [0.0, 0.0, -1.0],
        3.0,
        90.0_f32.to_radians().cos(),
        90.0_f32.to_radians().cos(),
    );
    let selected = select_local_lights(camera, &[rounded_90_degree_cone]).unwrap();
    assert!(matches!(
        selected.as_slice(),
        [LocalLight::Spot {
            cos_inner_angle: 0.0,
            cos_outer_angle: 0.0,
            ..
        }]
    ));
    let narrow_cone = spot([0.0, 0.0, -2.0], [0.0, 0.0, 1.0], 4.0, 1.0, 0.5);
    assert_eq!(
        select_local_lights(camera, &[narrow_cone]).unwrap().len(),
        1
    );
}

#[test]
fn local_light_selection_validates_every_input_and_caps_by_contribution_stably() {
    let camera = camera([0.0; 3]);
    let mut invalid_extra = vec![point([0.0, 0.0, -1.0], [1.0; 3], 2.0); MAX_RENDERED_LIGHTS];
    invalid_extra.push(point([0.0, 0.0, -1.0], [1.0; 3], f32::NAN));
    assert!(matches!(
        select_local_lights(camera, &invalid_extra),
        Err(RenderError::InvalidConfig(_))
    ));
    for invalid in [
        point([0.0, 0.0, -1.0], [-1.0, 1.0, 1.0], 2.0),
        point([f32::INFINITY, 0.0, -1.0], [1.0; 3], 2.0),
        spot([0.0, 0.0, -1.0], [0.0; 3], 2.0, 0.8, 0.5),
        spot([0.0, 0.0, -1.0], [0.0, 0.0, -1.0], 2.0, 1.001, 0.5),
        spot([0.0, 0.0, -1.0], [0.0, 0.0, -1.0], 2.0, 0.5, 0.8),
        spot([0.0, 0.0, -1.0], [0.0, 0.0, -1.0], 2.0, 0.8, -0.001),
    ] {
        assert!(matches!(
            select_local_lights(camera, &[invalid]),
            Err(RenderError::InvalidConfig(_))
        ));
    }

    let tied = (0..MAX_RENDERED_LIGHTS + 8)
        .map(|index| point([0.0, 0.0, 0.0], [1.0; 3], (index + 1) as f32))
        .collect::<Vec<_>>();
    let selected = select_local_lights(camera, &tied).unwrap();
    assert_eq!(selected.len(), MAX_RENDERED_LIGHTS);
    for (index, light) in selected.iter().enumerate() {
        assert_eq!(*light, point([0.0, 0.0, 0.0], [1.0; 3], (index + 1) as f32));
    }

    let sorted = select_local_lights(
        camera,
        &[
            point([0.0, 0.0, -5.0], [1.0; 3], 10.0),
            point([0.0, 0.0, -1.0], [1.0; 3], 10.0),
        ],
    )
    .unwrap();
    assert_eq!(sorted[0], point([0.0, 0.0, -1.0], [1.0; 3], 10.0));
}

#[test]
fn spot_cone_score_uses_light_ray_sign_and_equal_angles_are_a_hard_edge() {
    let forward = spot([0.0; 3], [0.0, 0.0, -1.0], 5.0, 0.8, 0.8);
    assert!(local_light_contribution([0.0, 0.0, -1.0], forward) > 0.0);
    assert_eq!(local_light_contribution([0.0, 0.0, 1.0], forward), 0.0);
    let extreme = spot(
        [-f32::MAX, -f32::MAX, 0.0],
        [1.0, -1.0, 0.0],
        f32::MAX,
        0.9,
        0.8,
    );
    assert!(local_light_contribution([f32::MAX, f32::MAX, 0.0], extreme).is_finite());
}

fn transform_normal(matrix: &[[f32; 4]; 4], normal: [f32; 3]) -> [f32; 3] {
    let [x, y, z] = std::array::from_fn(|row| {
        (0..3)
            .map(|column| matrix[column][row] * normal[column])
            .sum::<f32>()
    });
    let length = (x * x + y * y + z * z).sqrt();
    [x / length, y / length, z / length]
}

fn assert_close(actual: [f32; 3], expected: [f32; 3]) {
    assert!(
        actual
            .iter()
            .zip(expected)
            .all(|(a, e)| (a - e).abs() < 1e-5),
        "{actual:?} != {expected:?}"
    );
}

#[test]
fn shadow_pass_skips_draws_that_do_not_cast_shadows() {
    let draw = |mesh, casts_shadow| DrawCommand {
        mesh: MeshId(mesh),
        material: None,
        model: IDENTITY,
        previous_model: None,
        lod_level: 0,
        casts_shadow,

        skin: None,
    };
    let draws = [draw(0, true), draw(1, false), draw(2, true), draw(3, true)];
    let modes = [
        AlphaMode::Opaque,
        AlphaMode::Opaque,
        AlphaMode::Mask { cutoff: 0.4 },
        AlphaMode::Blend,
    ];
    let casters: Vec<_> = shadow_casters(&draws, |draw| modes[draw.mesh.0])
        .map(|(index, draw)| (index, draw.mesh.0))
        .collect();
    assert_eq!(casters, [(0, 0), (2, 2)], "индексы draw сохраняются");
}

#[test]
fn shadow_culling_uses_the_current_cascade_frustum() {
    let draw = DrawCommand {
        mesh: MeshId(0),
        material: None,
        model: IDENTITY,
        previous_model: None,
        lod_level: 0,
        casts_shadow: true,

        skin: None,
    };
    let bounds = LocalBounds {
        min: [-0.1, -0.1, 0.4],
        max: [0.1, 0.1, 0.6],
    };
    let first = Frustum::from_view_projection(IDENTITY);
    let mut second_matrix = IDENTITY;
    second_matrix[3][0] = -10.0;
    let second = Frustum::from_view_projection(second_matrix);

    assert!(!shadow_caster_is_outside(&draw, Some(bounds), Some(first)));
    assert!(shadow_caster_is_outside(&draw, Some(bounds), Some(second)));
    assert!(!shadow_caster_is_outside(&draw, None, Some(second)));
}

#[test]
fn shadow_array_has_four_depth_layers_and_tracks_sample_layout_dependencies() {
    let range = shadow_subresource_range();
    assert_eq!(range.layer_count, SHADOW_CASCADE_COUNT as u32);
    assert_eq!(range.aspect_mask, vk::ImageAspectFlags::DEPTH);
    assert_eq!(
        shadow_layout_source(vk::ImageLayout::UNDEFINED).0,
        vk::PipelineStageFlags2::NONE
    );
    assert_eq!(
        shadow_layout_source(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL),
        (
            vk::PipelineStageFlags2::FRAGMENT_SHADER,
            vk::AccessFlags2::SHADER_SAMPLED_READ,
        )
    );
    assert_eq!(SHADOW_MAP_RESOLUTION, 2048);
}

#[test]
fn draw_validation_rejects_unknown_meshes_and_materials() {
    let draw = |mesh, material: Option<usize>| DrawCommand {
        mesh: MeshId(mesh),
        material: material.map(MaterialId),
        model: IDENTITY,
        previous_model: None,
        lod_level: 0,
        casts_shadow: true,

        skin: None,
    };
    let slots = |count: usize| vec![Some(()); count];
    assert!(validate_draws(&[], &slots(0), &slots(1)).is_ok());
    assert!(validate_draws(&[draw(0, None), draw(1, Some(1))], &slots(2), &slots(2)).is_ok());
    assert!(matches!(
        validate_draws(&[draw(0, None), draw(2, None)], &slots(2), &slots(2)),
        Err(RenderError::InvalidMesh(_))
    ));
    assert!(matches!(
        validate_draws(&[draw(0, Some(2))], &slots(1), &slots(2)),
        Err(RenderError::InvalidMaterial(_))
    ));
    assert!(matches!(
        validate_draws(&[draw(0, None)], &slots(1), &slots(0)),
        Err(RenderError::InvalidMaterial(_))
    ));
}

#[test]
fn material_validation_checks_factors_texture_ids_and_color_spaces() {
    let spaces = [
        Some(TextureColorSpace::Srgb),
        Some(TextureColorSpace::Linear),
        Some(TextureColorSpace::Linear),
        Some(TextureColorSpace::Srgb),
        Some(TextureColorSpace::Linear),
    ];
    assert!(validate_material(&Material::default(), &spaces).is_ok());
    assert!(
        validate_material(
            &Material {
                base_color: [0.0, 1.0, 0.5],
                metallic: 1.0,
                roughness: 0.0,
                normal_scale: -3.0,
                ..Material::default()
            },
            &spaces
        )
        .is_ok()
    );
    for material in [
        Material {
            base_color: [f32::NAN, 0.0, 0.0],
            ..Material::default()
        },
        Material {
            base_color: [1.01, 0.0, 0.0],
            ..Material::default()
        },
        Material {
            metallic: f32::INFINITY,
            ..Material::default()
        },
        Material {
            roughness: -0.01,
            ..Material::default()
        },
        Material {
            normal_scale: f32::NAN,
            ..Material::default()
        },
        Material {
            opacity: -0.01,
            ..Material::default()
        },
        Material {
            occlusion_strength: f32::NAN,
            ..Material::default()
        },
        Material {
            emissive: [0.0, -0.1, 0.0],
            ..Material::default()
        },
        Material {
            emissive: [0.0, f32::INFINITY, 0.0],
            ..Material::default()
        },
        Material {
            alpha_mode: AlphaMode::Mask { cutoff: 1.01 },
            ..Material::default()
        },
    ] {
        assert!(matches!(
            validate_material(&material, &spaces),
            Err(RenderError::InvalidMaterial(_))
        ));
    }
    for material in [
        Material {
            base_color_texture: Some(TextureId(1)),
            ..Material::default()
        },
        Material {
            metallic_roughness_texture: Some(TextureId(0)),
            ..Material::default()
        },
        Material {
            normal_texture: Some(TextureId(0)),
            ..Material::default()
        },
        Material {
            normal_texture: Some(TextureId(3)),
            ..Material::default()
        },
        Material {
            emissive_texture: Some(TextureId(1)),
            ..Material::default()
        },
        Material {
            occlusion_texture: Some(TextureId(0)),
            ..Material::default()
        },
        Material {
            occlusion_texture: Some(TextureId(5)),
            ..Material::default()
        },
    ] {
        assert!(matches!(
            validate_material(&material, &spaces),
            Err(RenderError::InvalidMaterial(_))
        ));
    }
    assert!(
        validate_material(
            &Material {
                base_color_texture: Some(TextureId(0)),
                metallic_roughness_texture: Some(TextureId(1)),
                normal_texture: Some(TextureId(1)),
                emissive_texture: Some(TextureId(3)),
                occlusion_texture: Some(TextureId(4)),
                ..Material::default()
            },
            &spaces
        )
        .is_ok()
    );
}

#[test]
fn rasterization_state_uses_material_sidedness_and_model_determinant() {
    let identity = [
        [1.0, 0.0, 0.0, 0.0],
        [0.0, 1.0, 0.0, 0.0],
        [0.0, 0.0, 1.0, 0.0],
        [0.0, 0.0, 0.0, 1.0],
    ];
    let mirrored_x = [
        [-1.0, 0.0, 0.0, 0.0],
        [0.0, 1.0, 0.0, 0.0],
        [0.0, 0.0, 1.0, 0.0],
        [0.0, 0.0, 0.0, 1.0],
    ];
    let mirrored_xy = [
        [-1.0, 0.0, 0.0, 0.0],
        [0.0, -1.0, 0.0, 0.0],
        [0.0, 0.0, 1.0, 0.0],
        [0.0, 0.0, 0.0, 1.0],
    ];

    assert_eq!(
        draw_rasterization_state(false, &identity),
        (vk::CullModeFlags::BACK, vk::FrontFace::COUNTER_CLOCKWISE)
    );
    assert_eq!(
        draw_rasterization_state(true, &identity),
        (vk::CullModeFlags::NONE, vk::FrontFace::COUNTER_CLOCKWISE)
    );
    assert_eq!(
        draw_rasterization_state(false, &mirrored_x),
        (vk::CullModeFlags::BACK, vk::FrontFace::CLOCKWISE)
    );
    assert_eq!(
        draw_rasterization_state(true, &mirrored_x),
        (vk::CullModeFlags::NONE, vk::FrontFace::CLOCKWISE)
    );
    assert_eq!(
        draw_rasterization_state(false, &mirrored_xy),
        (vk::CullModeFlags::BACK, vk::FrontFace::COUNTER_CLOCKWISE)
    );
    assert!(!Material::default().double_sided);
}

#[test]
fn material_uniforms_pack_four_std140_vec4s_with_declared_offsets() {
    let material = Material {
        base_color: [0.2, 0.4, 0.8],
        metallic: 0.3,
        roughness: 0.7,
        normal_texture: Some(TextureId(2)),
        normal_scale: 1.5,
        emissive: [2.0, 3.0, 4.0],
        opacity: 0.6,
        alpha_mode: AlphaMode::Mask { cutoff: 0.25 },
        occlusion_strength: 0.7,
        ..Material::default()
    };
    assert_eq!(MATERIAL_UNIFORMS_SIZE, 64);
    assert_eq!(
        floats(&material_uniform_bytes(&material)),
        [
            0.2, 0.4, 0.8, 1.0, 0.3, 0.7, 1.5, 1.0, 2.0, 3.0, 4.0, 0.0, 0.6, 0.25, 1.0, 0.7
        ]
    );
    assert_eq!(
        &floats(&material_uniform_bytes(&Material::default()))[4..8],
        &[0.0, 1.0, 1.0, 0.0]
    );
    assert_eq!(
        &floats(&material_uniform_bytes(&Material::default()))[12..16],
        &[1.0, 0.0, 0.0, 1.0]
    );
}

#[test]
fn user_material_uniforms_are_sixteen_vec4s_and_keep_zero_defaults() {
    let mut material = Material::default();
    material.user_values[3] = [1.0, 2.0, 3.0, 4.0];
    let bytes = super::user_material_uniform_bytes(&material);
    assert_eq!(bytes.len(), 16 * 16);
    assert_eq!(floats(&bytes)[12..16], [1.0, 2.0, 3.0, 4.0]);
    assert_eq!(
        floats(&super::user_material_uniform_bytes(&Material::default())),
        [0.0; 64]
    );
}

#[test]
fn only_builtin_materials_with_zero_user_values_share_the_user_uniform() {
    let builtin = Material::default();
    assert!(super::uses_shared_user_uniform(&builtin));
    let mut with_values = Material::default();
    with_values.user_values[0] = [0.5, 0.0, 0.0, 0.0];
    assert!(!super::uses_shared_user_uniform(&with_values));
    let custom = Material {
        shader: Some(super::ShaderId(0)),
        ..Material::default()
    };
    assert!(!super::uses_shared_user_uniform(&custom));
}

#[test]
fn shader_material_validation_checks_shader_and_texture_color_space() {
    use super::{
        ShaderId, SurfaceProperty, SurfacePropertyKind, validate_shader_material_properties,
    };

    let material = Material {
        shader: Some(ShaderId(0)),
        user_textures: [Some(TextureId(0)), None, None, None, None, None, None, None],
        ..Material::default()
    };
    let properties = [SurfaceProperty {
        name: "albedo".to_owned(),
        kind: SurfacePropertyKind::Texture,
        slot: 0,
        color_space: Some(TextureColorSpace::Srgb),
        fallback: super::SurfaceTextureFallback::White,
    }];
    assert!(matches!(
        validate_shader_material_properties(&material, &[Some(TextureColorSpace::Linear)], None),
        Err(RenderError::InvalidShader(_))
    ));
    assert!(matches!(
        validate_shader_material_properties(
            &material,
            &[Some(TextureColorSpace::Linear)],
            Some(&properties)
        ),
        Err(RenderError::InvalidMaterial(_))
    ));
    assert!(
        validate_shader_material_properties(
            &material,
            &[Some(TextureColorSpace::Srgb)],
            Some(&properties)
        )
        .is_ok()
    );
}

#[test]
fn shader_cannot_be_destroyed_while_materials_reference_it() {
    use super::validate_shader_destroyable;
    assert!(validate_shader_destroyable(0).is_ok());
    assert!(matches!(
        validate_shader_destroyable(1),
        Err(RenderError::InvalidShader(_))
    ));
}

#[test]
fn indirect_batches_address_per_draw_commands_within_the_draw_count_limit() {
    use super::{MAX_DRAW_INDIRECT_COUNT, indirect_chunks};
    assert_eq!(indirect_chunks(3, 2).collect::<Vec<_>>(), [(60, 2)]);
    let long = indirect_chunks(1, MAX_DRAW_INDIRECT_COUNT + 5).collect::<Vec<_>>();
    assert_eq!(
        long,
        [
            (20, MAX_DRAW_INDIRECT_COUNT),
            (20 * (1 + u64::from(MAX_DRAW_INDIRECT_COUNT)), 5)
        ]
    );
    assert_eq!(indirect_chunks(0, 0).count(), 0);
}

#[test]
fn occlusion_culling_skips_empty_frames_and_overdraw_view() {
    use super::occlusion_culling_enabled;
    let draw = DrawCommand {
        mesh: MeshId(0),
        material: None,
        model: IDENTITY,
        previous_model: None,
        lod_level: 0,
        casts_shadow: false,
        skin: None,
    };
    assert!(occlusion_culling_enabled(DebugView::Lit, &[draw]));
    assert!(occlusion_culling_enabled(DebugView::Wireframe, &[draw]));
    assert!(!occlusion_culling_enabled(DebugView::Overdraw, &[draw]));
    assert!(!occlusion_culling_enabled(DebugView::Lit, &[]));
}

#[test]
fn surface_pipeline_selection_covers_prepass_and_builtin_debug_draws() {
    use super::{
        DrawPass, DrawPipeline, PipelineChoice, ShaderId, SurfacePipelineVariant,
        select_pipeline_choice,
    };
    let shader = ShaderId(4);
    let custom = Some(shader);
    assert_eq!(
        select_pipeline_choice(None, AlphaMode::Opaque, DrawPass::OpaqueAndMask, false),
        Some(PipelineChoice::Builtin(DrawPipeline::Opaque))
    );
    assert_eq!(
        select_pipeline_choice(custom, AlphaMode::Opaque, DrawPass::OpaqueAndMask, true),
        Some(PipelineChoice::Surface(
            shader,
            SurfacePipelineVariant::OpaqueAfterPrepass
        ))
    );
    assert_eq!(
        select_pipeline_choice(
            custom,
            AlphaMode::Mask { cutoff: 0.5 },
            DrawPass::OpaqueAndMask,
            true
        ),
        Some(PipelineChoice::Surface(
            shader,
            SurfacePipelineVariant::Mask
        ))
    );
    assert_eq!(
        select_pipeline_choice(custom, AlphaMode::Blend, DrawPass::Blend, false),
        Some(PipelineChoice::Surface(
            shader,
            SurfacePipelineVariant::Blend
        ))
    );
    assert_eq!(
        select_pipeline_choice(custom, AlphaMode::Opaque, DrawPass::Wireframe, false),
        Some(PipelineChoice::Builtin(DrawPipeline::Wireframe))
    );
    assert_eq!(
        select_pipeline_choice(custom, AlphaMode::Blend, DrawPass::Prepass, false),
        None
    );
}

#[test]
fn derivative_tangent_basis_maps_v_down_texels_and_preserves_mirrored_orientation() {
    let normal = [0.0, 0.0, 1.0];
    let basis = derivative_tangent_basis(
        [1.0, 0.0, 0.0],
        [0.0, -1.0, 0.0],
        [1.0, 0.0],
        [0.0, 1.0],
        normal,
    )
    .unwrap();
    assert_close(basis.0, [1.0, 0.0, 0.0]);
    assert_close(basis.1, [0.0, 1.0, 0.0]);
    let texel = [0.5, 1.0, 0.5];
    let tangent_normal = [
        texel[0] * 2.0 - 1.0,
        texel[1] * 2.0 - 1.0,
        texel[2] * 2.0 - 1.0,
    ];
    assert_close(
        transform_tangent_normal(basis.0, basis.1, normal, tangent_normal),
        [0.0, 1.0, 0.0],
    );

    let mirrored = derivative_tangent_basis(
        [1.0, 0.0, 0.0],
        [0.0, -1.0, 0.0],
        [-1.0, 0.0],
        [0.0, 1.0],
        normal,
    )
    .unwrap();
    assert_close(mirrored.0, [-1.0, 0.0, 0.0]);
    assert_close(mirrored.1, [0.0, 1.0, 0.0]);
    assert_close(
        transform_tangent_normal(mirrored.0, mirrored.1, normal, [1.0, 0.0, 0.0]),
        [-1.0, 0.0, 0.0],
    );
    assert_close(
        transform_tangent_normal(mirrored.0, mirrored.1, normal, [0.0, 1.0, 0.0]),
        [0.0, 1.0, 0.0],
    );
    let degenerate = derivative_tangent_basis(
        [1.0, 0.0, 0.0],
        [0.0, -1.0, 0.0],
        [1.0, 0.0],
        [2.0, 0.0],
        normal,
    )
    .unwrap_or(([0.0; 3], [0.0; 3]));
    assert_close(
        transform_tangent_normal(degenerate.0, degenerate.1, normal, [0.0, 0.0, 1.0]),
        normal,
    );
    assert!(
        derivative_tangent_basis(
            [1.0, 0.0, 0.0],
            [0.0, -1.0, 0.0],
            [1.0, 0.0],
            [2.0, 0.0],
            normal,
        )
        .is_none()
    );
    assert!(
        derivative_tangent_basis([0.0; 3], [0.0; 3], [1.0, 0.0], [0.0, 1.0], normal,).is_none()
    );
}

#[test]
fn draw_validation_rejects_destroyed_meshes_and_materials() {
    let draw = |mesh, material: Option<usize>| DrawCommand {
        mesh: MeshId(mesh),
        material: material.map(MaterialId),
        model: IDENTITY,
        previous_model: None,
        lod_level: 0,
        casts_shadow: true,

        skin: None,
    };
    let alive_meshes = vec![Some(()), Some(())];
    let alive_materials = vec![Some(()), Some(())];
    let destroyed_mesh = vec![Some(()), None];
    let destroyed_material = vec![Some(()), None];
    assert!(matches!(
        validate_draws(&[draw(1, None)], &destroyed_mesh, &alive_materials),
        Err(RenderError::InvalidMesh(_))
    ));
    assert!(matches!(
        validate_draws(&[draw(0, Some(1))], &alive_meshes, &destroyed_material),
        Err(RenderError::InvalidMaterial(_))
    ));
}

#[test]
fn texture_validation_requires_rgba8_bytes_within_gpu_limits() {
    assert!(validate_texture(1, 1, 4, 16).is_ok());
    assert!(validate_texture(16, 2, 128, 16).is_ok());
    for (width, height, bytes) in [
        (0, 1, 0),
        (1, 0, 0),
        (17, 1, 68),
        (1, 17, 68),
        (2, 2, 15),
        (2, 2, 17),
        (2, 2, 12),
    ] {
        assert!(
            matches!(
                validate_texture(width, height, bytes, 16),
                Err(RenderError::InvalidTexture(_))
            ),
            "{width}x{height} {bytes}"
        );
    }
    assert!(validate_texture(u32::MAX, u32::MAX, 0, u32::MAX).is_err());
}

#[test]
fn vertex_layout_matches_shader_inputs() {
    assert_eq!(size_of::<Vertex>(), 44);
    assert_eq!(std::mem::offset_of!(Vertex, position), 0);
    assert_eq!(std::mem::offset_of!(Vertex, normal), 12);
    assert_eq!(std::mem::offset_of!(Vertex, uv), 24);
    assert_eq!(std::mem::offset_of!(Vertex, color), 32);
    let vertices = [Vertex {
        position: [1.0, 2.0, 3.0],
        normal: [4.0, 5.0, 6.0],
        uv: [7.0, 8.0],
        color: [9.0, 10.0, 11.0],
    }];
    assert_eq!(
        floats(vertex_bytes(&vertices)),
        (1..=11).map(|value| value as f32).collect::<Vec<_>>()
    );
    assert_eq!(
        index_bytes(&[1, 2]),
        [1_u32.to_ne_bytes(), 2_u32.to_ne_bytes()].concat()
    );
}

#[test]
fn push_constants_hold_lod_and_shadow_layer() {
    let bytes = push_constant_bytes(6, 3);
    assert_eq!(PUSH_CONSTANTS_SIZE, 16);
    assert_eq!(u32::from_ne_bytes(bytes[..4].try_into().unwrap()), 6);
    assert_eq!(u32::from_ne_bytes(bytes[4..8].try_into().unwrap()), 3);
    assert_eq!(&bytes[8..], &[0; 8]);
}

fn instance_draw(model: [[f32; 4]; 4], previous_model: Option<[[f32; 4]; 4]>) -> DrawCommand {
    DrawCommand {
        mesh: MeshId(0),
        material: None,
        model,
        previous_model,
        lod_level: 0,
        casts_shadow: true,
        skin: None,
    }
}

#[test]
fn instances_hold_model_normal_and_previous_model_column_by_column() {
    let mut model = IDENTITY;
    model[0][0] = 2.0;
    model[3] = [5.0, 6.0, 7.0, 1.0];
    let mut previous_model = IDENTITY;
    previous_model[3] = [2.0, 3.0, 4.0, 1.0];
    let (bytes, bases) = pack_instances([&[instance_draw(model, Some(previous_model))][..]]);
    assert_eq!(bases, [0]);
    assert_eq!(INSTANCE_STRIDE, 176);
    let values = floats(&bytes);
    assert_eq!(values.len(), 44);
    assert_eq!(&values[..4], &[2.0, 0.0, 0.0, 0.0]);
    assert_eq!(&values[12..16], &[5.0, 6.0, 7.0, 1.0]);
    // Матрица нормалей пропорциональна обратной транспонированной: масштаб X делит нормаль X.
    assert_eq!(&values[16..20], &[1.0, 0.0, 0.0, 0.0]);
    assert_eq!(&values[20..24], &[0.0, 2.0, 0.0, 0.0]);
    assert_eq!(&values[24..28], &[0.0, 0.0, 2.0, 0.0]);
    assert_eq!(&values[28..32], &[1.0, 0.0, 0.0, 0.0]);
    assert_eq!(&values[40..44], &[2.0, 3.0, 4.0, 1.0]);
}

#[test]
fn instances_reuse_model_without_previous_and_place_views_back_to_back() {
    let mut model = IDENTITY;
    model[3] = [1.0, 2.0, 3.0, 1.0];
    let first = [instance_draw(IDENTITY, None), instance_draw(model, None)];
    let second = [instance_draw(model, None)];
    let (bytes, bases) = pack_instances([&first[..], &[], &second[..]]);
    assert_eq!(bases, [0, 2, 2]);
    assert_eq!(bytes.len(), 3 * INSTANCE_STRIDE);
    let values = floats(&bytes[INSTANCE_STRIDE..2 * INSTANCE_STRIDE]);
    // Без previous_model прошлый кадр совпадает с текущим: motion vector нулевой.
    assert_eq!(&values[..16], &values[28..44]);
    assert_eq!(&values[12..16], &[1.0, 2.0, 3.0, 1.0]);
    assert_eq!(pack_instances([&[][..]]), (Vec::new(), vec![0]));
}

#[test]
fn instance_attributes_cover_mesh_and_shadow_shader_locations() {
    let mesh = instance_attributes(MESH_INSTANCE_BINDING, 5, MESH_INSTANCE_VECTORS)
        .map(|attribute| (attribute.location, attribute.binding, attribute.offset))
        .collect::<Vec<_>>();
    assert_eq!(mesh.len(), 11);
    assert_eq!(mesh.first(), Some(&(5, 2, 0)));
    // Локация 15 — последняя из 16, гарантированных Vulkan для vertex input.
    assert_eq!(mesh.last(), Some(&(15, 2, 160)));
    let shadow = instance_attributes(SHADOW_INSTANCE_BINDING, 2, 4)
        .map(|attribute| (attribute.location, attribute.offset))
        .collect::<Vec<_>>();
    assert_eq!(shadow, [(2, 0), (3, 16), (4, 32), (5, 48)]);
    let binding = instance_binding(MESH_INSTANCE_BINDING);
    assert_eq!(binding.stride, INSTANCE_STRIDE as u32);
    assert_eq!(binding.input_rate, vk::VertexInputRate::INSTANCE);
}

#[test]
fn batch_draws_merges_adjacent_draws_with_equal_state() {
    let batches = batch_draws([(0, 'a'), (1, 'a'), (2, 'a'), (3, 'b'), (4, 'b')])
        .map(|batch| (batch.first, batch.count, batch.key))
        .collect::<Vec<_>>();
    assert_eq!(batches, [(0, 3, 'a'), (3, 2, 'b')]);
}

#[test]
fn batch_draws_splits_on_culled_gaps_and_state_changes_without_reordering() {
    // Draw 2 отсечён, 4 с другим состоянием: экземпляры вызова должны идти подряд.
    let batches = batch_draws([(0, 'a'), (1, 'a'), (3, 'a'), (4, 'b'), (5, 'a')])
        .map(|batch| (batch.first, batch.count, batch.key))
        .collect::<Vec<_>>();
    assert_eq!(
        batches,
        [(0, 2, 'a'), (3, 1, 'a'), (4, 1, 'b'), (5, 1, 'a')]
    );
}

#[test]
fn batch_draws_keeps_sorted_blend_order_and_merges_only_ascending_runs() {
    // Сортировка от дальнего к ближнему: убывающие индексы не склеиваются.
    let batches = batch_draws([(5, 'a'), (4, 'a'), (1, 'a'), (2, 'a')])
        .map(|batch| (batch.first, batch.count))
        .collect::<Vec<_>>();
    assert_eq!(batches, [(5, 1), (4, 1), (1, 2)]);
    assert_eq!(batch_draws(std::iter::empty::<(usize, ())>()).count(), 0);
}

#[test]
fn normal_matrix_keeps_normals_perpendicular_under_non_uniform_scale() {
    // Плоскость x + y = 0 после растяжения по X вдвое имеет нормаль, пропорциональную (1, 2, 0).
    let mut model = IDENTITY;
    model[0][0] = 2.0;
    model[3] = [5.0, 6.0, 7.0, 1.0];
    let inv_sqrt5 = 1.0 / 5.0_f32.sqrt();
    assert_close(
        transform_normal(&normal_matrix(&model), [1.0, 1.0, 0.0]),
        [inv_sqrt5, 2.0 * inv_sqrt5, 0.0],
    );
    // Поворот на 90° вокруг Z переводит нормаль +X в +Y.
    let rotation = [
        [0.0, 1.0, 0.0, 0.0],
        [-1.0, 0.0, 0.0, 0.0],
        [0.0, 0.0, 1.0, 0.0],
        [0.0, 0.0, 0.0, 1.0],
    ];
    assert_close(
        transform_normal(&normal_matrix(&rotation), [1.0, 0.0, 0.0]),
        [0.0, 1.0, 0.0],
    );
}

#[test]
fn normal_matrix_keeps_outward_normals_for_mirrored_models() {
    let mut mirror = IDENTITY;
    mirror[0][0] = -1.0;
    // Зеркальная по X грань +X становится гранью -X, и нормаль должна смотреть в -X.
    assert_close(
        transform_normal(&normal_matrix(&mirror), [1.0, 0.0, 0.0]),
        [-1.0, 0.0, 0.0],
    );
    assert_close(
        transform_normal(&normal_matrix(&mirror), [0.0, 1.0, 0.0]),
        [0.0, 1.0, 0.0],
    );
}

#[test]
fn a_viewport_may_be_rendered_only_once_per_frame() {
    use super::{ViewportId, validate_scene_viewports};
    assert!(validate_scene_viewports([ViewportId(0), ViewportId(1)].into_iter()).is_ok());
    assert!(validate_scene_viewports([ViewportId(1), ViewportId(1)].into_iter()).is_err());
}

#[test]
fn frame_uniforms_pack_cascade_matrices_and_light_at_std140_offsets() {
    let matrix = |start: f32| {
        std::array::from_fn(|column| std::array::from_fn(|row| start + (column * 4 + row) as f32))
    };
    let light = DirectionalLight {
        direction: [1.0, 2.0, 3.0],
        color: [4.0, 5.0, 6.0],
        ambient: [7.0, 8.0, 9.0],
    };
    let cascades = [matrix(17.0), matrix(33.0), matrix(49.0), matrix(65.0)];
    let frame = Frame {
        particles: &[],
        particle_step: 0.0,
        clear_color: [0.0; 4],
        camera: FrameCamera {
            view: IDENTITY,
            projection: perspective_projection(),
            position: [10.0, 11.0, 12.0],
        },
        environment: None,
        shadow_cascades: &cascades,
        light,
        lights: &[],
        local_light_shadows: &[],
        draws: &[],
        overlay: &[],
        ui: &[],
        debug_view: DebugView::Lit,
        view: ViewSettings::default(),
        reset_history: false,

        joint_palettes: &[],
    };
    let values = floats(&frame_uniform_bytes(&frame).unwrap());
    assert_eq!(FRAME_UNIFORMS_SIZE, 720);
    let view_projection = perspective_projection();
    assert_eq!(&values[0..16], view_projection.as_flattened());
    assert_eq!(
        &values[16..32],
        super::environment::inverse4x4(view_projection)
            .unwrap()
            .as_flattened()
    );
    for (cascade, start) in [(0, 32), (1, 48), (2, 64), (3, 80)] {
        let expected = (1..=16)
            .map(|offset| (cascade as f32 * 16.0) + 17.0 + offset as f32 - 1.0)
            .collect::<Vec<_>>();
        assert_eq!(&values[start..start + 16], expected.as_slice());
        assert_eq!(cascades[cascade].as_flattened(), expected.as_slice());
    }
    assert_eq!(&values[96..100], &[1.0, 2.0, 3.0, 0.0]);
    assert_eq!(&values[100..104], &[4.0, 5.0, 6.0, 0.0]);
    assert_eq!(&values[104..108], &[7.0, 8.0, 9.0, 0.0]);
    assert_eq!(&values[108..112], &[1.0, 1.0 / 2048.0, 4.0, 0.0]);
    assert_eq!(&values[112..116], &[10.0, 11.0, 12.0, 0.0]);
    assert_eq!(&values[136..140], &[0.0; 4]);
    assert_eq!(&values[140..156], IDENTITY.as_flattened());
    assert!((values[156] - 0.1).abs() < 1e-5);
    assert!((values[157] - 20.0).abs() < 1e-3);
    assert_eq!(&values[158..160], &[1.0, 1.0]);
    assert_eq!(&values[160..176], view_projection.as_flattened());
    assert_eq!(&values[176..180], &[0.0; 4]);

    let normals = Frame {
        debug_view: DebugView::Normals,
        ..frame
    };
    assert_eq!(floats(&frame_uniform_bytes(&normals).unwrap())[179], 2.0);

    let no_shadows = Frame {
        shadow_cascades: &[],
        ..frame
    };
    assert_eq!(floats(&frame_uniform_bytes(&no_shadows).unwrap())[108], 0.0);
}

#[test]
fn temporal_frame_uniforms_pack_previous_unjittered_view_and_render_parameters() {
    let current = perspective_projection();
    let mut previous = current;
    previous[3][0] += 0.25;
    let key = super::temporal::TemporalKey {
        render_extent: super::temporal::Extent {
            width: 640,
            height: 360,
        },
        output_extent: super::temporal::Extent {
            width: 1280,
            height: 720,
        },
        anti_aliasing: AntiAliasing::Taa,
        debug_view: 0,
    };
    let mut state = super::temporal::TemporalState::default();
    let first = state.plan(key, previous, false);
    state.submitted(first);
    let plan = state.plan(key, current, false);
    let frame = Frame {
        particles: &[],
        particle_step: 0.0,
        clear_color: [0.0; 4],
        camera: FrameCamera {
            view: IDENTITY,
            projection: current,
            position: [0.0; 3],
        },
        environment: None,
        shadow_cascades: &[],
        light: DirectionalLight::default(),
        lights: &[],
        local_light_shadows: &[],
        draws: &[],
        overlay: &[],
        ui: &[],
        debug_view: DebugView::Lit,
        view: ViewSettings {
            anti_aliasing: AntiAliasing::Taa,
            screen_shaders: ScreenShaders::default(),
            ..ViewSettings::default()
        },
        reset_history: false,

        joint_palettes: &[],
    };
    let uniforms = super::prepare_frame_with_temporal(
        &frame,
        vk::Extent2D {
            width: 640,
            height: 360,
        },
        Some(plan),
    )
    .unwrap()
    .uniforms;
    let values = floats(&uniforms);
    assert_eq!(&values[0..16], current.as_flattened());
    assert_eq!(&values[160..176], previous.as_flattened());
    assert_eq!(
        &values[176..180],
        &[plan.jitter_ndc[0], plan.jitter_ndc[1], -1.0, 0.0]
    );
}

#[test]
fn frame_uniforms_reject_zero_light_or_non_finite_shadow_matrix() {
    for direction in [[0.0; 3], [f32::NAN, 0.0, 0.0], [f32::INFINITY, 0.0, 0.0]] {
        let light = DirectionalLight {
            direction,
            ..DirectionalLight::default()
        };
        assert!(
            matches!(
                frame_uniform_bytes(&Frame {
                    particles: &[],
                    particle_step: 0.0,
                    clear_color: [0.0; 4],
                    camera: FrameCamera {
                        view: IDENTITY,
                        projection: IDENTITY,
                        position: [0.0; 3],
                    },
                    environment: None,
                    shadow_cascades: &[],
                    light,
                    lights: &[],
                    local_light_shadows: &[],
                    draws: &[],
                    overlay: &[],
                    ui: &[],
                    debug_view: DebugView::Lit,
                    view: ViewSettings::default(),
                    reset_history: false,

                    joint_palettes: &[],
                }),
                Err(RenderError::InvalidConfig(_))
            ),
            "{direction:?}"
        );
    }

    let mut invalid = IDENTITY;
    invalid[2][1] = f32::NAN;
    let invalid_cascades = [invalid];
    let frame = Frame {
        particles: &[],
        particle_step: 0.0,
        clear_color: [0.0; 4],
        camera: FrameCamera {
            view: IDENTITY,
            projection: IDENTITY,
            position: [0.0; 3],
        },
        environment: None,
        shadow_cascades: &invalid_cascades,
        light: DirectionalLight::default(),
        lights: &[],
        local_light_shadows: &[],
        draws: &[],
        overlay: &[],
        ui: &[],
        debug_view: DebugView::Lit,
        view: ViewSettings::default(),
        reset_history: false,

        joint_palettes: &[],
    };
    assert!(matches!(
        frame_uniform_bytes(&frame),
        Err(RenderError::InvalidConfig(_))
    ));
}

#[test]
fn frame_uniforms_reject_more_than_four_shadow_cascades() {
    let too_many = [IDENTITY; 5];
    let frame = Frame {
        particles: &[],
        particle_step: 0.0,
        clear_color: [0.0; 4],
        camera: FrameCamera {
            view: IDENTITY,
            projection: IDENTITY,
            position: [0.0; 3],
        },
        environment: None,
        shadow_cascades: &too_many,
        light: DirectionalLight::default(),
        lights: &[],
        local_light_shadows: &[],
        draws: &[],
        overlay: &[],
        ui: &[],
        debug_view: DebugView::Lit,
        view: ViewSettings::default(),
        reset_history: false,

        joint_palettes: &[],
    };
    assert!(matches!(
        frame_uniform_bytes(&frame),
        Err(RenderError::InvalidConfig(_))
    ));
}

#[test]
fn scene_viewport_points_clip_space_y_up() {
    let viewport = scene_viewport(vk::Extent2D {
        width: 640,
        height: 360,
    });
    assert_eq!((viewport.x, viewport.y), (0.0, 360.0));
    assert_eq!((viewport.width, viewport.height), (640.0, -360.0));
    assert_eq!((viewport.min_depth, viewport.max_depth), (0.0, 1.0));
}

fn extension(name: &CStr) -> vk::ExtensionProperties {
    let mut property = vk::ExtensionProperties::default();
    for (destination, source) in property
        .extension_name
        .iter_mut()
        .zip(name.to_bytes_with_nul())
    {
        *destination = *source as _;
    }
    property
}

#[test]
fn default_config_has_no_extra_extensions_and_view_defaults_to_full_quality() {
    let config = RendererConfig::default();
    assert!(config.extra_instance_extensions.is_empty());
    assert!(config.extra_device_extensions.is_empty());
    let view = ViewSettings::default();
    assert_eq!(view.render_scale, 1.0);
    assert_eq!(view.post, PostProcess::default());
    assert_eq!(view.anti_aliasing, AntiAliasing::Fxaa);
    assert!(validate_view_settings(view).is_ok());
}

#[test]
fn postprocess_enum_codes_are_stable_for_the_shader_contract() {
    assert_eq!(Tonemapper::Aces.code(), 0);
    assert_eq!(Tonemapper::Neutral.code(), 1);
    assert_eq!(Tonemapper::None.code(), 2);
    assert_eq!(AntiAliasing::Fxaa.code(), 0);
    assert_eq!(AntiAliasing::None.code(), 1);
    assert_eq!(AntiAliasing::Taa.code(), 2);
    assert_eq!(AntiAliasing::Fsr.code(), 3);
    assert_eq!(AntiAliasing::Dlss.code(), 4);
}

#[test]
fn view_settings_validate_scale_and_exposure_ranges() {
    let post_with_ev = |ev: f32| {
        let mut post = PostProcess::default();
        post.exposure.ev = ev;
        post
    };
    for render_scale in [1.0, 0.5, f32::MIN_POSITIVE] {
        assert!(
            validate_view_settings(ViewSettings {
                render_scale,
                ..ViewSettings::default()
            })
            .is_ok()
        );
    }
    for exposure_ev in [-16.0, 0.0, 16.0] {
        assert!(
            validate_view_settings(ViewSettings {
                post: post_with_ev(exposure_ev),
                ..ViewSettings::default()
            })
            .is_ok()
        );
    }
    for render_scale in [0.0, f32::NAN, f32::INFINITY, f32::NEG_INFINITY, 1.01, -0.1] {
        assert!(matches!(
            validate_view_settings(ViewSettings {
                render_scale,
                ..ViewSettings::default()
            }),
            Err(RenderError::InvalidConfig(_))
        ));
    }
    for exposure_ev in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY, -16.01, 16.01] {
        assert!(matches!(
            validate_view_settings(ViewSettings {
                post: post_with_ev(exposure_ev),
                ..ViewSettings::default()
            }),
            Err(RenderError::InvalidConfig(_))
        ));
    }
}

#[test]
fn extension_names_are_deduplicated_preserving_request_order() {
    let first = CString::new("VK_KHR_surface").unwrap();
    let second = CString::new("VK_KHR_swapchain").unwrap();
    assert_eq!(
        deduplicate_names(vec![
            first.clone(),
            second.clone(),
            first.clone(),
            second.clone(),
        ]),
        vec![first, second]
    );
    assert!(deduplicate_names(Vec::new()).is_empty());
}

#[test]
fn extension_checks_accept_supported_and_empty_requests() {
    let available = [extension(c"VK_KHR_surface"), extension(c"VK_KHR_swapchain")];
    let requested = [c"VK_KHR_swapchain".to_owned()];

    assert!(check_extensions(&requested, &available, "device").is_ok());
    assert!(check_extensions(&[], &available, "instance").is_ok());
    assert!(check_extensions(&[], &[], "device").is_ok());
}

#[test]
fn missing_extensions_report_all_missing_names_and_their_kind() {
    let available = [extension(c"VK_KHR_surface")];
    let requested = [
        c"VK_KHR_surface".to_owned(),
        c"VK_KHR_swapchain".to_owned(),
        c"VK_KHR_portability_subset".to_owned(),
    ];
    let missing = missing_extensions(&requested, &available, "device");

    assert_eq!(missing.len(), 2);
    assert!(missing[0].contains("device"));
    assert!(missing[0].contains("VK_KHR_swapchain"));
    assert!(missing[1].contains("VK_KHR_portability_subset"));
    match check_extensions(&requested, &available, "device") {
        Err(RenderError::Unsupported(message)) => {
            for diagnostic in missing {
                assert!(message.contains(&diagnostic));
            }
        }
        result => panic!("expected unsupported extensions, got {result:?}"),
    }
    assert_eq!(missing_extensions(&requested, &[], "device").len(), 3);
}

#[test]
fn extension_names_require_an_exact_match() {
    let available = [extension(c"VK_KHR_surface_extra")];
    let requested = [c"VK_KHR_surface".to_owned()];
    assert!(matches!(
        check_extensions(&requested, &available, "instance"),
        Err(RenderError::Unsupported(_))
    ));
}

#[test]
fn composite_alpha_prefers_opaque_then_supported_fallbacks() {
    let modes = [
        vk::CompositeAlphaFlagsKHR::OPAQUE,
        vk::CompositeAlphaFlagsKHR::PRE_MULTIPLIED,
        vk::CompositeAlphaFlagsKHR::POST_MULTIPLIED,
        vk::CompositeAlphaFlagsKHR::INHERIT,
    ];
    for index in 0..modes.len() {
        let supported = modes[index..]
            .iter()
            .copied()
            .fold(vk::CompositeAlphaFlagsKHR::empty(), |all, mode| all | mode);
        assert_eq!(select_composite_alpha(supported), Some(modes[index]));
    }
    assert_eq!(
        select_composite_alpha(vk::CompositeAlphaFlagsKHR::empty()),
        None
    );
}

#[test]
fn native_scale_preserves_large_output_dimensions() {
    let output = vk::Extent2D {
        width: u32::MAX,
        height: u32::MAX - 1,
    };
    assert_eq!(scaled_extent(output, 1.0), output);
}

#[test]
fn variable_surface_extent_preserves_sizes_inside_limits() {
    let capabilities = vk::SurfaceCapabilitiesKHR {
        current_extent: vk::Extent2D {
            width: u32::MAX,
            height: u32::MAX,
        },
        min_image_extent: vk::Extent2D {
            width: 100,
            height: 100,
        },
        max_image_extent: vk::Extent2D {
            width: 1920,
            height: 1080,
        },
        ..Default::default()
    };
    for size in [
        capabilities.min_image_extent,
        vk::Extent2D {
            width: 1280,
            height: 720,
        },
        capabilities.max_image_extent,
    ] {
        assert_eq!(select_surface_extent(capabilities, size), size);
    }
}

#[test]
fn render_scale_accepts_finite_values_in_range() {
    assert!(validate_render_scale(1.0).is_ok());
    assert!(validate_render_scale(0.5).is_ok());
    assert!(validate_render_scale(f32::MIN_POSITIVE).is_ok());
}

#[test]
fn render_scale_rejects_zero_nan_and_infinity() {
    for scale in [0.0, f32::NAN, f32::INFINITY, f32::NEG_INFINITY, 1.01, -0.1] {
        assert!(matches!(
            validate_render_scale(scale),
            Err(RenderError::InvalidConfig(_))
        ));
    }
}

#[test]
fn scaled_extent_floors_and_clamps_dimensions_to_one() {
    assert_eq!(
        scaled_extent(
            vk::Extent2D {
                width: 1280,
                height: 721
            },
            0.5
        ),
        vk::Extent2D {
            width: 640,
            height: 360
        }
    );
    assert_eq!(
        scaled_extent(
            vk::Extent2D {
                width: 1,
                height: 0
            },
            0.01
        ),
        vk::Extent2D {
            width: 1,
            height: 1
        }
    );
}

#[test]
fn output_pass_uses_reciprocal_source_extent_for_fxaa() {
    assert_eq!(
        inverse_extent(vk::Extent2D {
            width: 1280,
            height: 720,
        }),
        [1.0 / 1280.0, 1.0 / 720.0]
    );
}

#[test]
fn surface_extent_uses_fixed_extent_or_clamps_requested_size() {
    let capabilities = vk::SurfaceCapabilitiesKHR {
        current_extent: vk::Extent2D {
            width: u32::MAX,
            height: u32::MAX,
        },
        min_image_extent: vk::Extent2D {
            width: 100,
            height: 100,
        },
        max_image_extent: vk::Extent2D {
            width: 800,
            height: 600,
        },
        ..Default::default()
    };
    assert_eq!(
        select_surface_extent(
            capabilities,
            vk::Extent2D {
                width: 50,
                height: 900,
            }
        ),
        vk::Extent2D {
            width: 100,
            height: 600,
        }
    );
    let fixed = vk::SurfaceCapabilitiesKHR {
        current_extent: vk::Extent2D {
            width: 1920,
            height: 1080,
        },
        ..capabilities
    };
    assert_eq!(
        select_surface_extent(
            fixed,
            vk::Extent2D {
                width: 1280,
                height: 720,
            }
        ),
        vk::Extent2D {
            width: 1920,
            height: 1080,
        }
    );
}

#[test]
fn usable_surface_extent_skips_zero_and_resolves_fixed_or_clamped_sizes() {
    let variable = vk::SurfaceCapabilitiesKHR {
        current_extent: vk::Extent2D {
            width: u32::MAX,
            height: u32::MAX,
        },
        min_image_extent: vk::Extent2D {
            width: 100,
            height: 100,
        },
        max_image_extent: vk::Extent2D {
            width: 800,
            height: 600,
        },
        ..Default::default()
    };
    for requested in [
        vk::Extent2D {
            width: 0,
            height: 720,
        },
        vk::Extent2D {
            width: 1280,
            height: 0,
        },
        vk::Extent2D {
            width: 0,
            height: 0,
        },
    ] {
        assert_eq!(usable_surface_extent(variable, requested), None);
    }
    assert_eq!(
        usable_surface_extent(
            variable,
            vk::Extent2D {
                width: 50,
                height: 900,
            }
        ),
        Some(vk::Extent2D {
            width: 100,
            height: 600,
        })
    );

    let fixed = vk::SurfaceCapabilitiesKHR {
        current_extent: vk::Extent2D {
            width: 1920,
            height: 1080,
        },
        ..variable
    };
    assert_eq!(
        usable_surface_extent(
            fixed,
            vk::Extent2D {
                width: 1280,
                height: 720,
            }
        ),
        Some(vk::Extent2D {
            width: 1920,
            height: 1080,
        })
    );

    let fixed_zero = vk::SurfaceCapabilitiesKHR {
        current_extent: vk::Extent2D {
            width: 1920,
            height: 0,
        },
        ..fixed
    };
    assert_eq!(
        usable_surface_extent(
            fixed_zero,
            vk::Extent2D {
                width: 1280,
                height: 720,
            }
        ),
        None
    );
}

#[test]
fn overlay_validation_requires_whole_triangles() {
    let vertex = OverlayVertex::default();
    assert!(validate_overlay(&[]).is_ok());
    assert!(validate_overlay(&[vertex; 3]).is_ok());
    assert!(validate_overlay(&[vertex; 6]).is_ok());
    for count in [1, 2, 4] {
        assert!(matches!(
            validate_overlay(&vec![vertex; count]),
            Err(RenderError::InvalidMesh(_))
        ));
    }
}

#[test]
fn overlay_vertices_are_tightly_packed_floats() {
    let vertices = [OverlayVertex {
        position: [1.0, 2.0],
        color: [0.25, 0.5, 0.75, 1.0],
    }];
    let bytes = overlay_vertex_bytes(&vertices);
    assert_eq!(bytes.len(), 6 * size_of::<f32>());
    let floats = bytes
        .as_chunks::<4>()
        .0
        .iter()
        .map(|chunk| f32::from_ne_bytes(*chunk))
        .collect::<Vec<_>>();
    assert_eq!(floats, [1.0, 2.0, 0.25, 0.5, 0.75, 1.0]);
}

fn ns(value: u64) -> Duration {
    Duration::from_nanos(value)
}

fn passes_sum(view: &GpuViewTimings) -> Duration {
    view.passes.iter().flatten().sum()
}

#[test]
fn view_timings_split_ticks_between_recorded_boundaries() {
    // Начало, затем граница после каждого из семнадцати проходов в порядке выполнения;
    // экранные шейдеры не записаны и отдают пустую границу.
    let marks = [
        Some(100),
        Some(110),
        Some(125),
        Some(140),
        Some(145),
        Some(150),
        Some(160),
        Some(165),
        Some(175),
        Some(180),
        Some(185),
        Some(190),
        None,
        Some(195),
        Some(200),
        Some(205),
        Some(215),
        None,
    ];
    let view = decode_view(ProfiledView::Screen, &marks, 64, 1.0).unwrap();

    assert_eq!(view.key, ProfiledView::Screen);
    assert_eq!(
        view.passes,
        [
            Some(ns(10)),
            Some(ns(15)),
            Some(ns(15)),
            Some(ns(5)),
            Some(ns(5)),
            Some(ns(10)),
            Some(ns(5)),
            Some(ns(10)),
            Some(ns(5)),
            Some(ns(5)),
            Some(ns(5)),
            None,
            Some(ns(5)),
            Some(ns(5)),
            Some(ns(5)),
            Some(ns(10)),
            None,
        ]
    );
    assert_eq!(view.total, ns(115));
    assert_eq!(passes_sum(&view), view.total);
}

#[test]
fn view_timings_skip_unexecuted_passes_and_keep_sum() {
    // ShadowDirectional и Wireframe не выполнялись: их границ в кадре нет.
    let marks = [
        Some(100),
        Some(110),
        None,
        Some(140),
        Some(150),
        Some(160),
        Some(175),
        None,
        Some(200),
        Some(215),
    ];
    let view = decode_view(ProfiledView::Screen, &marks, 64, 1.0).unwrap();

    assert_eq!(view.passes[0], Some(ns(10)));
    assert_eq!(view.passes[1], None);
    assert_eq!(
        view.passes[2],
        Some(ns(30)),
        "ShadowLocal считается от Clusters"
    );
    assert_eq!(view.passes[6], None);
    assert_eq!(
        view.passes[7],
        Some(ns(25)),
        "TemporalResolve считается от Transparent"
    );
    assert_eq!(view.total, ns(115));
    assert_eq!(passes_sum(&view), view.total);
}

#[test]
fn view_timings_are_absent_without_start_boundary() {
    let marks = [
        None,
        Some(110),
        Some(125),
        Some(140),
        Some(150),
        Some(160),
        Some(175),
        Some(190),
        Some(200),
        Some(215),
    ];
    assert_eq!(decode_view(ProfiledView::Screen, &marks, 64, 1.0), None);
}

#[test]
fn view_timings_handle_counter_wrap_within_valid_bits() {
    // Счётчик на 36 бит переполняется между началом и первым проходом.
    let max = (1_u64 << 36) - 1;
    let marks = [
        Some(max - 9),
        Some(5),
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        Some(20),
    ];
    let view = decode_view(ProfiledView::Screen, &marks, 36, 1.0).unwrap();

    assert_eq!(view.passes[0], Some(ns(15)));
    assert_eq!(view.passes[8], Some(ns(15)));
    assert_eq!(view.total, ns(30));
    assert_eq!(passes_sum(&view), view.total);
}

#[test]
fn output_timings_need_both_boundaries() {
    assert_eq!(decode_output(&[Some(10), Some(35)], 64, 2.0), Some(ns(50)));
    assert_eq!(decode_output(&[Some(10), None], 64, 1.0), None);
    assert_eq!(decode_output(&[None, Some(35)], 64, 1.0), None);
}

#[test]
fn unavailable_timestamps_are_absent_even_with_stale_values() {
    // Значение 99 осталось в буфере от прошлой записи и не должно попасть в разбор.
    let results = [[10, 1], [99, 0], [30, 1]];

    assert_eq!(available_timestamps(&results), [Some(10), None, Some(30)]);
}

#[test]
fn all_unavailable_timestamps_are_absent() {
    assert_eq!(
        available_timestamps(&[[10, 0], [20, 0], [0, 0]]),
        [None, None, None]
    );
}

#[test]
fn available_timestamps_keep_every_tick_including_zero() {
    assert_eq!(
        available_timestamps(&[[1, 1], [0, 1], [3, 7]]),
        [Some(1), Some(0), Some(3)]
    );
}

#[test]
fn layout_decodes_each_view_with_the_keys_it_recorded() {
    let mut layout = GpuProfileLayout::default();
    let recorded = [3, 1, 7, 2].map(|id| ProfiledView::Viewport(ViewportId(id)));
    for (index, view) in recorded.into_iter().enumerate() {
        assert_eq!(layout.add_view(view), Some(index as u32 * VIEW_QUERY_COUNT));
    }
    assert_eq!(
        layout.add_view(ProfiledView::Screen),
        None,
        "пятый вид не профилируется"
    );

    let mut queries = [None; TIMESTAMP_QUERY_COUNT as usize];
    for index in 0..MAX_PROFILED_VIEWS {
        let first = index * VIEW_QUERY_COUNT as usize;
        queries[first] = Some(0);
        queries[first + GpuPass::COUNT] = Some(10 * (index as u64 + 1));
    }
    let timings = layout.decode(&queries, 64, 1.0);

    for (index, view) in recorded.into_iter().enumerate() {
        let timing = timings.views[index].expect("вид записан в кадр");
        assert_eq!(timing.key, view);
        assert_eq!(timing.total, ns(10 * (index as u64 + 1)));
    }
    assert_eq!(timings.output_ui, None);
    assert_eq!(timings.first_view_total(), Some(ns(10)));
}

#[test]
fn layout_keeps_keys_of_the_frame_after_viewport_set_changes() {
    // Набор viewport сменился: в кадре записан один вид, ключ берётся из записи этого кадра.
    let mut layout = GpuProfileLayout::default();
    assert_eq!(
        layout.add_view(ProfiledView::Viewport(ViewportId(5))),
        Some(0)
    );
    let mut queries = [None; TIMESTAMP_QUERY_COUNT as usize];
    queries[0] = Some(0);
    queries[GpuPass::COUNT] = Some(40);

    let timings = layout.decode(&queries, 64, 1.0);

    assert_eq!(
        timings.views[0].map(|view| view.key),
        Some(ProfiledView::Viewport(ViewportId(5)))
    );
    assert_eq!(timings.views[1], None);
}

#[test]
fn query_ranges_of_views_and_output_cover_pool_without_overlap() {
    assert_eq!(TIMESTAMP_QUERY_COUNT, 74);
    let mut owner = [None::<usize>; TIMESTAMP_QUERY_COUNT as usize];
    let mut layout = GpuProfileLayout::default();
    for index in 0..MAX_PROFILED_VIEWS {
        let first = layout
            .add_view(ProfiledView::Viewport(ViewportId(index)))
            .unwrap();
        for query in first..first + VIEW_QUERY_COUNT {
            assert!(owner[query as usize].replace(index).is_none());
        }
    }
    for query in OUTPUT_QUERY_FIRST..TIMESTAMP_QUERY_COUNT {
        assert!(owner[query as usize].replace(MAX_PROFILED_VIEWS).is_none());
    }
    assert!(owner.iter().all(Option::is_some));
}

#[test]
fn timestamp_duration_scales_ticks_by_period() {
    assert_eq!(
        timestamp_duration(100, 1_100, 64, 1.0),
        Duration::from_nanos(1_000)
    );
    assert_eq!(
        timestamp_duration(0, 1_000, 64, 2.5),
        Duration::from_nanos(2_500)
    );
    assert_eq!(timestamp_duration(5, 5, 64, 1.0), Duration::ZERO);
}

#[test]
fn timestamp_duration_handles_counter_wrap_within_valid_bits() {
    let max = (1_u64 << 36) - 1;
    assert_eq!(
        timestamp_duration(max - 9, 10, 36, 1.0),
        Duration::from_nanos(20)
    );
    // Биты выше timestampValidBits не определены и отбрасываются.
    assert_eq!(
        timestamp_duration(1 << 40, (1 << 40) + 7 + (1 << 37), 36, 1.0),
        Duration::from_nanos(7)
    );
    assert_eq!(
        timestamp_duration(u64::MAX, 4, 64, 1.0),
        Duration::from_nanos(5)
    );
}
