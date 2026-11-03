//! Подготовка кадра сцены: GPU-кэш ресурсов и команды отрисовки.

use crate::console::ConsoleLog;
use crate::project::OpenProject;
use crate::viewport::model_loader::ModelLoader;
use engine_assets::{Model, ModelAlphaMode};
use engine_render::AlphaMode;
use engine_render::{
    AntiAliasing as RenderAntiAliasing, DirectionalLight, DrawCommand, Frame, Material, MaterialId,
    MeshId, RenderError, Renderer, SHADOW_MAP_RESOLUTION, TextureColorSpace, TextureId,
    Tonemapper as RenderTonemapper, Vertex, ViewSettings, ViewportId,
};
use engine_scene::camera::Camera;
use engine_scene::document::{LodGroup, MaterialOverride, MeshRenderer};
use engine_scene::light::cascade_view_projections;
use engine_scene::world::ObjectHandle;
use engine_scene::{AntiAliasing, CameraSettings, LightState, SceneFrame, Tonemapper, World};
use glam::{Mat4, Vec3};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

const GRID_HALF_CELLS: i32 = 20;
const GRID_LINE_WIDTH: f32 = 0.02;

struct GpuModel {
    meshes: Vec<(MeshId, Material, MaterialId)>,
    textures: Vec<TextureId>,
}

/// GPU-ресурсы вьюпорта. Модели и текстуры кэшируются по virtual path; ошибка загрузки
/// запоминается и пишется в консоль один раз, пока кэш не будет сброшен.
pub struct SceneRenderer {
    pub viewport: ViewportId,
    pub game_viewport: ViewportId,
    models: HashMap<String, Option<GpuModel>>,
    textures: HashMap<(String, TextureColorSpace), Option<TextureId>>,
    object_materials: HashMap<(ObjectHandle, String, usize, usize), (Material, MaterialId)>,
    grid: Option<MeshId>,
    scene_draws: Vec<DrawCommand>,
    game_draws: Vec<DrawCommand>,
}

impl SceneRenderer {
    pub fn new(renderer: &mut Renderer) -> Result<Self, RenderError> {
        Ok(Self {
            viewport: renderer.create_viewport(1, 1)?,
            game_viewport: renderer.create_viewport(1, 1)?,
            models: HashMap::new(),
            textures: HashMap::new(),
            object_materials: HashMap::new(),
            grid: None,
            scene_draws: Vec::new(),
            game_draws: Vec::new(),
        })
    }

    /// Удаляет кэшированные ресурсы: файлы на диске могли измениться.
    pub fn clear_cache(
        &mut self,
        renderer: &mut Renderer,
        cpu_models: &mut HashMap<String, Arc<Model>>,
    ) -> Result<(), RenderError> {
        for model in self.models.drain().filter_map(|(_, model)| model) {
            destroy_model(renderer, model)?;
        }
        for (_, (_, material)) in self.object_materials.drain() {
            renderer.destroy_material(material)?;
        }
        for texture in self.textures.drain().filter_map(|(_, texture)| texture) {
            renderer.destroy_texture(texture)?;
        }
        cpu_models.clear();
        Ok(())
    }

