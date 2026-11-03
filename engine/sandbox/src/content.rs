//! Содержимое кадра sandbox: демонстрационная сцена или модель из assets.

use crate::scene::{
    SHADOWS_AMBIENT, SHADOWS_LIGHT_COLOR, Scene, material_sphere_meshes, materials_plane_mesh,
    plane_mesh, shadows_light_direction, sinusoidal_normal_texture,
};
use engine_assets::{Model, ModelAlphaMode, ModelMaterial, TextureData};
use engine_render::{DirectionalLight, LocalLight, SHADOW_MAP_RESOLUTION, Vertex};
use engine_scene::light::cascade_view_projections;
use engine_scene::{CameraSettings, ShadowSettings};
use glam::{Mat4, Vec3};

/// Меш с CPU-материалом и индексами его текстур в [`Content::textures`].
#[derive(Clone, Debug, PartialEq)]
pub struct Mesh {
    pub vertices: Vec<Vertex>,
    pub indices: Vec<u32>,
    pub material: ModelMaterial,
    /// Окружение остаётся неподвижным и не наследует fit или движение объекта.
    pub environment: bool,
}

/// Меши и текстуры для загрузки в renderer и анимация, которой они следуют.
#[derive(Clone, Debug, PartialEq)]
pub struct Content {
    pub meshes: Vec<Mesh>,
    pub textures: Vec<TextureData>,
    /// Камера и вращение берутся из этой сцены.
    pub motion: Scene,
    /// Переводит содержимое в куб со стороной 1 с центром в начале координат.
    pub fit: Mat4,
}

impl Content {
    pub fn from_scene(scene: Scene) -> Self {
        if scene == Scene::Materials {
            return Self::material_showcase();
        }
        let (vertices, indices) = scene.mesh();
        let textures = scene.texture().into_iter().collect::<Vec<_>>();
        let mut meshes = vec![Mesh {
            vertices,
            indices,
            material: ModelMaterial {
                base_color_texture: (!textures.is_empty()).then_some(0),
                ..ModelMaterial::default()
            },
            environment: false,
        }];
        if scene == Scene::Shadows {
            let (vertices, indices) = plane_mesh();
            meshes.push(Mesh {
                vertices,
                indices,
                material: ModelMaterial::default(),
                environment: true,
            });
        }
        Self {
            meshes,
            textures,
            motion: scene,
            fit: Mat4::IDENTITY,
        }
    }

    fn material_showcase() -> Self {
        let textures = vec![sinusoidal_normal_texture()];
        let mut meshes = material_sphere_meshes()
            .into_iter()
            .map(|(metallic, roughness, vertices, indices)| Mesh {
                vertices,
                indices,
                material: ModelMaterial {
                    base_color: if metallic == 0.0 {
                        [0.68, 0.16, 0.045]
                    } else {
                        [0.82, 0.58, 0.24]
                    },
                    metallic,
                    roughness,
                    emissive: if metallic == 0.0 && roughness == 0.0 {
                        [0.8, 0.08, 0.015]
                    } else {
                        [0.0; 3]
                    },
                    opacity: if metallic == 1.0 && roughness == 1.0 {
                        0.5
                    } else {
                        1.0
                    },
                    alpha_mode: if metallic == 1.0 && roughness == 1.0 {
                        ModelAlphaMode::Blend
                    } else {
                        ModelAlphaMode::Opaque
                    },
                    ..ModelMaterial::default()
                },
                environment: false,
            })
            .collect::<Vec<_>>();
        let (vertices, indices) = materials_plane_mesh();
        meshes.push(Mesh {
            vertices,
            indices,
            material: ModelMaterial {
                base_color: [0.54, 0.58, 0.63],
                roughness: 0.85,
                normal_texture: Some(0),
                normal_scale: 1.0,
                ..ModelMaterial::default()
            },
            environment: true,
        });
        Self {
            meshes,
            textures,
            motion: Scene::Materials,
            fit: Mat4::IDENTITY,
        }
    }

