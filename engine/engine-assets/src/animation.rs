//! Клипы FBX: локальные TRS-ключи узлов, полученные через `ufbx::bake_anim`.
//!
//! Ключи хранят время в секундах от начала клипа и имена узлов. Сопоставление
//! с костями скелета выполняется в `engine-animation`.

use crate::skin::SkeletonBuilder;

/// Предел числа ключей на все клипы модели, чтобы не допустить неограниченного роста памяти.
pub(crate) const KEY_LIMIT: usize = 4 * 1024 * 1024;

/// Ключ дорожки: время в секундах и значение.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ModelKey<T> {
    pub time: f32,
    pub value: T,
}

/// Дорожка одного узла; каналы без ключей берут позу узла из скелета.
#[derive(Clone, Debug, PartialEq)]
pub struct ModelTrack {
    /// Имя узла в FBX; сопоставляется с [`crate::ModelNode::name`] по имени.
    pub node: String,
    pub translation: Vec<ModelKey<[f32; 3]>>,
    /// Кватернионы `[x, y, z, w]`.
    pub rotation: Vec<ModelKey<[f32; 4]>>,
    pub scale: Vec<ModelKey<[f32; 3]>>,
}

/// Именованный клип с длительностью в секундах.
#[derive(Clone, Debug, PartialEq)]
pub struct ModelAnimation {
    pub name: String,
    pub duration: f32,
    pub tracks: Vec<ModelTrack>,
}

/// Есть ли в файле стек с хотя бы одним ключом. Пустой стек, например `Take 001` статичной
/// модели, анимацией не считается.
pub(crate) fn has_animated_stack(scene: &ufbx::Scene) -> Result<bool, String> {
    let mut keys = usize::MAX;
    for stack in scene.anim_stacks.iter() {
        let baked = bake_stack(scene, stack, String::new(), &|_| true, &mut keys)?;
        if !baked.tracks.is_empty() {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Запекает стеки модели с ключами; в клип попадают только узлы скелета.
pub(crate) fn bake_model_animations(
    builder: &SkeletonBuilder,
    scene: &ufbx::Scene,
) -> Result<Vec<ModelAnimation>, String> {
    let mut keys = KEY_LIMIT;
    let mut animations = Vec::new();
    for stack in scene.anim_stacks.iter() {
        let baked = bake_stack(
            scene,
            stack,
            stack.element.name.to_string(),
            &|typed_id| builder.contains(typed_id),
            &mut keys,
        )?;
        if !baked.tracks.is_empty() {
            animations.push(baked);
        }
    }
    Ok(animations)
}

/// Запекает стеки файла с ключами как клипы; имена — stem файла, если стек один.
pub(crate) fn bake_clip_file(
    scene: &ufbx::Scene,
    stem: &str,
) -> Result<Vec<ModelAnimation>, String> {
    let mut keys = KEY_LIMIT;
    let single = scene.anim_stacks.len() == 1;
    let mut clips = Vec::new();
    for stack in scene.anim_stacks.iter() {
        let name = if single {
            stem.to_owned()
        } else {
            format!("{stem}:{}", stack.element.name)
        };
        let baked = bake_stack(scene, stack, name, &|_| true, &mut keys)?;
        if !baked.tracks.is_empty() {
            clips.push(baked);
        }
    }
    Ok(clips)
}

fn bake_stack(
    scene: &ufbx::Scene,
    stack: &ufbx::AnimStack,
    name: String,
    include: &dyn Fn(u32) -> bool,
    keys: &mut usize,
) -> Result<ModelAnimation, String> {
    let baked = ufbx::bake_anim(scene, &stack.anim, ufbx::BakeOpts::default())
        .map_err(|error| format!("ошибка запекания анимации: {error:?}"))?;
    let begin = baked.playback_time_begin;
    let mut tracks = Vec::new();
    for node in baked.nodes.iter() {
        if !include(node.typed_id) {
            continue;
        }
        let count = node.translation_keys.len() + node.rotation_keys.len() + node.scale_keys.len();
        if count == 0 {
            continue;
        }
        *keys = keys
            .checked_sub(count)
            .ok_or_else(|| format!("анимации превышают предел памяти {KEY_LIMIT} ключей"))?;
        tracks.push(ModelTrack {
            node: scene.nodes[node.typed_id as usize].element.name.to_string(),
            translation: node
                .translation_keys
                .iter()
                .map(|key| ModelKey {
                    time: key_time(key.time, begin),
                    value: vec3(key.value),
                })
                .collect(),
            rotation: node
                .rotation_keys
                .iter()
                .map(|key| ModelKey {
                    time: key_time(key.time, begin),
                    value: [
                        key.value.x as f32,
                        key.value.y as f32,
                        key.value.z as f32,
                        key.value.w as f32,
                    ],
                })
                .collect(),
            scale: node
                .scale_keys
                .iter()
                .map(|key| ModelKey {
                    time: key_time(key.time, begin),
                    value: vec3(key.value),
                })
                .collect(),
        });
    }
    Ok(ModelAnimation {
        name,
        duration: baked.playback_duration as f32,
        tracks,
    })
}

fn key_time(time: f64, begin: f64) -> f32 {
    (time - begin).max(0.0) as f32
}

fn vec3(value: ufbx::Vec3) -> [f32; 3] {
    [value.x as f32, value.y as f32, value.z as f32]
}