    /// Загружает ресурсы объектов мира, которых ещё нет в кэше. Модели разбираются в фоне:
    /// до готовности объект не рисуется, а готовые модели загружаются в GPU в этом вызове.
    pub fn sync_assets(
        &mut self,
        renderer: &mut Renderer,
        project: &OpenProject,
        world: &World,
        cpu_models: &mut HashMap<String, Arc<Model>>,
        loader: &mut ModelLoader,
        log: &mut ConsoleLog,
    ) -> Result<(), RenderError> {
        for (path, loaded) in loader.finished() {
            let gpu = match loaded {
                Ok(model) => {
                    let model = Arc::new(model);
                    let gpu = upload_model(renderer, &model)?;
                    cpu_models.insert(path.clone(), model);
                    Some(gpu)
                }
                Err(error) => {
                    log.error(format!("{path}: {error}"));
                    None
                }
            };
            if let Some(Some(previous)) = self.models.insert(path, gpu) {
                destroy_model(renderer, previous)?;
            }
        }
        for object in world.objects() {
            let Some(mesh) = &object.mesh_renderer else {
                continue;
            };
            let levels = object
                .lod_group
                .as_ref()
                .map_or(1, |group| group.levels.len() + 1);
            for level in 0..levels {
                let Some(model_path) = lod_model_path(mesh, object.lod_group.as_ref(), level)
                else {
                    continue;
                };
                if !self.models.contains_key(model_path) {
                    match cpu_models.get(model_path).cloned() {
                        Some(model) => {
                            let gpu = upload_model(renderer, &model)?;
                            self.models.insert(model_path.to_owned(), Some(gpu));
                        }
                        None => {
                            if !loader.request(model_path) {
                                log.error(format!("{model_path}: фоновая загрузка недоступна"));
                                self.models.insert(model_path.to_owned(), None);
                            }
                        }
                    }
                }
            }
            for level in 0..levels {
                if let Some(path) = lod_texture_path(mesh, object.lod_group.as_ref(), level) {
                    self.sync_texture(renderer, project, log, path, TextureColorSpace::Srgb)?;
                }
            }
            if let Some(override_material) = &mesh.material {
                for (path, color_space) in [
                    override_material
                        .metallic_roughness_texture
                        .as_ref()
                        .map(|path| (path.as_str(), TextureColorSpace::Linear)),
                    override_material
                        .normal_texture
                        .as_ref()
                        .map(|path| (path.as_str(), TextureColorSpace::Linear)),
                    override_material
                        .emissive_texture
                        .as_ref()
                        .map(|path| (path.as_str(), TextureColorSpace::Srgb)),
                    override_material
                        .occlusion_texture
                        .as_ref()
                        .map(|path| (path.as_str(), TextureColorSpace::Linear)),
                ]
                .into_iter()
                .flatten()
                {
                    self.sync_texture(renderer, project, log, path, color_space)?;
                }
            }
        }
        Ok(())
    }

    fn sync_texture(
        &mut self,
        renderer: &mut Renderer,
        project: &OpenProject,
        log: &mut ConsoleLog,
        path: &str,
        color_space: TextureColorSpace,
    ) -> Result<(), RenderError> {
        let key = (path.to_owned(), color_space);
        if self.textures.contains_key(&key) {
            return Ok(());
        }
        let gpu = match project.assets.load_texture(path) {
            Ok(data) => Some(renderer.create_texture_with_color_space(
                data.width,
                data.height,
                &data.rgba,
                color_space,
            )?),
            Err(error) => {
                log.error(format!("{path}: {error}"));
                None
            }
        };
        self.textures.insert(key, gpu);
        Ok(())
    }

    /// Строит отдельные draw lists для Scene и Game; сетка добавляется только в Scene.
    pub fn build_draws(
        &mut self,
        renderer: &mut Renderer,
        world: &World,
        frame: &SceneFrame,
        scene_eye: Vec3,
        game_eye: Vec3,
        grid_camera_distance: Option<f32>,
    ) -> Result<(), RenderError> {
        self.scene_draws.clear();
        self.game_draws.clear();
        let used_object_materials = {
            let mut resources = DrawResources {
                models: &self.models,
                textures: &self.textures,
                object_materials: &mut self.object_materials,
                used_object_materials: HashSet::new(),
            };
            append_world_draws(
                renderer,
                world,
                frame,
                scene_eye,
                &mut resources,
                &mut self.scene_draws,
            )?;
            append_world_draws(
                renderer,
                world,
                frame,
                game_eye,
                &mut resources,
                &mut self.game_draws,
            )?;
            resources.used_object_materials
        };
        let stale = self
            .object_materials
            .keys()
            .filter(|key| !used_object_materials.contains(*key))
            .cloned()
            .collect::<Vec<_>>();
        for key in stale {
            if let Some((_, material)) = self.object_materials.remove(&key) {
                renderer.destroy_material(material)?;
            }
        }
        if let Some(distance) = grid_camera_distance {
            let grid = match self.grid {
                Some(grid) => grid,
                None => {
                    let (vertices, indices) = grid_mesh();
                    let grid = renderer.create_mesh(&vertices, &indices)?;
                    self.grid = Some(grid);
                    grid
                }
            };
            self.scene_draws.push(DrawCommand {
                mesh: grid,
                material: None,
                model: Mat4::from_scale(Vec3::splat(grid_scale(distance))).to_cols_array_2d(),
                casts_shadow: false,
            });
        }
        Ok(())
    }