    /// Модель вращается как куб и масштабируется так, чтобы её bounding box
    /// по наибольшей стороне совпал с кубом демонстрационной сцены.
    pub fn from_model(model: Model, scene: Scene) -> Self {
        let fit = model.bounds().map_or(Mat4::IDENTITY, |(min, max)| {
            fit_to_unit_cube(Vec3::from(min), Vec3::from(max))
        });
        let mut meshes = model
            .meshes
            .into_iter()
            .map(|mesh| Mesh {
                vertices: mesh
                    .vertices
                    .into_iter()
                    .map(|vertex| Vertex {
                        position: vertex.position,
                        normal: vertex.normal,
                        uv: vertex.uv,
                        color: [1.0, 1.0, 1.0],
                    })
                    .collect(),
                indices: mesh.indices,
                material: mesh.material,
                environment: false,
            })
            .collect::<Vec<_>>();
        if matches!(scene, Scene::Shadows | Scene::Materials) {
            let (vertices, indices) = if scene == Scene::Materials {
                materials_plane_mesh()
            } else {
                plane_mesh()
            };
            meshes.push(Mesh {
                vertices,
                indices,
                material: ModelMaterial::default(),
                environment: true,
            });
        }
        Self {
            meshes,
            textures: model.textures,
            motion: scene,
            fit,
        }
    }

    /// Заменяет base-color texture всех мешей одной текстурой.
    pub fn override_texture(&mut self, texture: TextureData) {
        let replacement = self.textures.len();
        self.textures.push(texture);
        for mesh in &mut self.meshes {
            mesh.material.base_color_texture = Some(replacement);
        }
    }

    pub fn model(&self, elapsed: f32) -> Mat4 {
        self.motion.model(elapsed) * self.fit
    }

    pub fn view_projection(
        &self,
        elapsed: f32,
        width: u32,
        height: u32,
        settings: CameraSettings,
    ) -> Mat4 {
        self.motion
            .view_projection(elapsed, width, height, settings)
    }

    pub fn light(&self, elapsed: f32) -> DirectionalLight {
        if !matches!(self.motion, Scene::Shadows | Scene::Materials) {
            return DirectionalLight::default();
        }
        DirectionalLight {
            direction: shadows_light_direction(elapsed).to_array(),
            color: SHADOWS_LIGHT_COLOR,
            ambient: SHADOWS_AMBIENT,
        }
    }

    pub fn local_lights(&self) -> Vec<LocalLight> {
        if self.motion == Scene::Clustered {
            let mut lights = (0..144)
                .map(|index| LocalLight::Point {
                    position: [0.0; 3],
                    color_intensity: [
                        1.8 + (index % 3) as f32 * 0.2,
                        1.2 + (index % 5) as f32 * 0.1,
                        0.8 + (index % 7) as f32 * 0.08,
                    ],
                    range: 8.0,
                })
                .collect::<Vec<_>>();
            lights.extend((0..16).map(|_| LocalLight::Spot {
                position: [0.0; 3],
                direction: [0.0, -1.0, 0.0],
                color_intensity: [0.5, 0.8, 1.2],
                range: 8.0,
                cos_inner_angle: 30.0_f32.to_radians().cos(),
                cos_outer_angle: 0.0,
            }));
            return lights;
        }
        if !matches!(self.motion, Scene::Shadows | Scene::Materials) {
            return Vec::new();
        }
        vec![
            LocalLight::Point {
                position: [-2.2, 2.0, 1.5],
                color_intensity: [4.0, 1.25, 0.35],
                range: 8.0,
            },
            LocalLight::Spot {
                position: [2.2, 3.0, 1.8],
                direction: [-0.55, -0.72, -0.43],
                color_intensity: [0.35, 1.5, 4.5],
                range: 10.0,
                cos_inner_angle: 15.0_f32.to_radians().cos(),
                cos_outer_angle: 27.0_f32.to_radians().cos(),
            },
        ]
    }

