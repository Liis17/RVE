//! Подготовка кадра сцены: GPU-кэш ресурсов и команды отрисовки.

use crate::console::ConsoleLog;
use crate::project::OpenProject;
use engine_assets::Model;
use engine_render::{
    AntiAliasing as RenderAntiAliasing, DirectionalLight, DrawCommand, Frame, Material, MaterialId,
    MeshId, RenderError, Renderer, TextureColorSpace, TextureId, Tonemapper as RenderTonemapper,
    Vertex, ViewSettings, ViewportId,
};
use engine_scene::document::MaterialOverride;
use engine_scene::light::shadow_view_projection;
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
    object_materials: HashMap<(ObjectHandle, usize), (Material, MaterialId)>,
    grid: Option<MeshId>,
    /// Команды кадра; сетка, если есть, идёт последней после `scene_draws` команд мира.
    draws: Vec<DrawCommand>,
    scene_draws: usize,
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
            draws: Vec::new(),
            scene_draws: 0,
        })
    }

    /// Удаляет кэшированные ресурсы: файлы на диске могли измениться.
    pub fn clear_cache(
        &mut self,
        renderer: &mut Renderer,
        cpu_models: &mut HashMap<String, Arc<Model>>,
    ) -> Result<(), RenderError> {
        for model in self.models.drain().filter_map(|(_, model)| model) {
            for (mesh, _, material) in model.meshes {
                renderer.destroy_mesh(mesh)?;
                renderer.destroy_material(material)?;
            }
            for texture in model.textures {
                renderer.destroy_texture(texture)?;
            }
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

    /// Загружает ресурсы объектов мира, которых ещё нет в кэше.
    pub fn sync_assets(
        &mut self,
        renderer: &mut Renderer,
        project: &OpenProject,
        world: &World,
        cpu_models: &mut HashMap<String, Arc<Model>>,
        log: &mut ConsoleLog,
    ) -> Result<(), RenderError> {
        for object in world.objects() {
            let Some(mesh) = &object.mesh_renderer else {
                continue;
            };
            let model_path = mesh.model.as_str();
            if !self.models.contains_key(model_path) {
                let gpu = match project.assets.load_model(model_path) {
                    Ok(model) => {
                        let model = Arc::new(model);
                        let gpu = upload_model(renderer, &model)?;
                        cpu_models.insert(model_path.to_owned(), model);
                        Some(gpu)
                    }
                    Err(error) => {
                        log.error(format!("{model_path}: {error}"));
                        None
                    }
                };
                self.models.insert(model_path.to_owned(), gpu);
            }
            let override_material = mesh.material.as_ref();
            for (path, color_space) in [
                mesh.texture
                    .as_ref()
                    .map(|path| (path.as_str(), TextureColorSpace::Srgb)),
                override_material
                    .and_then(|material| material.metallic_roughness_texture.as_ref())
                    .map(|path| (path.as_str(), TextureColorSpace::Linear)),
                override_material
                    .and_then(|material| material.normal_texture.as_ref())
                    .map(|path| (path.as_str(), TextureColorSpace::Linear)),
            ]
            .into_iter()
            .flatten()
            {
                let key = (path.to_owned(), color_space);
                if self.textures.contains_key(&key) {
                    continue;
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
            }
        }
        Ok(())
    }

    /// Строит команды отрисовки кадра мира; `grid_camera_distance` добавляет сетку.
    pub fn build_draws(
        &mut self,
        renderer: &mut Renderer,
        world: &World,
        frame: &SceneFrame,
        grid_camera_distance: Option<f32>,
    ) -> Result<(), RenderError> {
        self.draws.clear();
        let mut used_object_materials = HashSet::new();
        for instance in &frame.meshes {
            let Some(object) = world.object(instance.object) else {
                continue;
            };
            let Some(mesh) = &object.mesh_renderer else {
                continue;
            };
            let Some(Some(model)) = self.models.get(mesh.model.as_str()) else {
                continue;
            };
            let replacement = mesh.texture.as_ref().and_then(|path| {
                self.textures
                    .get(&(path.as_str().to_owned(), TextureColorSpace::Srgb))
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
                        self.textures
                            .get(&(path.as_str().to_owned(), TextureColorSpace::Linear))
                            .copied()
                            .flatten()
                    });
                let normal_texture = override_material
                    .and_then(|material| material.normal_texture.as_ref())
                    .and_then(|path| {
                        self.textures
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
                );
                let material_id = if resolved == *base_material {
                    *base_material_id
                } else {
                    let key = (instance.object, mesh_index);
                    used_object_materials.insert(key);
                    match self.object_materials.get(&key) {
                        Some((cached, id)) if *cached == resolved => *id,
                        previous => {
                            let id = renderer.create_material(&resolved)?;
                            if let Some((_, old_id)) = previous {
                                renderer.destroy_material(*old_id)?;
                            }
                            self.object_materials.insert(key, (resolved, id));
                            id
                        }
                    }
                };
                self.draws.push(DrawCommand {
                    mesh: *mesh_id,
                    material: Some(material_id),
                    model: instance.model.to_cols_array_2d(),
                    casts_shadow: true,
                });
            }
        }
        let stale = self
            .object_materials
            .keys()
            .filter(|key| !used_object_materials.contains(key))
            .copied()
            .collect::<Vec<_>>();
        for key in stale {
            if let Some((_, material)) = self.object_materials.remove(&key) {
                renderer.destroy_material(material)?;
            }
        }
        self.scene_draws = self.draws.len();
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
            self.draws.push(DrawCommand {
                mesh: grid,
                material: None,
                model: Mat4::from_scale(Vec3::splat(grid_scale(distance))).to_cols_array_2d(),
                casts_shadow: false,
            });
        }
        Ok(())
    }

    /// Собирает `Frame` для заданной матрицы камеры; `with_grid` включает сетку редактора.
    pub fn frame(
        &self,
        view_projection: Mat4,
        camera_position: Vec3,
        settings: CameraSettings,
        light: &LightState,
        scene_has_light: bool,
        with_grid: bool,
    ) -> Frame<'_> {
        let (light, shadow) = frame_light(light, scene_has_light);
        Frame {
            clear_color: [
                settings.render.clear_color[0],
                settings.render.clear_color[1],
                settings.render.clear_color[2],
                1.0,
            ],
            view_projection: view_projection.to_cols_array_2d(),
            shadow_view_projection: shadow,
            camera_position: camera_position.to_array(),
            light,
            draws: if with_grid {
                &self.draws
            } else {
                &self.draws[..self.scene_draws]
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
                normal_scale: base.normal_scale,
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
        if let Some(texture) = metallic_roughness_texture {
            base.metallic_roughness_texture = Some(texture);
        }
        if let Some(texture) = normal_texture {
            base.normal_texture = Some(texture);
        }
        if let Some(scale) = override_material.normal_scale {
            base.normal_scale = scale;
        }
    }
    base
}

/// Свет кадра. Сцена без directional light показывается светом renderer по умолчанию,
/// иначе объекты редактора выглядели бы плоско залитыми.
fn frame_light(
    light: &LightState,
    scene_has_light: bool,
) -> (DirectionalLight, Option<[[f32; 4]; 4]>) {
    if !scene_has_light {
        return (DirectionalLight::default(), None);
    }
    let shadow = light.shadow.map(|(center, radius)| {
        shadow_view_projection(Vec3::from_array(light.direction), center, radius).to_cols_array_2d()
    });
    (
        DirectionalLight {
            direction: light.direction,
            color: light.color,
            ambient: light.ambient,
        },
        shadow,
    )
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
    fn scene_without_a_light_uses_the_renderer_default_and_no_shadows() {
        let state = LightState::default();
        let (light, shadow) = frame_light(&state, false);
        assert_eq!(light.direction, DirectionalLight::default().direction);
        assert_eq!(shadow, None);
    }

    #[test]
    fn scene_light_and_shadow_are_passed_through() {
        let state = LightState {
            direction: [0.0, -1.0, 0.0],
            color: [0.5; 3],
            ambient: [0.1; 3],
            shadow: Some((Vec3::ZERO, 10.0)),
        };
        let (light, shadow) = frame_light(&state, true);
        assert_eq!(light.color, [0.5; 3]);
        assert!(shadow.is_some());
        let without_shadow = LightState {
            shadow: None,
            ..state
        };
        assert_eq!(frame_light(&without_shadow, true).1, None);
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
            metallic_roughness_texture: None,
            normal_texture: None,
            normal_scale: Some(0.5),
        };

        let resolved = resolve_material(base, None, Some(&override_material), None, None);

        assert_eq!(resolved.base_color, [0.8, 0.7, 0.6]);
        assert_eq!(resolved.metallic, 1.0);
        assert_eq!(resolved.roughness, 0.8);
        assert_eq!(resolved.normal_scale, 0.5);
        assert_eq!(resolved.base_color_texture, None);
        assert_eq!(resolved.metallic_roughness_texture, None);
        assert_eq!(resolved.normal_texture, None);
    }
}