    /// Собирает `Frame` для заданной матрицы камеры; `with_grid` выбирает Scene draw list.
    #[allow(clippy::too_many_arguments)]
    pub fn frame<'a>(
        &'a self,
        view_projection: Mat4,
        camera_position: Vec3,
        settings: CameraSettings,
        light: &LightState,
        scene_has_light: bool,
        shadow_cascades: &'a [[[f32; 4]; 4]],
        with_grid: bool,
    ) -> Frame<'a> {
        let light = frame_light(light, scene_has_light);
        Frame {
            clear_color: [
                settings.render.clear_color[0],
                settings.render.clear_color[1],
                settings.render.clear_color[2],
                1.0,
            ],
            view_projection: view_projection.to_cols_array_2d(),
            shadow_cascades,
            camera_position: camera_position.to_array(),
            light,
            draws: if with_grid {
                &self.scene_draws
            } else {
                &self.game_draws
            },
            overlay: &[],
            view: ViewSettings {
                render_scale: settings.render.render_scale,
                exposure_ev: settings.color.exposure_ev,
                tonemapper: match settings.color.tonemapper {
                    Tonemapper::Aces => RenderTonemapper::Aces,
                    Tonemapper::Neutral => RenderTonemapper::Neutral,
                    Tonemapper::None => RenderTonemapper::None,
                },
                anti_aliasing: match settings.render.anti_aliasing {
                    AntiAliasing::Fxaa => RenderAntiAliasing::Fxaa,
                    AntiAliasing::None => RenderAntiAliasing::None,
                },
            },
        }
    }

    pub fn shadow_cascades(
        camera: Camera,
        width: u32,
        height: u32,
        light: &LightState,
        scene_has_light: bool,
    ) -> Vec<[[f32; 4]; 4]> {
        if !scene_has_light {
            return Vec::new();
        }
        let Some(settings) = light.shadow else {
            return Vec::new();
        };
        cascade_view_projections(
            camera,
            width.max(1) as f32 / height.max(1) as f32,
            Vec3::from_array(light.direction),
            settings,
            SHADOW_MAP_RESOLUTION,
        )
        .into_iter()
        .map(|matrix| matrix.to_cols_array_2d())
        .collect()
    }
}

fn lod_model_path<'a>(
    mesh: &'a MeshRenderer,
    group: Option<&'a LodGroup>,
    level: usize,
) -> Option<&'a str> {
    if level == 0 {
        return Some(mesh.model.as_str());
    }
    let model = group?.levels.get(level - 1)?.model.as_str();
    Some(model.strip_prefix('/').unwrap_or(model))
}

fn lod_texture_path<'a>(
    mesh: &'a MeshRenderer,
    group: Option<&'a LodGroup>,
    level: usize,
) -> Option<&'a str> {
    let texture = if level == 0 {
        mesh.texture.as_ref().map(|path| path.as_str())
    } else {
        group?
            .levels
            .get(level - 1)?
            .texture
            .as_deref()
            .or_else(|| mesh.texture.as_ref().map(|path| path.as_str()))
    }?;
    Some(texture.strip_prefix('/').unwrap_or(texture))
}