    pub fn shadow_cascades(
        &self,
        elapsed: f32,
        width: u32,
        height: u32,
        settings: CameraSettings,
    ) -> Vec<[[f32; 4]; 4]> {
        let Some(camera) = self.motion.camera_at(elapsed, settings) else {
            return Vec::new();
        };
        let shadow_settings = ShadowSettings {
            distance: camera.far.min(100.0),
            cascades: 4,
        };
        cascade_view_projections(
            camera,
            width.max(1) as f32 / height.max(1) as f32,
            shadows_light_direction(elapsed),
            shadow_settings,
            SHADOW_MAP_RESOLUTION,
        )
        .into_iter()
        .map(|matrix| matrix.to_cols_array_2d())
        .collect()
    }
}

fn fit_to_unit_cube(min: Vec3, max: Vec3) -> Mat4 {
    let size = (max - min).max_element();
    let scale = if size > 0.0 && size.is_finite() {
        1.0 / size
    } else {
        1.0
    };
    Mat4::from_scale(Vec3::splat(scale)) * Mat4::from_translation(-(min + max) * 0.5)
}

#[cfg(test)]
mod tests {
    use super::{CameraSettings, Content, fit_to_unit_cube};
    use crate::scene::{Scene, cube_mesh};
    use engine_assets::{
        Model, ModelAlphaMode, ModelMaterial, ModelMesh, ModelVertex, TextureData,
    };
    use engine_render::LocalLight;
    use glam::Vec3;

    fn texture(value: u8) -> TextureData {
        TextureData {
            width: 1,
            height: 1,
            rgba: vec![value; 4],
        }
    }

    fn model() -> Model {
        let vertex = |position| ModelVertex {
            position,
            normal: [0.0, 0.0, 1.0],
            uv: [0.25, 0.75],
        };
        Model {
            meshes: vec![
                ModelMesh {
                    vertices: vec![
                        vertex([10.0, 0.0, 0.0]),
                        vertex([14.0, 2.0, 0.0]),
                        vertex([10.0, 2.0, 1.0]),
                    ],
                    indices: vec![0, 1, 2],
                    material: ModelMaterial {
                        base_color: [0.2, 0.3, 0.4],
                        metallic: 0.6,
                        roughness: 0.25,
                        base_color_texture: Some(0),
                        metallic_roughness_texture: Some(0),
                        normal_texture: Some(0),
                        normal_scale: 0.75,
                        ..ModelMaterial::default()
                    },
                },
                ModelMesh {
                    vertices: vec![vertex([12.0, 1.0, 0.5]); 3],
                    indices: vec![0, 1, 2],
                    material: ModelMaterial::default(),
                },
            ],
            textures: vec![texture(7)],
        }
    }

    #[test]
    fn scenes_become_single_mesh_content_with_their_texture() {
        let cube = Content::from_scene(Scene::Cube);
        assert_eq!(cube.meshes.len(), 1);
        assert_eq!(
            (
                cube.meshes[0].vertices.clone(),
                cube.meshes[0].indices.clone()
            ),
            cube_mesh()
        );
        assert_eq!(cube.meshes[0].material.base_color_texture, Some(0));
        assert_eq!(cube.textures.len(), 1);
        assert_eq!(cube.motion, Scene::Cube);

        let triangle = Content::from_scene(Scene::Triangle);
        assert_eq!(triangle.meshes[0].material.base_color_texture, None);
        assert!(triangle.textures.is_empty());
        assert_eq!(triangle.motion, Scene::Triangle);
    }

    #[test]
    fn models_keep_meshes_textures_and_attributes_with_white_vertex_color() {
        let content = Content::from_model(model(), Scene::Cube);
        assert_eq!(content.meshes.len(), 2);
        assert_eq!(content.meshes[0].material.base_color_texture, Some(0));
        assert_eq!(content.meshes[1].material.base_color_texture, None);
        assert_eq!(content.meshes[0].material.metallic, 0.6);
        assert_eq!(content.meshes[0].material.roughness, 0.25);
        assert_eq!(
            content.meshes[0].material.metallic_roughness_texture,
            Some(0)
        );
        assert_eq!(content.meshes[0].material.normal_texture, Some(0));
        assert_eq!(content.meshes[0].material.normal_scale, 0.75);
        assert_eq!(content.textures, [texture(7)]);
        assert_eq!(content.motion, Scene::Cube);
        let vertex = content.meshes[0].vertices[1];
        assert_eq!(vertex.position, [14.0, 2.0, 0.0]);
        assert_eq!(vertex.normal, [0.0, 0.0, 1.0]);
        assert_eq!(vertex.uv, [0.25, 0.75]);
        assert_eq!(vertex.color, [1.0, 1.0, 1.0]);
    }

