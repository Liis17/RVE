use super::*;

#[test]
fn scene_defaults_map_to_renderer_defaults() {
    assert_eq!(
        render_post_process(&PostProcessSettings::default(), None),
        PostProcess::default()
    );
}

#[test]
fn disabled_exposure_and_tonemapping_become_identity_values() {
    let mut post = PostProcessSettings::default();
    post.exposure.enabled = false;
    post.exposure.ev = 2.0;
    post.tonemapping.enabled = false;
    let mapped = render_post_process(&post, None);
    assert_eq!(mapped.exposure, Exposure::default());
    assert_eq!(mapped.tonemapper, RenderTonemapper::None);
}

#[test]
fn enabled_effects_keep_their_parameters_and_quality() {
    let mut post = PostProcessSettings::default();
    post.bloom.enabled = true;
    post.bloom.intensity = 1.5;
    post.ambient_occlusion.enabled = true;
    post.ambient_occlusion.quality = engine_scene::AmbientOcclusionQuality::High;
    post.exposure.mode = engine_scene::ExposureMode::Auto;
    post.exposure.ev = -0.5;
    let mapped = render_post_process(&post, None);
    assert!(mapped.bloom.enabled);
    assert_eq!(mapped.bloom.intensity, 1.5);
    assert!(mapped.ambient_occlusion.enabled);
    assert_eq!(
        mapped.ambient_occlusion.quality,
        engine_render::AmbientOcclusionQuality::High
    );
    assert_eq!(mapped.exposure.mode, engine_render::ExposureMode::Auto);
    assert_eq!(mapped.exposure.ev, -0.5);
}

fn fake_lut(value: usize) -> ColorLutId {
    // SAFETY: ColorLutId — обёртка над одним usize; любое значение допустимо для теста.
    unsafe { std::mem::transmute(value) }
}

#[test]
fn color_lut_maps_with_intensity_and_is_found_by_canonical_path() {
    let mut post = PostProcessSettings::default();
    post.color_grading.enabled = true;
    post.color_grading.lut_intensity = 0.25;
    let mapped = render_post_process(&post, Some(fake_lut(7)));
    assert_eq!(mapped.color_grading.lut, Some(fake_lut(7)));
    assert_eq!(mapped.color_grading.lut_intensity, 0.25);

    let luts = HashMap::from([("luts/film.cube".to_owned(), fake_lut(7))]);
    assert_eq!(scene_color_lut(&post, &luts), None, "без LUT в настройках");
    post.color_grading.lut = Some("/luts/film.cube".to_owned());
    assert_eq!(scene_color_lut(&post, &luts), Some(fake_lut(7)));
    post.color_grading.lut = Some("luts/film.cube".to_owned());
    assert_eq!(scene_color_lut(&post, &luts), Some(fake_lut(7)));
    post.color_grading.lut = Some("/luts/other.cube".to_owned());
    assert_eq!(scene_color_lut(&post, &luts), None, "LUT не загрузилась");
}

#[test]
fn screen_shader_time_keeps_fraction_of_long_sessions() {
    // 3_600_000 имеет ulp 0.25 в f32: сначала переведённое в f32 время потеряло бы 0.1 с.
    let time = screen_shader_time(3_600_000.1);
    assert!((time - 0.1).abs() < 1e-4, "получено {time}");
    assert_eq!(screen_shader_time(3_600.0 + 2.5), 2.5);
}