struct DrawResources<'a> {
    models: &'a HashMap<String, Option<GpuModel>>,
    textures: &'a HashMap<(String, TextureColorSpace), Option<TextureId>>,
    object_materials: &'a mut HashMap<(ObjectHandle, String, usize, usize), (Material, MaterialId)>,
    used_object_materials: HashSet<(ObjectHandle, String, usize, usize)>,
}

// Both viewports can reference the same model with different LOD textures in one frame.
fn object_material_key(
    object: ObjectHandle,
    model_path: &str,
    level: usize,
    mesh_index: usize,
) -> (ObjectHandle, String, usize, usize) {
    (object, model_path.to_owned(), level, mesh_index)
}

fn append_world_draws(
    renderer: &mut Renderer,
    world: &World,
    frame: &SceneFrame,
    eye: Vec3,
    resources: &mut DrawResources<'_>,
    draws: &mut Vec<DrawCommand>,
) -> Result<(), RenderError> {
    for instance in &frame.meshes {
        let Some(object) = world.object(instance.object) else {
            continue;
        };
        let Some(mesh) = &object.mesh_renderer else {
            continue;
        };
        let Some(level) = world.lod_level(instance.object, eye) else {
            continue;
        };
        let Some(model_path) = lod_model_path(mesh, object.lod_group.as_ref(), level) else {
            continue;
        };
        let Some(Some(model)) = resources.models.get(model_path) else {
            continue;
        };
        let replacement =
            lod_texture_path(mesh, object.lod_group.as_ref(), level).and_then(|path| {
                resources
                    .textures
                    .get(&(path.to_owned(), TextureColorSpace::Srgb))
                    .copied()
                    .flatten()
            });
        for (mesh_index, (mesh_id, base_material, base_material_id)) in
            model.meshes.iter().enumerate()
        {
            let override_material = mesh.material.as_ref();
            let mr_texture = override_material
                .and_then(|material| material.metallic_roughness_texture.as_ref())
                .and_then(|path| {
                    resources
                        .textures
                        .get(&(path.as_str().to_owned(), TextureColorSpace::Linear))
                        .copied()
                        .flatten()
                });
            let normal_texture = override_material
                .and_then(|material| material.normal_texture.as_ref())
                .and_then(|path| {
                    resources
                        .textures
                        .get(&(path.as_str().to_owned(), TextureColorSpace::Linear))
                        .copied()
                        .flatten()
                });
            let emissive_texture = override_material
                .and_then(|material| material.emissive_texture.as_ref())
                .and_then(|path| {
                    resources
                        .textures
                        .get(&(path.as_str().to_owned(), TextureColorSpace::Srgb))
                        .copied()
                        .flatten()
                });
            let occlusion_texture = override_material
                .and_then(|material| material.occlusion_texture.as_ref())
                .and_then(|path| {
                    resources
                        .textures
                        .get(&(path.as_str().to_owned(), TextureColorSpace::Linear))
                        .copied()
                        .flatten()
                });
            let resolved = resolve_material(
                *base_material,
                replacement,
                override_material,
                mr_texture,
                normal_texture,
                emissive_texture,
                occlusion_texture,
            );
            let material_id = if resolved == *base_material {
                *base_material_id
            } else {
                let key = object_material_key(instance.object, model_path, level, mesh_index);
                resources.used_object_materials.insert(key.clone());
                match resources.object_materials.get(&key) {
                    Some((cached, id)) if *cached == resolved => *id,
                    previous => {
                        let id = renderer.create_material(&resolved)?;
                        if let Some((_, old_id)) = previous {
                            renderer.destroy_material(*old_id)?;
                        }
                        resources.object_materials.insert(key, (resolved, id));
                        id
                    }
                }
            };
            draws.push(DrawCommand {
                mesh: *mesh_id,
                material: Some(material_id),
                model: instance.model.to_cols_array_2d(),
                casts_shadow: true,
            });
        }
    }
    Ok(())
}