    #[test]
    fn model_bounds_are_fitted_into_a_centered_unit_cube() {
        let content = Content::from_model(model(), Scene::Cube);
        // Bounding box (10..14, 0..2, 0..1): центр (12, 1, 0.5), наибольшая сторона 4.
        let fitted = |point: [f32; 3]| content.fit.transform_point3(Vec3::from(point));
        assert!(fitted([12.0, 1.0, 0.5]).length() < 1e-6);
        assert!((fitted([14.0, 2.0, 1.0]) - Vec3::new(0.5, 0.25, 0.125)).length() < 1e-6);
        assert!((fitted([10.0, 0.0, 0.0]) - Vec3::new(-0.5, -0.25, -0.125)).length() < 1e-6);
    }

    #[test]
    fn degenerate_bounds_only_center_the_model() {
        let fit = fit_to_unit_cube(Vec3::splat(3.0), Vec3::splat(3.0));
        assert!(fit.transform_point3(Vec3::splat(3.0)).length() < 1e-6);
        assert!((fit.transform_point3(Vec3::splat(4.0)) - Vec3::ONE).length() < 1e-6);
    }

    #[test]
    fn texture_override_applies_to_every_mesh() {
        let mut content = Content::from_model(model(), Scene::Cube);
        content.override_texture(texture(9));
        assert_eq!(content.textures, [texture(7), texture(9)]);
        assert!(
            content
                .meshes
                .iter()
                .all(|mesh| mesh.material.base_color_texture == Some(1))
        );
        assert_eq!(
            content.meshes[0].material.metallic_roughness_texture,
            Some(0)
        );
        assert_eq!(content.meshes[0].material.normal_texture, Some(0));

        let mut triangle = Content::from_scene(Scene::Triangle);
        triangle.override_texture(texture(9));
        assert_eq!(triangle.meshes[0].material.base_color_texture, Some(0));
    }

    #[test]
    fn scene_content_matrices_match_the_scene() {
        for scene in [Scene::Triangle, Scene::Cube] {
            let content = Content::from_scene(scene);
            assert_eq!(content.model(1.5), scene.model(1.5));
            assert_eq!(
                content.view_projection(1.5, 800, 600, CameraSettings::default()),
                scene.view_projection(1.5, 800, 600, CameraSettings::default())
            );
            assert_eq!(
                content.light(1.5),
                engine_render::DirectionalLight::default()
            );
            assert!(
                content
                    .shadow_cascades(1.5, 800, 600, CameraSettings::default())
                    .is_empty()
            );
        }
    }

    #[test]
    fn shadow_scene_adds_untextured_environment_mesh_and_scene_lighting() {
        let content = Content::from_scene(Scene::Shadows);
        assert_eq!(content.meshes.len(), 2);
        assert!(!content.meshes[0].environment);
        assert!(content.meshes[1].environment);
        assert_eq!(content.meshes[1].material.base_color_texture, None);
        assert_eq!(content.light(0.0).color, [1.0; 3]);
        assert_eq!(content.light(0.0).ambient, [0.15; 3]);
        assert_eq!(content.local_lights().len(), 2);
        let cascades = content.shadow_cascades(0.0, 1280, 720, CameraSettings::default());
        assert_eq!(cascades.len(), 4);
        assert!(
            cascades
                .iter()
                .flatten()
                .flatten()
                .all(|value| value.is_finite())
        );
        assert_eq!(
            Content::from_scene(Scene::Cube).light(0.0),
            engine_render::DirectionalLight::default()
        );
        assert!(Content::from_scene(Scene::Cube).local_lights().is_empty());
        assert!(
            Content::from_scene(Scene::Cube)
                .shadow_cascades(0.0, 1280, 720, CameraSettings::default())
                .is_empty()
        );
        assert!(
            Content::from_scene(Scene::Triangle)
                .shadow_cascades(0.0, 1280, 720, CameraSettings::default())
                .is_empty()
        );
    }

