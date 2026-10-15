//! Содержимое кадра sandbox: демонстрационная сцена или модель из assets.

use crate::scene::{
    SHADOWS_AMBIENT, SHADOWS_LIGHT_COLOR, Scene, plane_mesh, shadows_light_direction,
    shadows_light_view_projection,
};
use engine_assets::{Model, TextureData};
use engine_render::{DirectionalLight, Vertex};
use glam::{Mat4, Vec3};

/// Меш с индексом текстуры в [`Content::textures`].
#[derive(Clone, Debug, PartialEq)]
pub struct Mesh {
    pub vertices: Vec<Vertex>,
    pub indices: Vec<u32>,
    pub texture: Option<usize>,
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
        let (vertices, indices) = scene.mesh();
        let textures = scene.texture().into_iter().collect::<Vec<_>>();
        let mut meshes = vec![Mesh {
            vertices,
            indices,
            texture: (!textures.is_empty()).then_some(0),
            environment: false,
        }];
        if scene == Scene::Shadows {
            let (vertices, indices) = plane_mesh();
            meshes.push(Mesh {
                vertices,
                indices,
                texture: None,
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
                texture: mesh.texture,
                environment: false,
            })
            .collect::<Vec<_>>();
        if scene == Scene::Shadows {
            let (vertices, indices) = plane_mesh();
            meshes.push(Mesh {
                vertices,
                indices,
                texture: None,
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

    /// Заменяет текстуры всех мешей одной текстурой.
    pub fn override_texture(&mut self, texture: TextureData) {
        self.textures = vec![texture];
        for mesh in &mut self.meshes {
            mesh.texture = Some(0);
        }
    }

    pub fn model(&self, elapsed: f32) -> Mat4 {
        self.motion.model(elapsed) * self.fit
    }

    pub fn view_projection(&self, elapsed: f32, width: u32, height: u32) -> Mat4 {
        self.motion.view_projection(elapsed, width, height)
    }

    pub fn light(&self, elapsed: f32) -> DirectionalLight {
        if self.motion != Scene::Shadows {
            return DirectionalLight::default();
        }
        DirectionalLight {
            direction: shadows_light_direction(elapsed).to_array(),
            color: SHADOWS_LIGHT_COLOR,
            ambient: SHADOWS_AMBIENT,
        }
    }

    pub fn shadow_view_projection(&self, elapsed: f32) -> Option<Mat4> {
        (self.motion == Scene::Shadows).then(|| shadows_light_view_projection(elapsed))
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
    use super::{Content, fit_to_unit_cube};
    use crate::scene::{Scene, cube_mesh};
    use engine_assets::{Model, ModelMesh, ModelVertex, TextureData};
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
                    texture: Some(0),
                },
                ModelMesh {
                    vertices: vec![vertex([12.0, 1.0, 0.5]); 3],
                    indices: vec![0, 1, 2],
                    texture: None,
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
        assert_eq!(cube.meshes[0].texture, Some(0));
        assert_eq!(cube.textures.len(), 1);
        assert_eq!(cube.motion, Scene::Cube);

        let triangle = Content::from_scene(Scene::Triangle);
        assert_eq!(triangle.meshes[0].texture, None);
        assert!(triangle.textures.is_empty());
        assert_eq!(triangle.motion, Scene::Triangle);
    }

    #[test]
    fn models_keep_meshes_textures_and_attributes_with_white_vertex_color() {
        let content = Content::from_model(model(), Scene::Cube);
        assert_eq!(content.meshes.len(), 2);
        assert_eq!(content.meshes[0].texture, Some(0));
        assert_eq!(content.meshes[1].texture, None);
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
        assert_eq!(content.textures, [texture(9)]);
        assert!(content.meshes.iter().all(|mesh| mesh.texture == Some(0)));

        let mut triangle = Content::from_scene(Scene::Triangle);
        triangle.override_texture(texture(9));
        assert_eq!(triangle.meshes[0].texture, Some(0));
    }

    #[test]
    fn scene_content_matrices_match_the_scene() {
        for scene in [Scene::Triangle, Scene::Cube] {
            let content = Content::from_scene(scene);
            assert_eq!(content.model(1.5), scene.model(1.5));
            assert_eq!(
                content.view_projection(1.5, 800, 600),
                scene.view_projection(1.5, 800, 600)
            );
            assert_eq!(
                content.light(1.5),
                engine_render::DirectionalLight::default()
            );
            assert!(content.shadow_view_projection(1.5).is_none());
        }
    }

    #[test]
    fn shadow_scene_adds_untextured_environment_mesh_and_scene_lighting() {
        let content = Content::from_scene(Scene::Shadows);
        assert_eq!(content.meshes.len(), 2);
        assert!(!content.meshes[0].environment);
        assert!(content.meshes[1].environment);
        assert_eq!(content.meshes[1].texture, None);
        assert_eq!(content.light(0.0).color, [1.0; 3]);
        assert_eq!(content.light(0.0).ambient, [0.15; 3]);
        assert!(content.shadow_view_projection(0.0).is_some());
        assert_eq!(
            Content::from_scene(Scene::Cube).light(0.0),
            engine_render::DirectionalLight::default()
        );
        assert!(
            Content::from_scene(Scene::Cube)
                .shadow_view_projection(0.0)
                .is_none()
        );
        assert!(
            Content::from_scene(Scene::Triangle)
                .shadow_view_projection(0.0)
                .is_none()
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
        assert!(shadows.meshes[2].texture.is_none());
        assert_eq!(shadows.motion, Scene::Shadows);
        assert_eq!(shadows.model(0.0), Scene::Shadows.model(0.0) * shadows.fit);
        assert_eq!(cube.model(1.0), Scene::Cube.model(1.0) * cube.fit);
    }

    #[test]
    fn texture_override_also_covers_shadow_environment() {
        let mut content = Content::from_scene(Scene::Shadows);
        content.override_texture(texture(11));
        assert_eq!(content.textures, [texture(11)]);
        assert!(content.meshes.iter().all(|mesh| mesh.texture == Some(0)));
    }
}