fn destroy_model(renderer: &mut Renderer, model: GpuModel) -> Result<(), RenderError> {
    for (mesh, _, material) in model.meshes {
        renderer.destroy_mesh(mesh)?;
        renderer.destroy_material(material)?;
    }
    for texture in model.textures {
        renderer.destroy_texture(texture)?;
    }
    Ok(())
}

fn upload_model(renderer: &mut Renderer, model: &Model) -> Result<GpuModel, RenderError> {
    let mut textures_by_role = HashMap::new();
    let mut textures = Vec::new();
    let mut meshes = Vec::with_capacity(model.meshes.len());
    let mut materials = Vec::with_capacity(model.meshes.len());
    let upload = (|| {
        for model_mesh in &model.meshes {
            let base = &model_mesh.material;
            let material = Material {
                base_color: base.base_color,
                metallic: base.metallic,
                roughness: base.roughness,
                emissive: base.emissive,
                opacity: base.opacity,
                alpha_mode: match base.alpha_mode {
                    ModelAlphaMode::Opaque => AlphaMode::Opaque,
                    ModelAlphaMode::Blend => AlphaMode::Blend,
                },
                base_color_texture: model_texture(
                    renderer,
                    model,
                    base.base_color_texture,
                    TextureColorSpace::Srgb,
                    &mut textures_by_role,
                    &mut textures,
                )?,
                metallic_roughness_texture: model_texture(
                    renderer,
                    model,
                    base.metallic_roughness_texture,
                    TextureColorSpace::Linear,
                    &mut textures_by_role,
                    &mut textures,
                )?,
                normal_texture: model_texture(
                    renderer,
                    model,
                    base.normal_texture,
                    TextureColorSpace::Linear,
                    &mut textures_by_role,
                    &mut textures,
                )?,
                emissive_texture: model_texture(
                    renderer,
                    model,
                    base.emissive_texture,
                    TextureColorSpace::Srgb,
                    &mut textures_by_role,
                    &mut textures,
                )?,
                occlusion_texture: model_texture(
                    renderer,
                    model,
                    base.occlusion_texture,
                    TextureColorSpace::Linear,
                    &mut textures_by_role,
                    &mut textures,
                )?,
                normal_scale: base.normal_scale,
                occlusion_strength: base.occlusion_strength,
            };
            let material_id = renderer.create_material(&material)?;
            materials.push(material_id);
            let vertices = model_mesh
                .vertices
                .iter()
                .map(|vertex| Vertex {
                    position: vertex.position,
                    normal: vertex.normal,
                    uv: vertex.uv,
                    color: [1.0; 3],
                })
                .collect::<Vec<_>>();
            let mesh_id = renderer.create_mesh(&vertices, &model_mesh.indices)?;
            meshes.push((mesh_id, material, material_id));
        }
        Ok::<(), RenderError>(())
    })();
    if let Err(error) = upload {
        for (mesh, _, _) in meshes {
            let _ = renderer.destroy_mesh(mesh);
        }
        for material in materials {
            let _ = renderer.destroy_material(material);
        }
        for texture in textures {
            let _ = renderer.destroy_texture(texture);
        }
        return Err(error);
    }
    Ok(GpuModel { meshes, textures })
}

fn model_texture(
    renderer: &mut Renderer,
    model: &Model,
    index: Option<usize>,
    color_space: TextureColorSpace,
    cache: &mut HashMap<(usize, TextureColorSpace), TextureId>,
    owned: &mut Vec<TextureId>,
) -> Result<Option<TextureId>, RenderError> {
    let Some(index) = index else {
        return Ok(None);
    };
    if let Some(texture) = cache.get(&(index, color_space)) {
        return Ok(Some(*texture));
    }
    let data = model.textures.get(index).ok_or_else(|| {
        RenderError::InvalidMaterial(format!("Model texture index {index} is out of range"))
    })?;
    let texture = renderer.create_texture_with_color_space(
        data.width,
        data.height,
        &data.rgba,
        color_space,
    )?;
    cache.insert((index, color_space), texture);
    owned.push(texture);
    Ok(Some(texture))
}