    #[test]
    fn clustered_scene_provides_same_cluster_overflow_and_mixed_light_stress() {
        let content = Content::from_scene(Scene::Clustered);
        let lights = content.local_lights();
        assert_eq!(lights.len(), 160);
        assert_eq!(
            lights
                .iter()
                .filter(|light| matches!(light, LocalLight::Point { .. }))
                .count(),
            144
        );
        assert_eq!(
            lights
                .iter()
                .filter(|light| matches!(light, LocalLight::Spot { .. }))
                .count(),
            16
        );
        assert!(
            content
                .view_projection(0.0, 1280, 720, CameraSettings::default())
                .is_finite()
        );
    }

    #[test]
    fn materials_scene_has_dielectric_and_metal_roughness_rows_and_bumped_floor() {
        let content = Content::from_scene(Scene::Materials);
        assert_eq!(content.meshes.len(), 11);
        assert_eq!(content.textures.len(), 1);
        assert!(content.meshes[..5].iter().all(|mesh| {
            mesh.material.metallic == 0.0 && mesh.material.normal_texture.is_none()
        }));
        assert!(content.meshes[5..10].iter().all(|mesh| {
            mesh.material.metallic == 1.0 && mesh.material.normal_texture.is_none()
        }));
        assert_eq!(
            content.meshes[..5]
                .iter()
                .map(|mesh| mesh.material.roughness)
                .collect::<Vec<_>>(),
            [0.0, 0.25, 0.5, 0.75, 1.0]
        );
        assert_eq!(
            content.meshes[5..10]
                .iter()
                .map(|mesh| mesh.material.roughness)
                .collect::<Vec<_>>(),
            [0.0, 0.25, 0.5, 0.75, 1.0]
        );
        assert_eq!(content.meshes[0].material.emissive, [0.8, 0.08, 0.015]);
        assert_eq!(content.meshes[9].material.opacity, 0.5);
        assert_eq!(content.meshes[9].material.alpha_mode, ModelAlphaMode::Blend);
        let floor = content.meshes.last().unwrap();
        assert!(floor.environment);
        assert_eq!(floor.material.normal_texture, Some(0));
        assert_eq!(floor.vertices[0].position, [-4.0, -1.0, -4.0]);
        assert_eq!(content.light(0.0).ambient, [0.15; 3]);
        assert_eq!(
            content
                .shadow_cascades(0.0, 1280, 720, CameraSettings::default())
                .len(),
            4
        );
    }

    #[test]
    fn model_uses_selected_scene_motion_and_shadows_add_unfitted_plane() {
        let cube = Content::from_model(model(), Scene::Cube);
        let shadows = Content::from_model(model(), Scene::Shadows);
        assert_eq!(cube.meshes.len(), 2);
        assert_eq!(shadows.meshes.len(), 3);
        assert!(shadows.meshes[..2].iter().all(|mesh| !mesh.environment));
        assert!(shadows.meshes[2].environment);
        assert!(shadows.meshes[2].material.base_color_texture.is_none());
        assert_eq!(shadows.motion, Scene::Shadows);
        assert_eq!(shadows.model(0.0), Scene::Shadows.model(0.0) * shadows.fit);
        assert_eq!(cube.model(1.0), Scene::Cube.model(1.0) * cube.fit);
    }

    #[test]
    fn texture_override_also_covers_shadow_environment() {
        let mut content = Content::from_scene(Scene::Shadows);
        content.override_texture(texture(11));
        assert_eq!(content.textures.len(), 2);
        assert_eq!(content.textures[1], texture(11));
        assert!(
            content
                .meshes
                .iter()
                .all(|mesh| mesh.material.base_color_texture == Some(1))
        );
    }
}
