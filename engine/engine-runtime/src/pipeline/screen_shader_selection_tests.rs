use super::*;
use engine_files::VirtualPath;
use engine_scene::ScreenShaderStage as SceneStage;

fn shader(
    path: &str,
    stage: SceneStage,
    enabled: bool,
    params: &[f32],
) -> engine_scene::ScreenShaderRef {
    engine_scene::ScreenShaderRef {
        shader: VirtualPath::parse(path).unwrap(),
        stage,
        enabled,
        params: params.to_vec(),
    }
}

#[test]
fn enabled_passes_keep_file_order_stage_and_padded_parameters() {
    let shaders = [
        shader("fx/vhs.glsl", SceneStage::AfterPost, true, &[0.7, 0.3]),
        shader("fx/off.glsl", SceneStage::BeforePost, false, &[1.0]),
        shader("fx/failed.glsl", SceneStage::BeforePost, true, &[]),
        shader("fx/color.glsl", SceneStage::BeforePost, true, &[]),
    ];
    let ids = HashMap::from([
        ("fx/vhs.glsl".to_owned(), 1_u32),
        ("fx/off.glsl".to_owned(), 3),
        ("fx/color.glsl".to_owned(), 2),
    ]);
    let [first, second, third, fourth] = enabled_screen_passes(&shaders, &ids);
    let (id, stage, params) = first.expect("первый включённый проход");
    assert_eq!(
        (id, stage),
        (1, engine_render::ScreenShaderStage::AfterPost)
    );
    assert_eq!(params[..2], [0.7, 0.3]);
    assert!(params[2..].iter().all(|value| *value == 0.0));
    assert_eq!(
        second.map(|(id, stage, _)| (id, stage)),
        Some((2, engine_render::ScreenShaderStage::BeforePost)),
        "выключенный и не созданный шейдеры пропущены"
    );
    assert!(third.is_none() && fourth.is_none());
}

#[test]
fn selection_keeps_at_most_four_passes() {
    let shaders: Vec<_> = (0..5)
        .map(|index| {
            shader(
                &format!("fx/{index}.glsl"),
                SceneStage::AfterPost,
                true,
                &[],
            )
        })
        .collect();
    let ids: HashMap<String, u32> = (0..5)
        .map(|index| (format!("fx/{index}.glsl"), index))
        .collect();
    let passes = enabled_screen_passes(&shaders, &ids);
    assert_eq!(
        passes.iter().flatten().count(),
        engine_render::SCREEN_SHADER_MAX_PASSES
    );
}