fn resolve_material(
    mut base: Material,
    replacement_texture: Option<TextureId>,
    override_material: Option<&MaterialOverride>,
    metallic_roughness_texture: Option<TextureId>,
    normal_texture: Option<TextureId>,
    emissive_texture: Option<TextureId>,
    occlusion_texture: Option<TextureId>,
) -> Material {
    if let Some(texture) = replacement_texture {
        base.base_color_texture = Some(texture);
    }
    if let Some(override_material) = override_material {
        if let Some(color) = override_material.base_color {
            base.base_color = color;
        }
        if let Some(metallic) = override_material.metallic {
            base.metallic = metallic;
        }
        if let Some(roughness) = override_material.roughness {
            base.roughness = roughness;
        }
        if let Some(emissive) = override_material.emissive {
            base.emissive = emissive;
        }
        if let Some(opacity) = override_material.opacity {
            base.opacity = opacity;
        }
        if let Some(alpha_mode) = override_material.alpha_mode {
            base.alpha_mode = match alpha_mode {
                engine_scene::MaterialAlphaMode::Opaque => AlphaMode::Opaque,
                engine_scene::MaterialAlphaMode::Mask => AlphaMode::Mask {
                    cutoff: override_material.alpha_cutoff.unwrap_or(0.5),
                },
                engine_scene::MaterialAlphaMode::Blend => AlphaMode::Blend,
            };
        }
        if let Some(texture) = metallic_roughness_texture {
            base.metallic_roughness_texture = Some(texture);
        }
        if let Some(texture) = normal_texture {
            base.normal_texture = Some(texture);
        }
        if let Some(texture) = emissive_texture {
            base.emissive_texture = Some(texture);
        }
        if let Some(texture) = occlusion_texture {
            base.occlusion_texture = Some(texture);
        }
        if let Some(scale) = override_material.normal_scale {
            base.normal_scale = scale;
        }
        if let Some(strength) = override_material.occlusion_strength {
            base.occlusion_strength = strength;
        }
    }
    base
}

/// Свет кадра. Сцена без directional light показывается светом renderer по умолчанию,
/// иначе объекты редактора выглядели бы плоско залитыми.
fn frame_light(light: &LightState, scene_has_light: bool) -> DirectionalLight {
    if !scene_has_light {
        return DirectionalLight::default();
    }
    DirectionalLight {
        direction: light.direction,
        color: light.color,
        ambient: light.ambient,
    }
}

/// Шаг сетки — степень десяти по расстоянию камеры: линии сохраняют толщину на экране.
fn grid_scale(camera_distance: f32) -> f32 {
    10_f32.powf(camera_distance.max(1e-3).log10().floor())
}

/// Сетка пола из тонких квадратов: renderer рисует только треугольники.
fn grid_mesh() -> (Vec<Vertex>, Vec<u32>) {
    let mut vertices = Vec::new();
    let mut indices = Vec::new();
    let extent = GRID_HALF_CELLS as f32;
    let half = GRID_LINE_WIDTH * 0.5;
    let mut quad = |corners: [[f32; 2]; 4], color: [f32; 3]| {
        let first = vertices.len() as u32;
        vertices.extend(corners.map(|[x, z]| Vertex {
            position: [x, 0.0, z],
            normal: [0.0, 1.0, 0.0],
            uv: [0.0, 0.0],
            color,
        }));
        indices.extend([first, first + 1, first + 2, first, first + 2, first + 3]);
    };
    for line in -GRID_HALF_CELLS..=GRID_HALF_CELLS {
        let offset = line as f32;
        let (x_color, z_color) = match line {
            0 => ([0.75, 0.2, 0.2], [0.2, 0.35, 0.8]),
            _ if line % 5 == 0 => ([0.32; 3], [0.32; 3]),
            _ => ([0.2; 3], [0.2; 3]),
        };
        // Линия вдоль X в точке Z = offset и линия вдоль Z в точке X = offset.
        quad(
            [
                [-extent, offset - half],
                [extent, offset - half],
                [extent, offset + half],
                [-extent, offset + half],
            ],
            x_color,
        );
        quad(
            [
                [offset - half, -extent],
                [offset + half, -extent],
                [offset + half, extent],
                [offset - half, extent],
            ],
            z_color,
        );
    }
    (vertices, indices)
}

#[cfg(test)]
mod tests {
    use super::*;
    use engine_files::VirtualPath;
    use engine_scene::LodLevel;

    fn camera() -> Camera {
        Camera {
            position: Vec3::new(0.0, 0.0, 5.0),
            target: Vec3::ZERO,
            up: Vec3::Y,
            fov_narrow: std::f32::consts::FRAC_PI_4,
            near: 0.1,
            far: 100.0,
        }
    }

    #[test]
    fn lod_materials_for_two_viewports_do_not_replace_each_other() {
        let document = engine_scene::SceneDocument::parse(
            "test",
            r#"
[[objects]]
id = "camera"
[objects.camera]
"#,
        )
        .unwrap();
        let world = World::from_document(&document).unwrap();
        let object = world.handle("camera").unwrap();
        let scene_key = object_material_key(object, "shared.fbx", 1, 0);
        let game_key = object_material_key(object, "shared.fbx", 2, 0);
        let mut cache = HashMap::new();
        cache.insert(scene_key.clone(), "scene texture material");
        cache.insert(game_key.clone(), "game texture material");
        assert_eq!(cache.len(), 2);
        assert_eq!(cache[&scene_key], "scene texture material");
        assert_eq!(cache[&game_key], "game texture material");
    }

    #[test]
    fn grid_is_flat_indexed_quads_with_two_lines_per_row() {
        let (vertices, indices) = grid_mesh();
        let rows = (GRID_HALF_CELLS * 2 + 1) as usize;
        assert_eq!(vertices.len(), rows * 2 * 4);
        assert_eq!(indices.len(), rows * 2 * 6);
        assert!(vertices.iter().all(|vertex| vertex.position[1] == 0.0));
        assert!(
            indices
                .iter()
                .all(|&index| (index as usize) < vertices.len())
        );
    }

    #[test]
    fn grid_step_follows_the_camera_distance_in_powers_of_ten() {
        assert_eq!(grid_scale(8.0), 1.0);
        assert_eq!(grid_scale(25.0), 10.0);
        assert!((grid_scale(0.2) - 0.1).abs() < 1e-5);
        assert!(grid_scale(0.0) > 0.0);
    }

    #[test]
    fn lod_paths_keep_model_order_and_resolve_texture_inheritance() {
        let mesh = MeshRenderer {
            model: VirtualPath::parse("/models/base.fbx").unwrap(),
            texture: Some(VirtualPath::parse("/textures/base.png").unwrap()),
            material: None,
        };
        let group = LodGroup {
            levels: vec![
                LodLevel {
                    model: "/models/lod1.fbx".to_owned(),
                    texture: Some("/textures/lod.png".to_owned()),
                    distance: 10.0,
                },
                LodLevel {
                    model: "/models/lod2.fbx".to_owned(),
                    texture: None,
                    distance: 20.0,
                },
            ],
            cull_distance: None,
        };
        assert_eq!(
            lod_model_path(&mesh, Some(&group), 0),
            Some("models/base.fbx")
        );
        assert_eq!(
            lod_model_path(&mesh, Some(&group), 1),
            Some("models/lod1.fbx")
        );
        assert_eq!(
            lod_model_path(&mesh, Some(&group), 2),
            Some("models/lod2.fbx")
        );
        assert_eq!(
            lod_texture_path(&mesh, Some(&group), 0),
            Some("textures/base.png")
        );
        assert_eq!(
            lod_texture_path(&mesh, Some(&group), 1),
            Some("textures/lod.png")
        );
        assert_eq!(
            lod_texture_path(&mesh, Some(&group), 2),
            Some("textures/base.png")
        );
    }

    #[test]
    fn scene_without_a_light_uses_the_renderer_default_and_no_shadows() {
        let state = LightState::default();
        let light = frame_light(&state, false);
        assert_eq!(light.direction, DirectionalLight::default().direction);
        assert!(SceneRenderer::shadow_cascades(camera(), 1280, 720, &state, false).is_empty());
    }

    #[test]
    fn scene_light_and_shadow_are_passed_through() {
        let state = LightState {
            direction: [0.0, -1.0, 0.0],
            color: [0.5; 3],
            ambient: [0.1; 3],
            shadow: Some(engine_scene::ShadowSettings {
                distance: 40.0,
                cascades: 2,
            }),
        };
        let light = frame_light(&state, true);
        assert_eq!(light.color, [0.5; 3]);
        assert_eq!(
            SceneRenderer::shadow_cascades(camera(), 1280, 720, &state, true).len(),
            2
        );
        let without_shadow = LightState {
            shadow: None,
            ..state
        };
        assert!(
            SceneRenderer::shadow_cascades(camera(), 1280, 720, &without_shadow, true).is_empty()
        );
    }

    #[test]
    fn scene_and_game_viewports_build_camera_relative_shadow_cascades() {
        let light = LightState {
            shadow: Some(engine_scene::ShadowSettings {
                distance: 60.0,
                cascades: 3,
            }),
            ..LightState::default()
        };
        let first = camera();
        let mut second = first;
        second.position += Vec3::X * 12.0;
        second.target += Vec3::X * 12.0;
        let first_cascades = SceneRenderer::shadow_cascades(first, 1280, 720, &light, true);
        let second_cascades = SceneRenderer::shadow_cascades(second, 1280, 720, &light, true);
        assert_eq!(first_cascades.len(), 3);
        assert_eq!(second_cascades.len(), 3);
        assert_ne!(first_cascades[0], second_cascades[0]);
    }

    #[test]
    fn scene_material_factors_override_fbx_values_and_unset_fields_are_kept() {
        let base = Material {
            base_color: [0.4, 0.5, 0.6],
            metallic: 0.2,
            roughness: 0.8,
            normal_scale: 0.7,
            ..Material::default()
        };
        let override_material = MaterialOverride {
            base_color: Some([0.8, 0.7, 0.6]),
            metallic: Some(1.0),
            roughness: None,
            emissive: Some([2.0, 1.0, 0.5]),
            opacity: Some(0.4),
            alpha_mode: Some(engine_scene::MaterialAlphaMode::Mask),
            alpha_cutoff: Some(0.3),
            metallic_roughness_texture: None,
            normal_texture: None,
            emissive_texture: None,
            occlusion_texture: None,
            normal_scale: Some(0.5),
            occlusion_strength: Some(0.25),
        };

        let resolved =
            resolve_material(base, None, Some(&override_material), None, None, None, None);

        assert_eq!(resolved.base_color, [0.8, 0.7, 0.6]);
        assert_eq!(resolved.metallic, 1.0);
        assert_eq!(resolved.roughness, 0.8);
        assert_eq!(resolved.emissive, [2.0, 1.0, 0.5]);
        assert_eq!(resolved.opacity, 0.4);
        assert_eq!(resolved.alpha_mode, AlphaMode::Mask { cutoff: 0.3 });
        assert_eq!(resolved.normal_scale, 0.5);
        assert_eq!(resolved.occlusion_strength, 0.25);
        assert_eq!(resolved.base_color_texture, None);
        assert_eq!(resolved.metallic_roughness_texture, None);
        assert_eq!(resolved.normal_texture, None);
    }
}
