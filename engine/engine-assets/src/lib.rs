//! Загрузка и хранение текстур, моделей и других ресурсов.
//!
//! Ресурсы лежат в каталоге [`ASSETS_DIR`] рабочего каталога и адресуются путями
//! вида `/model/cube.fbx`: такой путь указывает на файл `assets/model/cube.fbx`.

mod animation;
mod prebuilt;
mod skin;

pub use animation::{ModelAnimation, ModelKey, ModelTrack};
/// Возвращает `true`, если байты начинаются с сигнатуры готовой модели.
pub use prebuilt::{PREBUILT_MODEL_MAX_BYTES, PREBUILT_MODEL_VERSION, is_prebuilt_model};
pub use skin::{ModelInheritMode, ModelJoint, ModelNode, ModelSkeleton, ModelSkin, ModelTransform};

#[cfg(feature = "fbx")]
use bevy_mikktspace::{Geometry, TangentSpace};
use engine_resources::{ResourceError, ResourceErrorKind, ResourceHandle, ResourceStat};
use std::collections::{HashMap, HashSet};
use std::error::Error;
use std::fmt;
use std::fs;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

/// Версия крейта (SemVer).
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Каталог ресурсов относительно рабочего каталога приложения.
pub const ASSETS_DIR: &str = "assets";

/// Предел памяти декодера PNG; хватает на текстуру 8192x8192 RGBA с запасом.
const PNG_DECODE_LIMIT: usize = 512 * 1024 * 1024;
const HDR_DECODE_LIMIT: usize = 512 * 1024 * 1024;

/// Пиксели RGBA8 по строкам сверху вниз. Цветовое пространство задаёт использующий материал:
/// base-color карты являются sRGB, metallic-roughness и normal карты — linear.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TextureData {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}

/// Линейные RGB-пиксели Radiance HDR сверху вниз.
#[derive(Clone, Debug, PartialEq)]
pub struct HdrImageData {
    pub width: u32,
    pub height: u32,
    pub rgb: Vec<f32>,
}

/// Вершина модели в метрах; оси: +X вправо, +Y вверх, +Z к зрителю.
///
/// `uv` отсчитывается от левого верхнего угла текстуры.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ModelVertex {
    pub position: [f32; 3],
    pub normal: [f32; 3],
    pub uv: [f32; 2],
    /// Единичное направление ∂P/∂u в пространстве модели (xyz) и знак `w` ±1.
    ///
    /// Bitangent равен `cross(normal, tangent.xyz) * w` и указывает в сторону роста
    /// исходного FBX `v`, то есть в сторону убывания `uv.y` (загрузчик пишет `uv.y = 1 − v`).
    /// `[0, 0, 0, 0]` — tangent отсутствует: у модели нет UV или базис вырожденный.
    pub tangent: [f32; 4],
}

/// Часть модели с одним материалом; каждые три индекса образуют треугольник.
///
/// `skin` задан, если у модели есть скелет: тогда каждая вершина связана с костями.
#[derive(Clone, Debug, PartialEq)]
pub struct ModelMesh {
    pub vertices: Vec<ModelVertex>,
    pub indices: Vec<u32>,
    pub material: ModelMaterial,
    pub skin: Option<ModelSkin>,
}

/// Режим альфа-смешивания, извлечённый из FBX opacity.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ModelAlphaMode {
    #[default]
    Opaque,
    Blend,
}

/// Линейные факторы, двусторонность и индексы текстур материала модели.
#[derive(Clone, Debug, PartialEq)]
pub struct ModelMaterial {
    pub base_color: [f32; 3],
    pub metallic: f32,
    pub roughness: f32,
    pub emissive: [f32; 3],
    pub opacity: f32,
    pub alpha_mode: ModelAlphaMode,
    pub double_sided: bool,
    /// Индекс в [`Model::textures`] для sRGB base-color.
    pub base_color_texture: Option<usize>,
    /// Индекс в [`Model::textures`] для линейной metallic-roughness карты.
    pub metallic_roughness_texture: Option<usize>,
    /// Индекс в [`Model::textures`] для линейной tangent-space normal карты.
    pub normal_texture: Option<usize>,
    /// Индекс в [`Model::textures`] для sRGB emissive карты.
    pub emissive_texture: Option<usize>,
    /// Индекс в [`Model::textures`] для линейной ambient-occlusion карты.
    pub occlusion_texture: Option<usize>,
    pub normal_scale: f32,
    pub occlusion_strength: f32,
}

impl Default for ModelMaterial {
    fn default() -> Self {
        Self {
            base_color: [1.0; 3],
            metallic: 0.0,
            roughness: 1.0,
            emissive: [0.0; 3],
            opacity: 1.0,
            alpha_mode: ModelAlphaMode::Opaque,
            double_sided: false,
            base_color_texture: None,
            metallic_roughness_texture: None,
            normal_texture: None,
            emissive_texture: None,
            occlusion_texture: None,
            normal_scale: 1.0,
            occlusion_strength: 1.0,
        }
    }
}

/// Модель, собранная из всех мешей файла в мировом пространстве сцены.
///
/// `skeleton` и `animations` заданы, если в FBX есть skin или анимационные стеки;
/// иначе модель статическая, как и раньше.
#[derive(Clone, Debug, PartialEq)]
pub struct Model {
    pub meshes: Vec<ModelMesh>,
    pub textures: Vec<TextureData>,
    pub skeleton: Option<ModelSkeleton>,
    pub animations: Vec<ModelAnimation>,
}

impl Model {
    /// Минимальный и максимальный углы bounding box всех вершин.
    pub fn bounds(&self) -> Option<([f32; 3], [f32; 3])> {
        bounds_from_positions(
            self.meshes
                .iter()
                .flat_map(|mesh| &mesh.vertices)
                .map(|vertex| vertex.position),
        )
    }
}

fn bounds_from_positions(
    positions: impl IntoIterator<Item = [f32; 3]>,
) -> Option<([f32; 3], [f32; 3])> {
    let mut positions = positions.into_iter();
    let first = positions.next()?;
    Some(positions.fold((first, first), |(min, max), position| {
        (
            std::array::from_fn(|axis| min[axis].min(position[axis])),
            std::array::from_fn(|axis| max[axis].max(position[axis])),
        )
    }))
}

/// Ошибка поиска или загрузки ресурса.
#[derive(Debug)]
pub enum AssetError {
    InvalidPath {
        path: String,
        reason: &'static str,
    },
    UnsupportedFormat {
        path: PathBuf,
        expected: &'static str,
    },
    Io {
        path: PathBuf,
        error: io::Error,
    },
    Decode {
        path: PathBuf,
        message: String,
    },
    MissingTexture {
        model: PathBuf,
        texture: String,
        tried: Vec<PathBuf>,
    },
    /// Загрузка прервана флагом отмены вызывающего.
    Cancelled {
        path: PathBuf,
    },
}

impl fmt::Display for AssetError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidPath { path, reason } => {
                write!(f, "некорректный путь ресурса {path:?}: {reason}")
            }
            Self::UnsupportedFormat { path, expected } => write!(
                f,
                "{}: неподдерживаемый формат, ожидается {expected}",
                path.display()
            ),
            Self::Io { path, error } => write!(f, "{}: {error}", path.display()),
            Self::Decode { path, message } => write!(f, "{}: {message}", path.display()),
            Self::Cancelled { path } => write!(f, "{}: загрузка отменена", path.display()),
            Self::MissingTexture {
                model,
                texture,
                tried,
            } => {
                write!(
                    f,
                    "{}: не найдена текстура {texture:?}; проверены пути:",
                    model.display()
                )?;
                for path in tried {
                    write!(f, " {}", path.display())?;
                }
                Ok(())
            }
        }
    }
}

impl Error for AssetError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Io { error, .. } => Some(error),
            _ => None,
        }
    }
}

/// Каталог ресурсов и загрузка файлов из него.
#[derive(Clone, Debug)]
pub struct Assets {
    root: PathBuf,
    resources: Option<ResourceHandle>,
}

impl Assets {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            resources: None,
        }
    }

    /// Uses a background resource handle for every loose or packed byte read.
    pub fn with_resources(resources: ResourceHandle) -> Self {
        Self {
            root: resources.root().to_owned(),
            resources: Some(resources),
        }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Переводит путь вида `/model/cube.fbx` в путь файла внутри каталога ресурсов.
    ///
    /// Ведущий `/` необязателен. Компоненты разделяются `/`; пустые компоненты,
    /// `.`, `..`, `\` и `:` запрещены, поэтому путь не выходит за пределы каталога.
    pub fn resolve(&self, asset_path: &str) -> Result<PathBuf, AssetError> {
        let invalid = |reason| AssetError::InvalidPath {
            path: asset_path.to_owned(),
            reason,
        };
        let relative = asset_path.strip_prefix('/').unwrap_or(asset_path);
        if relative.is_empty() {
            return Err(invalid("путь пуст"));
        }
        let mut path = self.root.clone();
        for component in relative.split('/') {
            match component {
                "" => return Err(invalid("пустой компонент пути")),
                "." | ".." => return Err(invalid("компоненты . и .. запрещены")),
                _ if component.contains(['\\', ':']) => {
                    return Err(invalid("символы \\ и : запрещены, разделитель — /"));
                }
                _ => path.push(component),
            }
        }
        Ok(path)
    }

    /// Загружает PNG-текстуру и приводит её к RGBA8.
    pub fn load_texture(&self, asset_path: &str) -> Result<TextureData, AssetError> {
        let path = self.resolve(asset_path)?;
        require_extension(&path, "png")?;
        let bytes = self.read_asset(asset_path, &path)?;
        decode_png(&bytes).map_err(|message| AssetError::Decode { path, message })
    }

    /// Загружает Radiance HDR equirectangular изображение.
    pub fn load_hdr(&self, asset_path: &str) -> Result<HdrImageData, AssetError> {
        let path = self.resolve(asset_path)?;
        require_extension(&path, "hdr")?;
        let stat = self.stat_asset(asset_path, &path)?;
        if stat.size > HDR_DECODE_LIMIT as u64 {
            return Err(AssetError::Decode {
                path,
                message: "HDR превышает предел памяти декодера 512 MiB".to_owned(),
            });
        }
        let bytes = self.read_asset(asset_path, &path)?;
        decode_hdr(&bytes).map_err(|message| AssetError::Decode { path, message })
    }

    /// Загружает FBX-модель с её PNG-текстурами материалов.
    ///
    /// Внешняя текстура ищется по пути, записанному в FBX относительно файла модели,
    /// а затем по имени файла рядом с моделью. Встроенные в FBX PNG тоже поддерживаются.
    pub fn load_model(&self, asset_path: &str) -> Result<Model, AssetError> {
        self.load_model_cancellable(asset_path, &AtomicBool::new(false))
    }

    /// Как [`Self::load_model`], но разбор прерывается с [`AssetError::Cancelled`], как только
    /// `cancel` становится `true`: флаг проверяется во время разбора FBX и между частями модели.
    pub fn load_model_cancellable(
        &self,
        asset_path: &str,
        cancel: &AtomicBool,
    ) -> Result<Model, AssetError> {
        let path = self.resolve(asset_path)?;
        require_extension(&path, "fbx")?;
        check_cancel(cancel, &path)?;
        let bytes = self.read_model_asset(asset_path, &path)?;
        if prebuilt::is_prebuilt_model(&bytes) {
            load_prebuilt_model(&bytes, &path, &virtual_asset_path(asset_path), self, cancel)
        } else {
            #[cfg(feature = "fbx")]
            {
                load_fbx(&bytes, &virtual_asset_path(asset_path), &path, self, cancel)
            }
            #[cfg(not(feature = "fbx"))]
            {
                Err(unsupported_fbx(&path))
            }
        }
    }

    /// Преобразует FBX в версионированный артефакт готовой модели.
    #[cfg(feature = "fbx")]
    pub fn convert_model(&self, asset_path: &str) -> Result<Vec<u8>, AssetError> {
        let path = self.resolve(asset_path)?;
        require_extension(&path, "fbx")?;
        let bytes = self.read_model_asset(asset_path, &path)?;
        self.convert_model_from_bytes(asset_path, &bytes)
    }

    /// Преобразует переданный снимок FBX в артефакт готовой модели.
    ///
    /// Метод позволяет сборщику хешировать и преобразовывать одни и те же байты, не перечитывая
    /// исходный файл между этими операциями.
    #[cfg(feature = "fbx")]
    pub fn convert_model_from_bytes(
        &self,
        asset_path: &str,
        bytes: &[u8],
    ) -> Result<Vec<u8>, AssetError> {
        let path = self.resolve(asset_path)?;
        require_extension(&path, "fbx")?;
        let virtual_path = virtual_asset_path(asset_path);
        let scene = load_fbx_scene(bytes, &virtual_path, &path)?;
        let mut meshes = Vec::new();
        let prepared = prepare_fbx_parts(&scene, &path, &AtomicBool::new(false), |mesh, _| {
            meshes.push(mesh);
            Ok(())
        })?;
        if meshes.is_empty() && !prepared.has_animation_stacks {
            return Err(AssetError::Decode {
                path,
                message: "в модели нет треугольников".to_owned(),
            });
        }
        let (skeleton, model_animations) = if meshes.is_empty() {
            (None, Vec::new())
        } else if let Some(builder) = prepared.skeleton_builder {
            let animations =
                animation::bake_model_animations(&builder, &scene).map_err(|message| {
                    AssetError::Decode {
                        path: path.clone(),
                        message,
                    }
                })?;
            (Some(builder.finish()), animations)
        } else {
            (None, Vec::new())
        };
        let clips = animation::bake_clip_file(
            &scene,
            path.file_stem()
                .and_then(|name| name.to_str())
                .unwrap_or("clip"),
        )
        .map_err(|message| AssetError::Decode {
            path: path.clone(),
            message,
        })?;
        prebuilt::encode(&prebuilt::PreparedModel {
            texture_references: prepared.texture_references,
            meshes: (!meshes.is_empty()).then_some(meshes),
            skeleton,
            model_animations,
            has_animation_stacks: prepared.has_animation_stacks,
            clips,
        })
        .map_err(|message| AssetError::Decode { path, message })
    }

    /// Загружает клипы из FBX, в котором нет мешей, например файл Mixamo только с анимацией.
    ///
    /// Имя клипа — stem файла; если в файле несколько стеков, имя формируется как `stem:stack`.
    /// Узлы сопоставляются со скелетом модели позже, по именам.
    pub fn load_animations(&self, asset_path: &str) -> Result<Vec<ModelAnimation>, AssetError> {
        let path = self.resolve(asset_path)?;
        require_extension(&path, "fbx")?;
        let bytes = self.read_model_asset(asset_path, &path)?;
        if prebuilt::is_prebuilt_model(&bytes) {
            let model = decode_prebuilt(&bytes, &path)?;
            if model.clips.is_empty() {
                return Err(AssetError::Decode {
                    path,
                    message: "нет анимационных стеков с ключами".to_owned(),
                });
            }
            return Ok(model.clips);
        }
        #[cfg(feature = "fbx")]
        {
            let virtual_path = virtual_asset_path(asset_path);
            let scene = load_fbx_scene(&bytes, &virtual_path, &path)?;
            let stem = path
                .file_stem()
                .and_then(|stem| stem.to_str())
                .unwrap_or("clip");
            let clips =
                animation::bake_clip_file(&scene, stem).map_err(|message| AssetError::Decode {
                    path: path.clone(),
                    message,
                })?;
            if clips.is_empty() {
                return Err(AssetError::Decode {
                    path,
                    message: "нет анимационных стеков с ключами".to_owned(),
                });
            }
            Ok(clips)
        }
        #[cfg(not(feature = "fbx"))]
        {
            Err(unsupported_fbx(&path))
        }
    }

    /// Возвращает внешние PNG-зависимости FBX без декодирования изображений.
    ///
    /// Встроенные изображения пропускаются. Внешние пути возвращаются как
    /// канонические virtual paths с ведущим `/`, в порядке использования моделью.
    pub fn model_dependencies(&self, asset_path: &str) -> Result<Vec<String>, AssetError> {
        let path = self.resolve(asset_path)?;
        require_extension(&path, "fbx")?;
        let bytes = self.read_model_asset(asset_path, &path)?;
        self.model_dependencies_from_bytes(asset_path, &bytes)
    }

    /// Возвращает внешние текстурные зависимости для переданного снимка FBX или готовой модели.
    pub fn model_dependencies_from_bytes(
        &self,
        asset_path: &str,
        bytes: &[u8],
    ) -> Result<Vec<String>, AssetError> {
        let path = self.resolve(asset_path)?;
        require_extension(&path, "fbx")?;
        ensure_model_data_size(bytes, &path, PREBUILT_MODEL_MAX_BYTES)?;
        if prebuilt::is_prebuilt_model(bytes) {
            let model = decode_prebuilt(bytes, &path)?;
            if model.meshes.is_none() {
                if model.has_animation_stacks {
                    return Ok(Vec::new());
                }
                return Err(AssetError::Decode {
                    path,
                    message: "в модели нет треугольников".to_owned(),
                });
            }
            return prebuilt_dependencies(&model, &virtual_asset_path(asset_path), &path, self);
        }
        #[cfg(feature = "fbx")]
        {
            let virtual_path = virtual_asset_path(asset_path);
            let scene = load_fbx_scene(bytes, &virtual_path, &path)?;
            if !scene_has_triangles(&scene) {
                // Файл только с клипами (например, из Mixamo) не является моделью и зависимостей не имеет.
                if !scene.anim_stacks.is_empty() {
                    return Ok(Vec::new());
                }
                return Err(AssetError::Decode {
                    path,
                    message: "в модели нет треугольников".to_owned(),
                });
            }
            let mut dependencies = Vec::new();
            let mut seen_textures = HashSet::new();

            for node in scene.nodes.iter() {
                let Some(mesh) = node.mesh.as_ref() else {
                    continue;
                };
                let materials = if node.materials.is_empty() {
                    &mesh.materials
                } else {
                    &node.materials
                };
                for part in mesh.material_parts.iter() {
                    if part.num_triangles == 0 {
                        continue;
                    }
                    let Some(material) = materials.get(part.index as usize) else {
                        continue;
                    };
                    let textures =
                        material_textures(material).map_err(|message| AssetError::Decode {
                            path: path.to_owned(),
                            message: message.to_owned(),
                        })?;
                    for texture in textures.all().into_iter().flatten() {
                        if !seen_textures.insert(texture.element.element_id)
                            || !texture.content.is_empty()
                        {
                            continue;
                        }
                        let dependency = resolve_external_texture(
                            &virtual_path,
                            &path,
                            texture,
                            self,
                            |asset_path, _| Ok(asset_path.to_owned()),
                        )?;
                        if !dependencies.contains(&dependency) {
                            dependencies.push(dependency);
                        }
                    }
                }
            }

            Ok(dependencies)
        }
        #[cfg(not(feature = "fbx"))]
        {
            Err(unsupported_fbx(&path))
        }
    }

    /// Проверяет структуру артефакта готовой модели без загрузки внешних текстур.
    pub fn validate_prebuilt_model(
        &self,
        asset_path: &str,
        bytes: &[u8],
    ) -> Result<(), AssetError> {
        let path = self.resolve(asset_path)?;
        require_extension(&path, "fbx")?;
        decode_prebuilt(bytes, &path).map(|_| ())
    }

    fn read_asset(&self, asset_path: &str, path: &Path) -> Result<Vec<u8>, AssetError> {
        let Some(resources) = &self.resources else {
            return read_legacy(&self.root, path);
        };
        let request = resources
            .read_all(asset_path)
            .map_err(|error| map_resource_error(path, error))?;
        request
            .wait()
            .map_err(|error| map_resource_error(path, error))
    }

    fn read_model_asset(&self, asset_path: &str, path: &Path) -> Result<Vec<u8>, AssetError> {
        let stat = self.stat_asset(asset_path, path)?;
        if stat.size > PREBUILT_MODEL_MAX_BYTES as u64 {
            let prefix = self.read_asset_prefix(
                asset_path,
                path,
                stat.size.min(prebuilt::MAGIC.len() as u64),
            )?;
            ensure_model_size_if_prebuilt(stat.size, &prefix, path, PREBUILT_MODEL_MAX_BYTES)?;
        }
        let bytes = self.read_asset(asset_path, path)?;
        ensure_model_data_size(&bytes, path, PREBUILT_MODEL_MAX_BYTES)?;
        Ok(bytes)
    }

    fn read_asset_prefix(
        &self,
        asset_path: &str,
        path: &Path,
        length: u64,
    ) -> Result<Vec<u8>, AssetError> {
        if let Some(resources) = &self.resources {
            let request = resources
                .read_range(asset_path, 0..length)
                .map_err(|error| map_resource_error(path, error))?;
            return request
                .wait()
                .map_err(|error| map_resource_error(path, error));
        }
        let path = legacy_path(&self.root, path)?;
        let file = fs::File::open(&path).map_err(|error| AssetError::Io {
            path: path.clone(),
            error,
        })?;
        let mut prefix = Vec::new();
        file.take(length)
            .read_to_end(&mut prefix)
            .map_err(|error| AssetError::Io { path, error })?;
        Ok(prefix)
    }

    fn stat_asset(&self, asset_path: &str, path: &Path) -> Result<ResourceStat, AssetError> {
        let Some(resources) = &self.resources else {
            let path = legacy_path(&self.root, path)?;
            let metadata = fs::metadata(&path).map_err(|error| AssetError::Io {
                path: path.clone(),
                error,
            })?;
            if !metadata.is_file() {
                return Err(AssetError::Io {
                    path,
                    error: io::Error::new(io::ErrorKind::InvalidInput, "not a regular file"),
                });
            }
            return Ok(ResourceStat {
                size: metadata.len(),
            });
        };
        let request = resources
            .stat(asset_path)
            .map_err(|error| map_resource_error(path, error))?;
        request
            .wait()
            .map_err(|error| map_resource_error(path, error))
    }
}

fn ensure_model_size(size: u64, path: &Path, limit: usize) -> Result<(), AssetError> {
    if size > limit as u64 {
        return Err(AssetError::Decode {
            path: path.to_owned(),
            message: format!(
                "готовая модель превышает предел размера {} MiB",
                limit / (1024 * 1024)
            ),
        });
    }
    Ok(())
}

fn ensure_model_size_if_prebuilt(
    size: u64,
    prefix: &[u8],
    path: &Path,
    limit: usize,
) -> Result<(), AssetError> {
    if prebuilt::is_prebuilt_model(prefix) {
        ensure_model_size(size, path, limit)?;
    }
    Ok(())
}

fn ensure_model_data_size(bytes: &[u8], path: &Path, limit: usize) -> Result<(), AssetError> {
    if prebuilt::is_prebuilt_model(bytes) {
        ensure_model_size(bytes.len() as u64, path, limit)?;
    }
    Ok(())
}

fn require_extension(path: &Path, expected: &'static str) -> Result<(), AssetError> {
    let matches = path
        .extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| extension.eq_ignore_ascii_case(expected));
    if matches {
        Ok(())
    } else {
        Err(AssetError::UnsupportedFormat {
            path: path.to_owned(),
            expected,
        })
    }
}

fn virtual_asset_path(asset_path: &str) -> String {
    format!("/{}", asset_path.strip_prefix('/').unwrap_or(asset_path))
}

fn map_resource_error(path: &Path, error: ResourceError) -> AssetError {
    if matches!(
        error.kind(),
        ResourceErrorKind::InvalidPath | ResourceErrorKind::PathOutsideRoot
    ) {
        return AssetError::InvalidPath {
            path: path.display().to_string(),
            reason: "путь ресурса не прошёл проверку ресурсного корня",
        };
    }
    let kind = match error.kind() {
        ResourceErrorKind::NotFound => io::ErrorKind::NotFound,
        ResourceErrorKind::NonRegularFile => io::ErrorKind::InvalidInput,
        ResourceErrorKind::Io => match &error {
            ResourceError::Io { source, .. } => source.kind(),
            _ => io::ErrorKind::Other,
        },
        ResourceErrorKind::Unsupported => io::ErrorKind::Unsupported,
        ResourceErrorKind::BudgetExceeded => io::ErrorKind::OutOfMemory,
        ResourceErrorKind::Stopped | ResourceErrorKind::Cancelled => io::ErrorKind::Interrupted,
        ResourceErrorKind::QueueFull => io::ErrorKind::WouldBlock,
        _ => io::ErrorKind::Other,
    };
    // Keeping ResourceError as the inner error preserves its typed kind for callers.
    AssetError::Io {
        path: path.to_owned(),
        error: io::Error::new(kind, error),
    }
}

fn is_missing_texture_candidate(error: &AssetError) -> bool {
    let AssetError::Io { error, .. } = error else {
        return false;
    };
    match error
        .get_ref()
        .and_then(|source| source.downcast_ref::<ResourceError>())
    {
        Some(resource_error) => resource_error.kind() == ResourceErrorKind::NotFound,
        None => error.kind() == io::ErrorKind::NotFound,
    }
}

fn legacy_path(root: &Path, path: &Path) -> Result<PathBuf, AssetError> {
    let canonical_root = fs::canonicalize(root).map_err(|error| AssetError::Io {
        path: root.to_owned(),
        error,
    })?;
    let canonical_path = fs::canonicalize(path).map_err(|error| AssetError::Io {
        path: path.to_owned(),
        error,
    })?;
    if !canonical_path.starts_with(&canonical_root) {
        return Err(AssetError::Io {
            path: path.to_owned(),
            error: io::Error::new(
                io::ErrorKind::PermissionDenied,
                "resource path resolves outside its root",
            ),
        });
    }
    Ok(canonical_path)
}

fn read_legacy(root: &Path, path: &Path) -> Result<Vec<u8>, AssetError> {
    let path = legacy_path(root, path)?;
    let metadata = fs::metadata(&path).map_err(|error| AssetError::Io {
        path: path.clone(),
        error,
    })?;
    if !metadata.is_file() {
        return Err(AssetError::Io {
            path,
            error: io::Error::new(io::ErrorKind::InvalidInput, "not a regular file"),
        });
    }
    fs::read(&path).map_err(|error| AssetError::Io { path, error })
}

/// Декодирует PNG любого цветового типа и глубины в RGBA8.
pub fn decode_png(bytes: &[u8]) -> Result<TextureData, String> {
    let mut decoder = png::Decoder::new_with_limits(
        io::Cursor::new(bytes),
        png::Limits {
            bytes: PNG_DECODE_LIMIT,
        },
    );
    decoder.set_transformations(png::Transformations::normalize_to_color8());
    let mut reader = decoder
        .read_info()
        .map_err(|error| format!("ошибка PNG: {error}"))?;
    let size = reader
        .output_buffer_size()
        .ok_or("PNG слишком велик для памяти")?;
    let mut pixels = vec![0; size];
    let frame = reader
        .next_frame(&mut pixels)
        .map_err(|error| format!("ошибка PNG: {error}"))?;
    pixels.truncate(frame.buffer_size());
    let rgba = match frame.color_type {
        png::ColorType::Rgba => pixels,
        png::ColorType::Rgb => pixels
            .as_chunks::<3>()
            .0
            .iter()
            .flat_map(|&[r, g, b]| [r, g, b, 255])
            .collect(),
        png::ColorType::GrayscaleAlpha => pixels
            .as_chunks::<2>()
            .0
            .iter()
            .flat_map(|&[gray, alpha]| [gray, gray, gray, alpha])
            .collect(),
        png::ColorType::Grayscale => pixels
            .iter()
            .flat_map(|&gray| [gray, gray, gray, 255])
            .collect(),
        // EXPAND переводит палитру в RGB(A), поэтому индексы сюда не доходят.
        png::ColorType::Indexed => return Err("PNG с палитрой не развёрнут в RGB".to_owned()),
    };
    Ok(TextureData {
        width: frame.width,
        height: frame.height,
        rgba,
    })
}

/// Декодирует Radiance RGBE в линейные float RGB. Порядок пикселей в результате канонический
/// (строки сверху вниз, слева направо) для всех восьми ориентаций строки разрешения.
pub fn decode_hdr(bytes: &[u8]) -> Result<HdrImageData, String> {
    if bytes.len() > HDR_DECODE_LIMIT {
        return Err("HDR превышает предел памяти декодера 512 MiB".to_owned());
    }
    let mut cursor = 0;
    let magic = hdr_line(bytes, &mut cursor)?;
    if magic != b"#?RADIANCE" && magic != b"#?RGBE" {
        return Err("ожидается заголовок #?RADIANCE или #?RGBE".to_owned());
    }
    let mut has_format = false;
    loop {
        let line = hdr_line(bytes, &mut cursor)?;
        if line.is_empty() {
            break;
        }
        if line == b"FORMAT=32-bit_rle_rgbe" {
            has_format = true;
        }
    }
    if !has_format {
        return Err("ожидается FORMAT=32-bit_rle_rgbe".to_owned());
    }
    let resolution = hdr_line(bytes, &mut cursor)?;
    let resolution =
        std::str::from_utf8(resolution).map_err(|_| "строка ориентации HDR должна быть ASCII")?;
    let orientation = parse_hdr_orientation(resolution)?;
    let (width, height) = (orientation.width, orientation.height);
    let pixel_count = (width as usize)
        .checked_mul(height as usize)
        .ok_or("размеры HDR переполняют память")?;
    let float_count = pixel_count
        .checked_mul(3)
        .ok_or("размеры HDR переполняют память")?;
    let output_bytes = float_count
        .checked_mul(std::mem::size_of::<f32>())
        .ok_or("размеры HDR переполняют память")?;
    let scanlen = orientation.scanlen();
    let scanline_bytes = scanlen
        .checked_mul(4)
        .ok_or("размер строки HDR переполняет память")?;
    if bytes
        .len()
        .checked_add(output_bytes)
        .and_then(|size| size.checked_add(scanline_bytes))
        .is_none_or(|size| size > HDR_DECODE_LIMIT)
    {
        return Err("HDR превышает предел памяти декодера 512 MiB".to_owned());
    }
    let mut rgb = Vec::new();
    rgb.try_reserve_exact(float_count)
        .map_err(|_| "не удалось выделить память для HDR")?;
    rgb.resize(float_count, 0.0);
    let mut scanline = Vec::new();
    scanline
        .try_reserve_exact(scanline_bytes)
        .map_err(|_| "не удалось выделить память для строки HDR")?;
    scanline.resize(scanline_bytes, 0);

    for scanline_index in 0..orientation.scanline_count() {
        let marker = take_hdr_bytes(bytes, &mut cursor, 4)?;
        let rle_width = ((marker[2] as usize) << 8) | marker[3] as usize;
        if (8..=0x7fff).contains(&scanlen)
            && marker[0] == 2
            && marker[1] == 2
            && rle_width == scanlen
        {
            for channel in 0..4 {
                decode_hdr_channel(bytes, &mut cursor, &mut scanline, scanlen, channel)?;
            }
            for pixel in 0..scanlen {
                let rgbe = [
                    scanline[pixel],
                    scanline[scanlen + pixel],
                    scanline[2 * scanlen + pixel],
                    scanline[3 * scanlen + pixel],
                ];
                let index = orientation.canonical_index(scanline_index, pixel);
                write_hdr_pixel(&mut rgb, index, rgbe);
            }
        } else {
            let row_bytes = scanlen * 4;
            scanline[..4].copy_from_slice(marker);
            let remaining = row_bytes - 4;
            scanline[4..row_bytes].copy_from_slice(take_hdr_bytes(bytes, &mut cursor, remaining)?);
            for pixel in 0..scanlen {
                let offset = pixel * 4;
                let index = orientation.canonical_index(scanline_index, pixel);
                write_hdr_pixel(
                    &mut rgb,
                    index,
                    scanline[offset..offset + 4].try_into().unwrap(),
                );
            }
        }
    }
    Ok(HdrImageData { width, height, rgb })
}

/// Ось строки Radiance: первая ось заголовка задаёт scanline, вторая — пиксели внутри них.
#[derive(Clone, Copy, PartialEq, Eq)]
enum HdrAxis {
    X,
    Y,
}

/// Ориентация Radiance: размеры и направления обхода, одинаковые для flat и RLE.
#[derive(Clone, Copy)]
struct HdrOrientation {
    width: u32,
    height: u32,
    /// `true`, если первая ось заголовка — Y: scanline идут по строкам, иначе по столбцам.
    y_major: bool,
    x_negative: bool,
    y_negative: bool,
}

impl HdrOrientation {
    /// Число пикселей в одной scanline.
    fn scanlen(self) -> usize {
        if self.y_major {
            self.width as usize
        } else {
            self.height as usize
        }
    }

    /// Число scanline в файле.
    fn scanline_count(self) -> usize {
        if self.y_major {
            self.height as usize
        } else {
            self.width as usize
        }
    }

    /// Канонический индекс пикселя по номеру scanline и позиции в ней: строки сверху вниз, столбцы слева направо.
    fn canonical_index(self, scanline: usize, pixel: usize) -> usize {
        let width = self.width as usize;
        let height = self.height as usize;
        let (x, y) = if self.y_major {
            (pixel, scanline)
        } else {
            (scanline, pixel)
        };
        // `-Y` идёт сверху вниз, `+Y` — снизу вверх; `-X` начинает справа, `+X` слева.
        let row = if self.y_negative { y } else { height - 1 - y };
        let column = if self.x_negative { width - 1 - x } else { x };
        row * width + column
    }
}

/// Разбирает `±Y H ±X W` или `±X W ±Y H`: оси должны различаться, число после X — ширина, после Y — высота.
fn parse_hdr_orientation(line: &str) -> Result<HdrOrientation, String> {
    let unsupported = || "ожидается ориентация ±Y H ±X W или ±X W ±Y H с разными осями".to_owned();
    let mut parts = line.split_ascii_whitespace();
    let (Some(first_axis), Some(first_size), Some(second_axis), Some(second_size), None) = (
        parts.next(),
        parts.next(),
        parts.next(),
        parts.next(),
        parts.next(),
    ) else {
        return Err(unsupported());
    };
    let (Some((first_axis, first_negative)), Some((second_axis, second_negative))) =
        (parse_hdr_axis(first_axis), parse_hdr_axis(second_axis))
    else {
        return Err(unsupported());
    };
    if first_axis == second_axis {
        return Err(unsupported());
    }
    let y_major = first_axis == HdrAxis::Y;
    let (x_size, x_negative, y_size, y_negative) = if y_major {
        (second_size, second_negative, first_size, first_negative)
    } else {
        (first_size, first_negative, second_size, second_negative)
    };
    let height = y_size
        .parse::<u32>()
        .map_err(|_| "некорректная высота HDR")?;
    let width = x_size
        .parse::<u32>()
        .map_err(|_| "некорректная ширина HDR")?;
    if width == 0 || height == 0 {
        return Err("размеры HDR должны быть больше нуля".to_owned());
    }
    Ok(HdrOrientation {
        width,
        height,
        y_major,
        x_negative,
        y_negative,
    })
}

/// Разбирает токен оси `+X`, `-X`, `+Y` или `-Y`: возвращает ось и признак минуса.
fn parse_hdr_axis(token: &str) -> Option<(HdrAxis, bool)> {
    match token {
        "+X" => Some((HdrAxis::X, false)),
        "-X" => Some((HdrAxis::X, true)),
        "+Y" => Some((HdrAxis::Y, false)),
        "-Y" => Some((HdrAxis::Y, true)),
        _ => None,
    }
}

fn hdr_line<'a>(bytes: &'a [u8], cursor: &mut usize) -> Result<&'a [u8], String> {
    let tail = bytes
        .get(*cursor..)
        .ok_or_else(|| "неожиданный конец HDR".to_owned())?;
    let length = tail
        .iter()
        .position(|byte| *byte == b'\n')
        .ok_or_else(|| "неожиданный конец строки HDR".to_owned())?;
    let mut line = &tail[..length];
    if line.last() == Some(&b'\r') {
        line = &line[..line.len() - 1];
    }
    *cursor += length + 1;
    Ok(line)
}

fn take_hdr_bytes<'a>(
    bytes: &'a [u8],
    cursor: &mut usize,
    count: usize,
) -> Result<&'a [u8], String> {
    let end = (*cursor)
        .checked_add(count)
        .ok_or_else(|| "смещение HDR переполняется".to_owned())?;
    let result = bytes
        .get(*cursor..end)
        .ok_or_else(|| "данные HDR обрезаны".to_owned())?;
    *cursor = end;
    Ok(result)
}

fn decode_hdr_channel(
    bytes: &[u8],
    cursor: &mut usize,
    scanline: &mut [u8],
    scanlen: usize,
    channel: usize,
) -> Result<(), String> {
    let mut written = 0;
    while written < scanlen {
        let code = take_hdr_bytes(bytes, cursor, 1)?[0] as usize;
        if code == 0 {
            return Err("некорректный нулевой run в HDR".to_owned());
        }
        let count = if code > 128 { code - 128 } else { code };
        if count > scanlen - written {
            return Err("run HDR выходит за длину строки".to_owned());
        }
        let output_start = channel * scanlen + written;
        if code > 128 {
            let value = take_hdr_bytes(bytes, cursor, 1)?[0];
            scanline[output_start..output_start + count].fill(value);
        } else {
            let values = take_hdr_bytes(bytes, cursor, count)?;
            scanline[output_start..output_start + count].copy_from_slice(values);
        }
        written += count;
    }
    Ok(())
}

fn write_hdr_pixel(rgb: &mut [f32], index: usize, rgbe: [u8; 4]) {
    let output = index * 3;
    if rgbe[3] == 0 {
        rgb[output..output + 3].fill(0.0);
        return;
    }
    let scale = 2.0_f32.powi(i32::from(rgbe[3]) - 136);
    rgb[output] = (f32::from(rgbe[0]) + 0.5) * scale;
    rgb[output + 1] = (f32::from(rgbe[1]) + 0.5) * scale;
    rgb[output + 2] = (f32::from(rgbe[2]) + 0.5) * scale;
}

fn check_cancel(cancel: &AtomicBool, path: &Path) -> Result<(), AssetError> {
    if cancel.load(Ordering::Relaxed) {
        Err(AssetError::Cancelled {
            path: path.to_owned(),
        })
    } else {
        Ok(())
    }
}

fn for_each_cancellable<T>(
    items: impl IntoIterator<Item = T>,
    cancel: &AtomicBool,
    path: &Path,
    mut visit: impl FnMut(T) -> Result<(), AssetError>,
) -> Result<(), AssetError> {
    for item in items {
        check_cancel(cancel, path)?;
        visit(item)?;
    }
    Ok(())
}

#[cfg(feature = "fbx")]
fn load_fbx(
    bytes: &[u8],
    virtual_path: &str,
    path: &Path,
    assets: &Assets,
    cancel: &AtomicBool,
) -> Result<Model, AssetError> {
    let scene = load_fbx_scene_cancellable(bytes, virtual_path, path, cancel)?;
    let mut model = Model {
        meshes: Vec::new(),
        textures: Vec::new(),
        skeleton: None,
        animations: Vec::new(),
    };
    let mut texture_cache = ModelTextureCache::default();
    let prepared = {
        let mut material_loader = ModelMaterialLoader {
            model_virtual_path: virtual_path,
            model_path: path,
            assets,
            model: &mut model,
            texture_cache: &mut texture_cache,
            cancel,
        };
        prepare_fbx_parts(&scene, path, cancel, |mesh, references| {
            check_cancel(cancel, path)?;
            let material = material_loader.load_material(&mesh.material, references)?;
            material_loader.model.meshes.push(ModelMesh {
                vertices: mesh.vertices,
                indices: mesh.indices,
                material,
                skin: mesh.skin,
            });
            Ok(())
        })?
    };
    if model.meshes.is_empty() {
        return Err(AssetError::Decode {
            path: path.to_owned(),
            message: "в модели нет треугольников".to_owned(),
        });
    }
    if let Some(builder) = prepared.skeleton_builder {
        model.animations =
            animation::bake_model_animations(&builder, &scene).map_err(|message| {
                AssetError::Decode {
                    path: path.to_owned(),
                    message,
                }
            })?;
        model.skeleton = Some(builder.finish());
    }
    Ok(model)
}

#[cfg(feature = "fbx")]
struct FbxParts {
    texture_references: Vec<prebuilt::TextureReference>,
    skeleton_builder: Option<skin::SkeletonBuilder>,
    has_animation_stacks: bool,
}

#[cfg(feature = "fbx")]
fn prepare_fbx_parts(
    scene: &ufbx::Scene,
    path: &Path,
    cancel: &AtomicBool,
    mut emit: impl FnMut(
        prebuilt::PreparedMesh,
        &[prebuilt::TextureReference],
    ) -> Result<(), AssetError>,
) -> Result<FbxParts, AssetError> {
    let decode_error = |message: String| AssetError::Decode {
        path: path.to_owned(),
        message,
    };
    // Скелет нужен при skin или анимациях с ключами: тогда все части получают связи с костями.
    let needs_skeleton = !scene.skin_deformers.is_empty()
        || animation::has_animated_stack(scene).map_err(decode_error)?;
    let mut builder = needs_skeleton.then(skin::SkeletonBuilder::default);
    let mut texture_sources = prebuilt::TextureSources::default();
    let mut triangle = Vec::new();
    for node in scene.nodes.iter() {
        check_cancel(cancel, path)?;
        let Some(mesh) = node.mesh.as_ref() else {
            continue;
        };
        let normal_matrix = ufbx::matrix_for_normals(&node.geometry_to_world);
        // Материалы экземпляра в узле имеют приоритет над материалами геометрии.
        let materials = if node.materials.is_empty() {
            &mesh.materials
        } else {
            &node.materials
        };
        let mirrored = ufbx::matrix_determinant(&node.geometry_to_world) < 0.0;
        // Связи вершин с костями: rigid — узел самой части, cluster_joints — кости деформера.
        let (rigid, deformer, cluster_joints) = match builder.as_mut() {
            Some(builder) => {
                let rigid = builder.rigid_joint(node);
                match mesh.skin_deformers.first() {
                    Some(deformer) => {
                        let joints = builder.skin_joints(node, deformer);
                        (Some(rigid), Some(&**deformer), joints)
                    }
                    None => (Some(rigid), None, Vec::new()),
                }
            }
            None => (None, None, Vec::new()),
        };
        triangle.resize(mesh.max_face_triangles * 3, 0);
        // Слои tangent и bitangent файла (оба) имеют приоритет над MikkTSpace.
        let file_tangents = mesh.vertex_tangent.exists && mesh.vertex_bitangent.exists;
        for part in mesh.material_parts.iter() {
            check_cancel(cancel, path)?;
            let mut corners = Vec::with_capacity(part.num_triangles * 3);
            let mut influences = Vec::with_capacity(if rigid.is_some() {
                part.num_triangles * 3
            } else {
                0
            });
            for &face_index in part.face_indices.iter() {
                let face = mesh.faces[face_index as usize];
                let triangles = mesh.triangulate_face(&mut triangle, face) as usize;
                for &index in &triangle[..triangles * 3] {
                    let index = index as usize;
                    let position = ufbx::transform_position(
                        &node.geometry_to_world,
                        mesh.vertex_position[index],
                    );
                    let normal =
                        ufbx::transform_direction(&normal_matrix, mesh.vertex_normal[index]);
                    let normal = normalize([normal.x as f32, normal.y as f32, normal.z as f32]);
                    let uv = if mesh.vertex_uv.exists {
                        let uv = mesh.vertex_uv[index];
                        // В FBX ось v направлена вверх от нижнего края текстуры.
                        [uv.x as f32, 1.0 - uv.y as f32]
                    } else {
                        [0.0, 0.0]
                    };
                    let tangent = if file_tangents {
                        let tangent = ufbx::transform_direction(
                            &node.geometry_to_world,
                            mesh.vertex_tangent[index],
                        );
                        let bitangent = ufbx::transform_direction(
                            &node.geometry_to_world,
                            mesh.vertex_bitangent[index],
                        );
                        tangent_from_layers(
                            normal,
                            [tangent.x as f32, tangent.y as f32, tangent.z as f32],
                            [bitangent.x as f32, bitangent.y as f32, bitangent.z as f32],
                        )
                    } else {
                        [0.0; 4]
                    };
                    corners.push(ModelVertex {
                        position: [position.x as f32, position.y as f32, position.z as f32],
                        normal,
                        uv,
                        tangent,
                    });
                    if let Some(rigid) = rigid {
                        // Веса берутся по контрольной точке: углы одной точки получают одинаковые веса.
                        influences.push(match deformer {
                            Some(deformer) => skin::corner_influences(
                                deformer,
                                &cluster_joints,
                                mesh.vertex_indices[index] as usize,
                                rigid,
                            ),
                            None => ([rigid, 0, 0, 0], [1.0, 0.0, 0.0, 0.0]),
                        });
                    }
                }
            }
            if corners.is_empty() {
                continue;
            }
            if !file_tangents && mesh.vertex_uv.exists {
                // Ошибок у bevy_mikktspace 1.0 нет (тип ошибки пуст). Угол без результата
                // остаётся с нулевым tangent.
                let _ = bevy_mikktspace::generate_tangents(&mut MikkGeometry {
                    corners: &mut corners,
                    mirrored,
                });
            }
            let mut builder = MeshPartBuilder::new(corners.len(), rigid.is_some());
            for (index, vertex) in corners.into_iter().enumerate() {
                builder
                    .push(vertex, influences.get(index).copied())
                    .map_err(|message| decode_error(message.to_owned()))?;
            }
            let material = match materials.get(part.index as usize) {
                Some(material) => material_recipe(material, &mut texture_sources)
                    .map_err(|message| decode_error(message.to_owned()))?,
                None => default_material_recipe(),
            };
            let mesh = builder.finish(ModelMaterial::default(), mirrored);
            emit(
                prebuilt::PreparedMesh {
                    vertices: mesh.vertices,
                    indices: mesh.indices,
                    skin: mesh.skin,
                    material,
                },
                &texture_sources.references,
            )?;
        }
    }
    Ok(FbxParts {
        texture_references: texture_sources.references,
        skeleton_builder: builder,
        has_animation_stacks: !scene.anim_stacks.is_empty(),
    })
}

fn decode_prebuilt(bytes: &[u8], path: &Path) -> Result<prebuilt::PreparedModel, AssetError> {
    prebuilt::decode(bytes).map_err(|message| AssetError::Decode {
        path: path.to_owned(),
        message,
    })
}

#[cfg(not(feature = "fbx"))]
fn unsupported_fbx(path: &Path) -> AssetError {
    AssetError::Decode {
        path: path.to_owned(),
        message: "FBX не поддерживается этой сборкой; соберите релиз через rve-build".to_owned(),
    }
}

fn load_prebuilt_model(
    bytes: &[u8],
    path: &Path,
    virtual_path: &str,
    assets: &Assets,
    cancel: &AtomicBool,
) -> Result<Model, AssetError> {
    let prepared = decode_prebuilt(bytes, path)?;
    let Some(meshes) = prepared.meshes else {
        return Err(AssetError::Decode {
            path: path.to_owned(),
            message: "в модели нет треугольников".to_owned(),
        });
    };
    let mut model = Model {
        meshes: Vec::new(),
        textures: Vec::new(),
        skeleton: prepared.skeleton,
        animations: prepared.model_animations,
    };
    let mut texture_cache = ModelTextureCache::default();
    {
        let mut material_loader = ModelMaterialLoader {
            model_virtual_path: virtual_path,
            model_path: path,
            assets,
            model: &mut model,
            texture_cache: &mut texture_cache,
            cancel,
        };
        for_each_cancellable(meshes, cancel, path, |mesh| {
            let material =
                material_loader.load_material(&mesh.material, &prepared.texture_references)?;
            material_loader.model.meshes.push(ModelMesh {
                vertices: mesh.vertices,
                indices: mesh.indices,
                material,
                skin: mesh.skin,
            });
            Ok(())
        })?;
    }
    Ok(model)
}

fn prebuilt_dependencies(
    prepared: &prebuilt::PreparedModel,
    virtual_path: &str,
    path: &Path,
    assets: &Assets,
) -> Result<Vec<String>, AssetError> {
    let Some(meshes) = &prepared.meshes else {
        return Ok(Vec::new());
    };
    let mut dependencies = Vec::new();
    let mut seen_sources = HashSet::new();
    for mesh in meshes {
        for source in mesh.material.sources() {
            if !seen_sources.insert(source) {
                continue;
            }
            let Some(prebuilt::TextureReference::External {
                filename,
                relative_filename,
                absolute_filename,
                ..
            }) = prepared.texture_references.get(source)
            else {
                continue;
            };
            let dependency = resolve_external_texture_source(
                &TextureResolveContext {
                    model_virtual_path: virtual_path,
                    model_path: path,
                    assets,
                },
                ExternalTextureSource {
                    filename,
                    relative_filename,
                    absolute_filename,
                },
                |asset_path, _| Ok(asset_path.to_owned()),
            )?;
            if !dependencies.contains(&dependency) {
                dependencies.push(dependency);
            }
        }
    }
    Ok(dependencies)
}

/// Битовый ключ вершины: позиция, нормаль, UV, tangent, веса и индексы суставов.
#[cfg(feature = "fbx")]
type VertexKey = [u32; 20];

/// Собирает часть меша из углов треугольников, объединяя углы с побитово равными
/// итоговыми атрибутами. Порядок треугольников и вершин в них сохраняется, поэтому
/// швы UV/нормалей и skinning остаются отдельными вершинами.
#[cfg(feature = "fbx")]
struct MeshPartBuilder {
    vertices: Vec<ModelVertex>,
    skin: Option<ModelSkin>,
    indices: Vec<u32>,
    lookup: HashMap<VertexKey, u32>,
}

#[cfg(feature = "fbx")]
impl MeshPartBuilder {
    fn new(corner_capacity: usize, skinned: bool) -> Self {
        Self {
            vertices: Vec::new(),
            skin: skinned.then(|| ModelSkin {
                joints: Vec::new(),
                weights: Vec::new(),
            }),
            indices: Vec::with_capacity(corner_capacity),
            lookup: HashMap::new(),
        }
    }

    /// Добавляет угол треугольника; `influence` задаётся только для частей со skin.
    fn push(
        &mut self,
        vertex: ModelVertex,
        influence: Option<([u32; 4], [f32; 4])>,
    ) -> Result<(), &'static str> {
        let (joints, weights) = influence.unwrap_or_default();
        let mut key = [0; 20];
        let attributes = vertex
            .position
            .iter()
            .chain(&vertex.normal)
            .chain(&vertex.uv)
            .chain(&vertex.tangent)
            .chain(&weights)
            .map(|value| value.to_bits())
            .chain(joints);
        for (slot, bits) in key.iter_mut().zip(attributes) {
            *slot = bits;
        }
        let index = match self.lookup.get(&key) {
            Some(&index) => index,
            None => {
                let index = u32::try_from(self.vertices.len())
                    .map_err(|_| "в меше больше u32::MAX вершин")?;
                self.lookup.insert(key, index);
                self.vertices.push(vertex);
                if let Some(skin) = self.skin.as_mut() {
                    skin.joints.push(joints);
                    skin.weights.push(weights);
                }
                index
            }
        };
        self.indices.push(index);
        Ok(())
    }

    /// Готовая часть; у зеркального узла winding каждого треугольника разворачивается.
    fn finish(mut self, material: ModelMaterial, mirrored: bool) -> ModelMesh {
        if mirrored {
            for triangle in self.indices.as_chunks_mut::<3>().0 {
                triangle.swap(1, 2);
            }
        }
        ModelMesh {
            vertices: self.vertices,
            indices: self.indices,
            material,
            skin: self.skin,
        }
    }
}

/// Углы части для MikkTSpace: грани — треугольники в итоговом winding.
///
/// Зеркальный узел переставляет вершины грани так же, как [`MeshPartBuilder::finish`],
/// поэтому знак `w` соответствует winding, который увидит renderer.
#[cfg(feature = "fbx")]
struct MikkGeometry<'a> {
    corners: &'a mut [ModelVertex],
    mirrored: bool,
}

#[cfg(feature = "fbx")]
impl MikkGeometry<'_> {
    /// Индекс угла части для вершины `vert` грани `face` в итоговом порядке.
    fn corner(&self, face: usize, vert: usize) -> usize {
        let vert = match (self.mirrored, vert) {
            (true, 1) => 2,
            (true, 2) => 1,
            _ => vert,
        };
        face * 3 + vert
    }
}

#[cfg(feature = "fbx")]
impl Geometry for MikkGeometry<'_> {
    fn num_faces(&self) -> usize {
        self.corners.len() / 3
    }

    fn num_vertices_of_face(&self, _face: usize) -> usize {
        3
    }

    fn position(&self, face: usize, vert: usize) -> [f32; 3] {
        self.corners[self.corner(face, vert)].position
    }

    fn normal(&self, face: usize, vert: usize) -> [f32; 3] {
        self.corners[self.corner(face, vert)].normal
    }

    fn tex_coord(&self, face: usize, vert: usize) -> [f32; 2] {
        // Загрузчик хранит uv.y = 1 − v; MikkTSpace должен получить исходный v,
        // иначе знак `w` инвертируется. Возврат через 1 − (1 − v) ошибается не больше ulp.
        let [u, v] = self.corners[self.corner(face, vert)].uv;
        [u, 1.0 - v]
    }

    fn set_tangent(&mut self, tangent: Option<TangentSpace>, face: usize, vert: usize) {
        let index = self.corner(face, vert);
        self.corners[index].tangent = tangent.map_or([0.0; 4], |tangent| tangent.tangent_encoded());
    }
}

/// Tangent из слоёв файла: `tangent` ортогонализуется к нормали, `w` выбирает сторону `bitangent`.
///
/// Нулевой tangent или bitangent, лежащий в плоскости tangent и нормали, даёт `[0, 0, 0, 0]`.
#[cfg(feature = "fbx")]
fn tangent_from_layers(normal: [f32; 3], tangent: [f32; 3], bitangent: [f32; 3]) -> [f32; 4] {
    let dot = |a: [f32; 3], b: [f32; 3]| a[0] * b[0] + a[1] * b[1] + a[2] * b[2];
    let along_normal = dot(tangent, normal);
    let orthogonal: [f32; 3] =
        std::array::from_fn(|axis| tangent[axis] - normal[axis] * along_normal);
    let length = dot(orthogonal, orthogonal).sqrt();
    if !length.is_finite() || length <= 0.0 {
        return [0.0; 4];
    }
    let [x, y, z] = orthogonal.map(|value| value / length);
    // bitangent = cross(normal, tangent) * w: знак w — знак проекции bitangent на это направление.
    let side = [
        normal[1] * z - normal[2] * y,
        normal[2] * x - normal[0] * z,
        normal[0] * y - normal[1] * x,
    ];
    let w = match dot(side, bitangent) {
        handedness if handedness > 0.0 => 1.0,
        handedness if handedness < 0.0 => -1.0,
        _ => return [0.0; 4],
    };
    [x, y, z, w]
}

/// Файловая текстура основного цвета: классический diffuse FBX или base color PBR.
#[cfg(feature = "fbx")]
fn diffuse_texture(material: &ufbx::Material) -> Option<&ufbx::Texture> {
    [&material.fbx.diffuse_color, &material.pbr.base_color]
        .into_iter()
        .filter_map(|map| map.texture.as_deref())
        .find(|texture| texture.type_ == ufbx::TextureType::File)
}

#[cfg(feature = "fbx")]
struct MaterialTextures<'a> {
    base_color: Option<&'a ufbx::Texture>,
    normal: Option<&'a ufbx::Texture>,
    roughness: Option<&'a ufbx::Texture>,
    metalness: Option<&'a ufbx::Texture>,
    emissive: Option<&'a ufbx::Texture>,
    occlusion: Option<&'a ufbx::Texture>,
    opacity: Option<(&'a ufbx::Texture, bool)>,
}

#[derive(Default)]
struct ModelTextureCache {
    loaded_textures: HashMap<usize, usize>,
    loaded_metallic_roughness: HashMap<(Option<usize>, Option<usize>), usize>,
    loaded_base_color_opacity: HashMap<(Option<usize>, usize, bool, u32), usize>,
}

struct ModelMaterialLoader<'a> {
    model_virtual_path: &'a str,
    model_path: &'a Path,
    assets: &'a Assets,
    model: &'a mut Model,
    texture_cache: &'a mut ModelTextureCache,
    cancel: &'a AtomicBool,
}

#[cfg(feature = "fbx")]
impl MaterialTextures<'_> {
    fn all(&self) -> [Option<&ufbx::Texture>; 7] {
        [
            self.base_color,
            self.normal,
            self.roughness,
            self.metalness,
            self.emissive,
            self.occlusion,
            self.opacity.map(|(texture, _)| texture),
        ]
    }
}

#[cfg(feature = "fbx")]
fn material_textures(material: &ufbx::Material) -> Result<MaterialTextures<'_>, &'static str> {
    let pbr = supports_metallic_roughness(material.shader_type);
    let opacity_map = if material.pbr.opacity.texture.is_some() {
        Some((&material.pbr.opacity, false))
    } else if material.fbx.transparency_factor.texture.is_some() {
        Some((&material.fbx.transparency_factor, true))
    } else if material.fbx.transparency_color.texture.is_some() {
        Some((&material.fbx.transparency_color, true))
    } else {
        None
    }
    .map(|(map, invert)| {
        map.texture
            .as_deref()
            .filter(|texture| texture.type_ == ufbx::TextureType::File)
            .map(|texture| (texture, invert))
            .ok_or("opacity texture must be a file texture")
    })
    .transpose()?;
    Ok(MaterialTextures {
        base_color: diffuse_texture(material),
        normal: file_texture(&material.pbr.normal_map)
            .or_else(|| file_texture(&material.fbx.normal_map)),
        roughness: pbr.then(|| file_texture(&material.pbr.roughness)).flatten(),
        metalness: pbr.then(|| file_texture(&material.pbr.metalness)).flatten(),
        emissive: file_texture(&material.pbr.emission_color)
            .or_else(|| file_texture(&material.fbx.emission_color)),
        occlusion: file_texture(&material.pbr.ambient_occlusion),
        opacity: opacity_map,
    })
}

#[cfg(feature = "fbx")]
fn file_texture(map: &ufbx::MaterialMap) -> Option<&ufbx::Texture> {
    map.texture
        .as_deref()
        .filter(|texture| texture.type_ == ufbx::TextureType::File)
}

#[cfg(feature = "fbx")]
fn supports_metallic_roughness(shader_type: ufbx::ShaderType) -> bool {
    matches!(
        shader_type,
        ufbx::ShaderType::OslStandardSurface
            | ufbx::ShaderType::ArnoldStandardSurface
            | ufbx::ShaderType::E3DsMaxPhysicalMaterial
            | ufbx::ShaderType::E3DsMaxPbrMetalRough
            | ufbx::ShaderType::E3DsMaxPbrSpecGloss
            | ufbx::ShaderType::GltfMaterial
            | ufbx::ShaderType::OpenpbrMaterial
            | ufbx::ShaderType::ShaderfxGraph
    )
}

#[cfg(feature = "fbx")]
fn material_recipe(
    material: &ufbx::Material,
    sources: &mut prebuilt::TextureSources,
) -> Result<prebuilt::MaterialRecipe, &'static str> {
    let textures = material_textures(material)?;
    let ids = MaterialTextureIds {
        base_color: textures.base_color.map(|texture| sources.intern(texture)),
        normal: textures.normal.map(|texture| sources.intern(texture)),
        roughness: textures.roughness.map(|texture| sources.intern(texture)),
        metalness: textures.metalness.map(|texture| sources.intern(texture)),
        emissive: textures.emissive.map(|texture| sources.intern(texture)),
        occlusion: textures.occlusion.map(|texture| sources.intern(texture)),
        opacity: textures.opacity.map(|(texture, _)| sources.intern(texture)),
    };
    let has_metallic_roughness = supports_metallic_roughness(material.shader_type);
    let base_color = if ids.base_color.is_some() {
        [1.0; 3]
    } else if material.pbr.base_color.has_value {
        map_vec3(&material.pbr.base_color)
    } else if material.fbx.diffuse_color.has_value {
        let factor = if material.fbx.diffuse_factor.has_value {
            map_scalar(&material.fbx.diffuse_factor)
        } else {
            1.0
        };
        map_vec3(&material.fbx.diffuse_color).map(|value| value * factor)
    } else {
        [1.0; 3]
    };
    let transparency_scale = if let Some((_, invert)) = textures.opacity {
        if !invert {
            1.0
        } else if material.fbx.transparency_factor.texture.is_some() {
            map_scalar(&material.fbx.transparency_color)
        } else {
            map_scalar(&material.fbx.transparency_factor)
        }
    } else {
        1.0
    };
    Ok(prebuilt::MaterialRecipe {
        base_color,
        metallic: if !has_metallic_roughness {
            0.0
        } else if ids.metalness.is_some() {
            1.0
        } else if material.pbr.metalness.has_value {
            map_scalar(&material.pbr.metalness)
        } else {
            0.0
        },
        roughness: if has_metallic_roughness
            && ids.roughness.is_none()
            && material.pbr.roughness.has_value
        {
            map_scalar(&material.pbr.roughness)
        } else {
            1.0
        },
        emissive: emissive_factor(material),
        opacity: opacity_factor(material),
        double_sided: material.features.double_sided.enabled,
        occlusion_strength: if material.pbr.ambient_occlusion.has_value {
            map_scalar(&material.pbr.ambient_occlusion)
        } else {
            1.0
        },
        base_color_source: ids.base_color,
        normal_source: ids.normal,
        roughness_source: ids.roughness,
        metalness_source: ids.metalness,
        emissive_source: ids.emissive,
        occlusion_source: ids.occlusion,
        opacity_source: textures.opacity.map(|(_, invert)| prebuilt::OpacityRecipe {
            source: ids.opacity.expect("opacity texture has a source"),
            invert,
            transparency_scale,
        }),
    })
}

#[cfg(feature = "fbx")]
struct MaterialTextureIds {
    base_color: Option<usize>,
    normal: Option<usize>,
    roughness: Option<usize>,
    metalness: Option<usize>,
    emissive: Option<usize>,
    occlusion: Option<usize>,
    opacity: Option<usize>,
}

#[cfg(feature = "fbx")]
fn default_material_recipe() -> prebuilt::MaterialRecipe {
    prebuilt::MaterialRecipe {
        base_color: [1.0; 3],
        metallic: 0.0,
        roughness: 1.0,
        emissive: [0.0; 3],
        opacity: 1.0,
        double_sided: false,
        occlusion_strength: 1.0,
        base_color_source: None,
        normal_source: None,
        roughness_source: None,
        metalness_source: None,
        emissive_source: None,
        occlusion_source: None,
        opacity_source: None,
    }
}

impl ModelMaterialLoader<'_> {
    fn load_material(
        &mut self,
        recipe: &prebuilt::MaterialRecipe,
        texture_references: &[prebuilt::TextureReference],
    ) -> Result<ModelMaterial, AssetError> {
        let base_color_source = recipe
            .base_color_source
            .map(|source| self.load_texture(source, texture_references))
            .transpose()?;
        let normal_texture = recipe
            .normal_source
            .map(|source| self.load_texture(source, texture_references))
            .transpose()?;
        let metallic_roughness_texture = self.load_metallic_roughness(
            recipe.roughness_source,
            recipe.metalness_source,
            texture_references,
        )?;
        let opacity_texture = recipe
            .opacity_source
            .as_ref()
            .map(|opacity| {
                self.load_texture(opacity.source, texture_references)
                    .map(|index| (index, opacity))
            })
            .transpose()?;
        let base_color_texture = if let Some((opacity_index, opacity)) = opacity_texture {
            let key = (
                recipe.base_color_source,
                opacity.source,
                opacity.invert,
                opacity.transparency_scale.to_bits(),
            );
            if let Some(&index) = self.texture_cache.loaded_base_color_opacity.get(&key) {
                Some(index)
            } else {
                let combined = combine_base_color_opacity(
                    base_color_source.map(|index| &self.model.textures[index]),
                    &self.model.textures[opacity_index],
                    opacity.invert,
                    opacity.transparency_scale,
                )
                .map_err(|message| AssetError::Decode {
                    path: self.model_path.to_owned(),
                    message,
                })?;
                self.model.textures.push(combined);
                let index = self.model.textures.len() - 1;
                self.texture_cache
                    .loaded_base_color_opacity
                    .insert(key, index);
                Some(index)
            }
        } else {
            base_color_source
        };
        let emissive_texture = recipe
            .emissive_source
            .map(|source| self.load_texture(source, texture_references))
            .transpose()?;
        let occlusion_texture = recipe
            .occlusion_source
            .map(|source| self.load_texture(source, texture_references))
            .transpose()?;
        let has_transparent_base_alpha = base_color_texture.is_some_and(|index| {
            self.model.textures[index]
                .rgba
                .as_chunks::<4>()
                .0
                .iter()
                .any(|pixel| pixel[3] < 255)
        });
        Ok(ModelMaterial {
            base_color: recipe.base_color,
            metallic: recipe.metallic,
            roughness: recipe.roughness,
            emissive: recipe.emissive,
            opacity: recipe.opacity,
            alpha_mode: if recipe.opacity < 1.0
                || recipe.opacity_source.is_some()
                || has_transparent_base_alpha
            {
                ModelAlphaMode::Blend
            } else {
                ModelAlphaMode::Opaque
            },
            double_sided: recipe.double_sided,
            base_color_texture,
            metallic_roughness_texture,
            normal_texture,
            emissive_texture,
            occlusion_texture,
            normal_scale: 1.0,
            occlusion_strength: recipe.occlusion_strength,
        })
    }

    fn load_texture(
        &mut self,
        source_id: usize,
        texture_references: &[prebuilt::TextureReference],
    ) -> Result<usize, AssetError> {
        check_cancel(self.cancel, self.model_path)?;
        if let Some(&index) = self.texture_cache.loaded_textures.get(&source_id) {
            return Ok(index);
        }
        let reference = texture_references
            .get(source_id)
            .ok_or_else(|| AssetError::Decode {
                path: self.model_path.to_owned(),
                message: "индекс текстурной ссылки выходит за пределы таблицы".to_owned(),
            })?;
        let texture = load_texture_reference(
            self.model_virtual_path,
            self.model_path,
            reference,
            self.assets,
        )?;
        self.model.textures.push(texture);
        let index = self.model.textures.len() - 1;
        self.texture_cache.loaded_textures.insert(source_id, index);
        Ok(index)
    }

    fn load_metallic_roughness(
        &mut self,
        roughness: Option<usize>,
        metalness: Option<usize>,
        texture_references: &[prebuilt::TextureReference],
    ) -> Result<Option<usize>, AssetError> {
        if roughness.is_none() && metalness.is_none() {
            return Ok(None);
        }
        if let (Some(roughness), Some(metalness)) = (roughness, metalness)
            && roughness == metalness
        {
            return self.load_texture(roughness, texture_references).map(Some);
        }
        let ids = (roughness, metalness);
        if let Some(&index) = self.texture_cache.loaded_metallic_roughness.get(&ids) {
            return Ok(Some(index));
        }
        let roughness_index = roughness
            .map(|source| self.load_texture(source, texture_references))
            .transpose()?;
        let metalness_index = metalness
            .map(|source| self.load_texture(source, texture_references))
            .transpose()?;
        let packed = pack_metallic_roughness(
            roughness_index.map(|index| &self.model.textures[index]),
            metalness_index.map(|index| &self.model.textures[index]),
        )
        .map_err(|message| AssetError::Decode {
            path: self.model_path.to_owned(),
            message,
        })?;
        self.model.textures.push(packed);
        let index = self.model.textures.len() - 1;
        self.texture_cache
            .loaded_metallic_roughness
            .insert(ids, index);
        Ok(Some(index))
    }
}

#[cfg(feature = "fbx")]
fn map_scalar(map: &ufbx::MaterialMap) -> f32 {
    map.value_vec4.x as f32
}

#[cfg(feature = "fbx")]
fn map_vec3(map: &ufbx::MaterialMap) -> [f32; 3] {
    [
        map.value_vec4.x as f32,
        map.value_vec4.y as f32,
        map.value_vec4.z as f32,
    ]
}

#[cfg(feature = "fbx")]
fn emissive_factor(material: &ufbx::Material) -> [f32; 3] {
    let (color, factor) = if material.pbr.emission_color.has_value
        || material.pbr.emission_color.texture.is_some()
        || material.pbr.emission_factor.has_value
    {
        (&material.pbr.emission_color, &material.pbr.emission_factor)
    } else {
        (&material.fbx.emission_color, &material.fbx.emission_factor)
    };
    if !color.has_value && color.texture.is_none() && !factor.has_value {
        return [0.0; 3];
    }
    let color_factor = if color.has_value {
        map_vec3(color)
    } else {
        [1.0; 3]
    };
    let scalar = if factor.has_value {
        map_scalar(factor)
    } else {
        1.0
    };
    color_factor.map(|channel| channel * scalar)
}

#[cfg(feature = "fbx")]
fn opacity_factor(material: &ufbx::Material) -> f32 {
    if material.pbr.opacity.has_value || material.pbr.opacity.texture.is_some() {
        if material.pbr.opacity.has_value {
            map_scalar(&material.pbr.opacity)
        } else {
            1.0
        }
    } else if material.fbx.transparency_factor.texture.is_some()
        || material.fbx.transparency_color.texture.is_some()
    {
        // The combined transparency is baked into base color alpha.
        1.0
    } else {
        1.0 - map_scalar(&material.fbx.transparency_factor)
            * map_scalar(&material.fbx.transparency_color)
    }
}

fn combine_base_color_opacity(
    base_color: Option<&TextureData>,
    opacity: &TextureData,
    invert_opacity: bool,
    transparency_scale: f32,
) -> Result<TextureData, String> {
    if base_color.is_some_and(|base| (base.width, base.height) != (opacity.width, opacity.height)) {
        return Err("base color и opacity карты имеют разные размеры".to_owned());
    }
    let mut rgba =
        base_color.map_or_else(|| vec![255; opacity.rgba.len()], |base| base.rgba.clone());
    for (pixel, opacity_pixel) in rgba
        .as_chunks_mut::<4>()
        .0
        .iter_mut()
        .zip(opacity.rgba.as_chunks::<4>().0.iter())
    {
        let opacity = if invert_opacity {
            (255.0 - f32::from(opacity_pixel[0]) * transparency_scale)
                .round()
                .clamp(0.0, 255.0) as u8
        } else {
            opacity_pixel[0]
        };
        pixel[3] = ((u16::from(pixel[3]) * u16::from(opacity) + 127) / 255) as u8;
    }
    Ok(TextureData {
        width: opacity.width,
        height: opacity.height,
        rgba,
    })
}

fn pack_metallic_roughness(
    roughness: Option<&TextureData>,
    metalness: Option<&TextureData>,
) -> Result<TextureData, String> {
    let source = roughness
        .or(metalness)
        .expect("at least one map is present");
    if roughness
        .is_some_and(|texture| (texture.width, texture.height) != (source.width, source.height))
        || metalness
            .is_some_and(|texture| (texture.width, texture.height) != (source.width, source.height))
    {
        return Err("roughness и metallic карты имеют разные размеры".to_owned());
    }
    let mut rgba = Vec::with_capacity(source.rgba.len());
    let roughness = roughness.map(|texture| texture.rgba.as_slice());
    let metalness = metalness.map(|texture| texture.rgba.as_slice());
    for pixel in 0..(source.width as usize * source.height as usize) {
        rgba.extend_from_slice(&[
            0,
            roughness.map_or(255, |rgba| rgba[pixel * 4]),
            metalness.map_or(255, |rgba| rgba[pixel * 4]),
            255,
        ]);
    }
    Ok(TextureData {
        width: source.width,
        height: source.height,
        rgba,
    })
}

#[cfg(feature = "fbx")]
fn load_fbx_scene(
    bytes: &[u8],
    virtual_path: &str,
    path: &Path,
) -> Result<ufbx::SceneRoot, AssetError> {
    load_fbx_scene_cancellable(bytes, virtual_path, path, &AtomicBool::new(false))
}

#[cfg(feature = "fbx")]
fn load_fbx_scene_cancellable(
    bytes: &[u8],
    virtual_path: &str,
    path: &Path,
    cancel: &AtomicBool,
) -> Result<ufbx::SceneRoot, AssetError> {
    let progress = |_: &ufbx::Progress| {
        if cancel.load(Ordering::Relaxed) {
            ufbx::ProgressResult::Cancel
        } else {
            ufbx::ProgressResult::Continue
        }
    };
    let options = ufbx::LoadOpts {
        progress_cb: ufbx::ProgressCb::Ref(&progress),
        filename: virtual_path.into(),
        path_separator: b'/',
        load_external_files: false,
        open_main_file_with_default: false,
        open_file_cb: Default::default(),
        target_axes: ufbx::CoordinateAxes {
            right: ufbx::CoordinateAxis::PositiveX,
            up: ufbx::CoordinateAxis::PositiveY,
            front: ufbx::CoordinateAxis::PositiveZ,
        },
        target_unit_meters: 1.0,
        space_conversion: ufbx::SpaceConversion::ModifyGeometry,
        generate_missing_normals: true,
        ..Default::default()
    };
    let scene = ufbx::load_memory(bytes, options);
    check_cancel(cancel, path)?;
    scene.map_err(|error| AssetError::Decode {
        path: path.to_owned(),
        message: format!("ошибка FBX: {error:?}"),
    })
}

#[cfg(feature = "fbx")]
fn scene_has_triangles(scene: &ufbx::Scene) -> bool {
    scene.nodes.iter().any(|node| {
        node.mesh.as_ref().is_some_and(|mesh| {
            mesh.material_parts
                .iter()
                .any(|part| part.num_triangles > 0)
        })
    })
}

struct TextureResolveContext<'a> {
    model_virtual_path: &'a str,
    model_path: &'a Path,
    assets: &'a Assets,
}

struct ExternalTextureSource<'a> {
    filename: &'a str,
    relative_filename: &'a str,
    absolute_filename: &'a str,
}

fn load_texture_reference(
    model_virtual_path: &str,
    model: &Path,
    texture: &prebuilt::TextureReference,
    assets: &Assets,
) -> Result<TextureData, AssetError> {
    match texture {
        prebuilt::TextureReference::Embedded { name, png } => {
            decode_png(png).map_err(|message| AssetError::Decode {
                path: model.to_owned(),
                message: format!("встроенная текстура {name:?}: {message}"),
            })
        }
        prebuilt::TextureReference::External {
            filename,
            relative_filename,
            absolute_filename,
            ..
        } => resolve_external_texture_source(
            &TextureResolveContext {
                model_virtual_path,
                model_path: model,
                assets,
            },
            ExternalTextureSource {
                filename,
                relative_filename,
                absolute_filename,
            },
            |asset_path, path| {
                let bytes = assets.read_asset(asset_path, path)?;
                decode_png(&bytes).map_err(|message| AssetError::Decode {
                    path: path.to_owned(),
                    message,
                })
            },
        ),
    }
}

#[cfg(feature = "fbx")]
fn resolve_external_texture<T>(
    model_virtual_path: &str,
    model: &Path,
    texture: &ufbx::Texture,
    assets: &Assets,
    use_candidate: impl FnMut(&str, &Path) -> Result<T, AssetError>,
) -> Result<T, AssetError> {
    resolve_external_texture_source(
        &TextureResolveContext {
            model_virtual_path,
            model_path: model,
            assets,
        },
        ExternalTextureSource {
            filename: texture.filename.as_ref(),
            relative_filename: texture.relative_filename.as_ref(),
            absolute_filename: texture.absolute_filename.as_ref(),
        },
        use_candidate,
    )
}

fn resolve_external_texture_source<T>(
    context: &TextureResolveContext<'_>,
    source: ExternalTextureSource<'_>,
    mut use_candidate: impl FnMut(&str, &Path) -> Result<T, AssetError>,
) -> Result<T, AssetError> {
    let tried = texture_candidates(
        context.model_virtual_path,
        source.filename,
        source.relative_filename,
        source.absolute_filename,
    );
    let tried_paths = tried
        .iter()
        .map(|candidate| context.assets.root.join(candidate))
        .collect::<Vec<_>>();
    for candidate in &tried {
        let asset_path = format!("/{candidate}");
        let path = context.assets.root.join(candidate);
        match context.assets.stat_asset(&asset_path, &path) {
            Err(error) if is_missing_texture_candidate(&error) => continue,
            Err(error) => return Err(error),
            Ok(_) => {}
        }
        require_extension(&path, "png")?;
        match use_candidate(&asset_path, &path) {
            Err(error) if is_missing_texture_candidate(&error) => continue,
            Err(error) => return Err(error),
            Ok(value) => return Ok(value),
        }
    }
    let texture_name = [
        source.relative_filename,
        source.absolute_filename,
        source.filename,
    ]
    .into_iter()
    .find(|candidate| !candidate.is_empty())
    .unwrap_or_default()
    .to_owned();
    Err(AssetError::MissingTexture {
        model: context.model_path.to_owned(),
        texture: texture_name,
        tried: tried_paths,
    })
}

/// Пути поиска внешней текстуры без повторов: путь, который ufbx разрешил
/// относительно модели, затем файлы с тем же именем рядом с моделью.
fn texture_candidates(model: &str, resolved: &str, relative: &str, absolute: &str) -> Vec<String> {
    let model = model.trim_start_matches('/').replace('\\', "/");
    let model_directory = model
        .rsplit_once('/')
        .map_or("", |(directory, _)| directory);
    let mut candidates = Vec::new();
    if !resolved.is_empty() {
        let resolved_path = resolved.replace('\\', "/");
        let rooted = resolved_path.starts_with('/')
            || (!model_directory.is_empty()
                && resolved_path
                    .strip_prefix(model_directory)
                    .is_some_and(|rest| rest.starts_with('/')));
        if let Some(candidate) = normalize_texture_reference(&model, resolved, rooted) {
            candidates.push(candidate);
        }
    }
    for name in [relative, absolute] {
        // В FBX из Windows разделителем может быть `\`, поэтому имя выделяется вручную.
        let normalized = name.replace('\\', "/");
        if !is_host_absolute(&normalized)
            && let Some(candidate) = normalize_texture_reference(&model, &normalized, false)
        {
            candidates.push(candidate);
        }
        if let Some(file_name) = normalized
            .rsplit('/')
            .next()
            .filter(|file_name| !file_name.is_empty())
            && let Some(candidate) = normalize_texture_reference(&model, file_name, false)
        {
            candidates.push(candidate);
        }
    }
    let mut unique = Vec::with_capacity(candidates.len());
    for candidate in candidates {
        if !unique.contains(&candidate) {
            unique.push(candidate);
        }
    }
    unique
}

fn is_host_absolute(path: &str) -> bool {
    path.starts_with('/')
        || (path.as_bytes().get(1) == Some(&b':') && path.as_bytes()[0].is_ascii_alphabetic())
}

fn normalize_texture_reference(model: &str, reference: &str, rooted: bool) -> Option<String> {
    let reference = reference.replace('\\', "/");
    let rooted = rooted || reference.starts_with('/');
    let reference = reference.trim_start_matches('/');
    let mut components = Vec::new();
    if !rooted && let Some((directory, _)) = model.rsplit_once('/') {
        components.extend(directory.split('/'));
    }
    for component in reference.split('/') {
        match component {
            "" => return None,
            "." => {}
            ".." => {
                components.pop()?;
            }
            _ if component.contains([':', '\0']) => return None,
            _ => components.push(component),
        }
    }
    if components.is_empty() {
        return None;
    }
    Some(components.join("/"))
}

#[cfg(feature = "fbx")]
fn normalize([x, y, z]: [f32; 3]) -> [f32; 3] {
    let length = (x * x + y * y + z * z).sqrt();
    if length > 0.0 && length.is_finite() {
        [x / length, y / length, z / length]
    } else {
        // Вырожденный треугольник: любая единичная нормаль лучше NaN в шейдере.
        [0.0, 1.0, 0.0]
    }
}

#[cfg(test)]
mod tests {
    use super::{
        AssetError, Assets, ExternalTextureSource, HdrImageData, Model, ModelMaterial, ModelMesh,
        ModelVertex, TextureData, TextureResolveContext, decode_hdr, decode_png,
        ensure_model_data_size, ensure_model_size_if_prebuilt, for_each_cancellable,
        is_missing_texture_candidate, map_resource_error, pack_metallic_roughness,
        resolve_external_texture_source, texture_candidates,
    };
    #[cfg(feature = "fbx")]
    use super::{
        MeshPartBuilder, ModelAlphaMode, normalize, supports_metallic_roughness,
        tangent_from_layers,
    };
    use engine_files::VirtualPath;
    use engine_pak::{ArchiveId, ArchiveWriter, BlobId, WriterOptions};
    use engine_resources::{ResourceConfig, ResourceError, ResourceErrorKind, ResourceSystem};
    use std::fs;
    use std::io::{self, Cursor};
    use std::path::{Path, PathBuf};

    /// Временный каталог ресурсов, удаляемый в конце теста.
    struct TempAssets {
        root: PathBuf,
    }

    impl TempAssets {
        fn new(name: &str) -> Self {
            let nonce = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let root = std::env::temp_dir()
                .join(format!("rve-assets-{name}-{}-{nonce}", std::process::id()));
            fs::create_dir_all(&root).unwrap();
            Self { root }
        }

        fn write(&self, relative: &str, bytes: &[u8]) {
            let path = self.root.join(relative);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, bytes).unwrap();
        }

        fn assets(&self) -> Assets {
            Assets::new(&self.root)
        }

        fn resource_assets(&self) -> (ResourceSystem, Assets) {
            let system = ResourceSystem::start(ResourceConfig {
                root: self.root.clone(),
                ..ResourceConfig::default()
            })
            .unwrap();
            let assets = Assets::with_resources(system.handle());
            (system, assets)
        }
    }

    impl Drop for TempAssets {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    #[cfg(feature = "fbx")]
    fn convert_in_place(assets: &Assets, asset_path: &str) -> Model {
        let expected = assets.load_model(asset_path).unwrap();
        let ready = assets.convert_model(asset_path).unwrap();
        fs::write(assets.resolve(asset_path).unwrap(), ready).unwrap();
        let actual = assets.load_model(asset_path).unwrap();
        assert_eq!(actual, expected, "{asset_path}");
        actual
    }

    fn write_archive(
        root: &Path,
        name: &str,
        entries: &[(&str, &[u8])],
    ) -> (ArchiveId, Vec<BlobId>) {
        let mut writer = ArchiveWriter::new(
            fs::File::create(root.join(name)).unwrap(),
            WriterOptions::default(),
        )
        .unwrap();
        let mut blobs = Vec::with_capacity(entries.len());
        for (path, bytes) in entries {
            blobs.push(
                writer
                    .add_local(VirtualPath::parse(path).unwrap(), Cursor::new(*bytes), None)
                    .unwrap(),
            );
        }
        let finished = writer.finish().unwrap();
        let id = finished.archive_id;
        drop(finished.into_inner());
        (id, blobs)
    }

    fn resource_error(error: &AssetError) -> Option<&ResourceError> {
        match error {
            AssetError::Io { error, .. } => error
                .get_ref()
                .and_then(|source| source.downcast_ref::<ResourceError>()),
            _ => None,
        }
    }

    fn encode_png(
        width: u32,
        height: u32,
        color: png::ColorType,
        depth: png::BitDepth,
        data: &[u8],
    ) -> Vec<u8> {
        let mut bytes = Vec::new();
        let mut encoder = png::Encoder::new(&mut bytes, width, height);
        encoder.set_color(color);
        encoder.set_depth(depth);
        let mut writer = encoder.write_header().unwrap();
        writer.write_image_data(data).unwrap();
        writer.finish().unwrap();
        bytes
    }

    fn rgba_png() -> Vec<u8> {
        encode_png(
            2,
            1,
            png::ColorType::Rgba,
            png::BitDepth::Eight,
            &[1, 2, 3, 4, 5, 6, 7, 8],
        )
    }

    fn hdr_header(width: u32, height: u32) -> Vec<u8> {
        format!("#?RADIANCE\nFORMAT=32-bit_rle_rgbe\n\n-Y {height} +X {width}\n").into_bytes()
    }

    #[test]
    fn decodes_flat_and_new_style_rle_radiance_hdr() {
        let mut flat = hdr_header(2, 1);
        flat.extend_from_slice(&[128, 64, 32, 129, 0, 0, 0, 0]);
        let decoded = decode_hdr(&flat).unwrap();
        assert_eq!((decoded.width, decoded.height), (2, 1));
        assert_eq!(decoded.rgb.len(), 6);
        assert!((decoded.rgb[0] - 1.003_906_3).abs() < 1e-6);
        assert!((decoded.rgb[1] - 0.50390625).abs() < 1e-6);
        assert!((decoded.rgb[2] - 0.25390625).abs() < 1e-6);
        assert_eq!(&decoded.rgb[3..], &[0.0; 3]);

        let mut rle = hdr_header(8, 1);
        rle.extend_from_slice(&[2, 2, 0, 8]);
        for value in [128, 64, 32, 129] {
            rle.extend_from_slice(&[136, value]);
        }
        let decoded = decode_hdr(&rle).unwrap();
        assert_eq!((decoded.width, decoded.height), (8, 1));
        assert_eq!(decoded.rgb.len(), 24);
        for pixel in decoded.rgb.as_chunks::<3>().0 {
            assert!((pixel[0] - decoded.rgb[0]).abs() < 1e-6);
            assert!((pixel[1] - decoded.rgb[1]).abs() < 1e-6);
            assert!((pixel[2] - decoded.rgb[2]).abs() < 1e-6);
        }
    }

    #[test]
    fn rejects_truncated_invalid_orientation_format_and_budgeted_hdr() {
        let mut truncated = hdr_header(2, 1);
        truncated.extend_from_slice(&[128, 64, 32, 129]);
        assert!(decode_hdr(&truncated).unwrap_err().contains("обрезаны"));

        let mut invalid_run = hdr_header(8, 1);
        invalid_run.extend_from_slice(&[2, 2, 0, 8, 137, 4]);
        assert!(decode_hdr(&invalid_run).unwrap_err().contains("run HDR"));
        let mut zero_run = hdr_header(8, 1);
        zero_run.extend_from_slice(&[2, 2, 0, 8, 0]);
        assert!(decode_hdr(&zero_run).unwrap_err().contains("нулевой run"));

        let wrong_format = b"#?RADIANCE\nFORMAT=32-bit_rgbe\n\n-Y 1 +X 1\n";
        assert!(decode_hdr(wrong_format).unwrap_err().contains("FORMAT"));
        // `+Y 1 +X 1` — допустимая ориентация, данных нет: ошибка обрезанных данных.
        let no_pixels = b"#?RGBE\nFORMAT=32-bit_rle_rgbe\n\n+Y 1 +X 1\n";
        assert!(decode_hdr(no_pixels).unwrap_err().contains("обрезаны"));
        let excessive_resolution = format!(
            "#?RGBE\nFORMAT=32-bit_rle_rgbe\n\n-Y 1 +X 1 {}\n",
            "x ".repeat(100_000)
        );
        assert!(
            decode_hdr(excessive_resolution.as_bytes())
                .unwrap_err()
                .contains("ориентация")
        );
        let oversized = hdr_header(u32::MAX, u32::MAX);
        assert!(decode_hdr(&oversized).unwrap_err().contains("размеры HDR"));
    }

    /// Восемь ориентаций Radiance: первая ось заголовка и вторая; стандартная — `-Y`, `+X`.
    const HDR_ORIENTATIONS: [(&str, &str); 8] = [
        ("-Y", "+X"),
        ("-Y", "-X"),
        ("+Y", "+X"),
        ("+Y", "-X"),
        ("+X", "-Y"),
        ("+X", "+Y"),
        ("-X", "-Y"),
        ("-X", "+Y"),
    ];

    /// Файл Radiance с заданной строкой ориентации и без пикселей.
    fn hdr_file(resolution: &str) -> Vec<u8> {
        format!("#?RADIANCE\nFORMAT=32-bit_rle_rgbe\n\n{resolution}\n").into_bytes()
    }

    /// Строка ориентации для изображения `width` × `height`: размер после X — ширина, после Y — высота.
    fn hdr_resolution(first: &str, second: &str, width: usize, height: usize) -> String {
        let size = |axis: &str| if axis.ends_with('Y') { height } else { width };
        format!("{first} {} {second} {}", size(first), size(second))
    }

    /// Порядок записи пикселей в файл как пары (столбец, строка) канонической раскладки.
    /// Первая ось задаёт порядок scanline, вторая — порядок пикселей внутри них.
    /// `-Y` начинает с верхней строки, `+Y` — с нижней; `+X` — с левого столбца, `-X` — с правого.
    fn hdr_file_order(
        first: &str,
        second: &str,
        width: usize,
        height: usize,
    ) -> Vec<(usize, usize)> {
        let (x_axis, y_axis) = if first.ends_with('X') {
            (first, second)
        } else {
            (second, first)
        };
        let columns: Vec<usize> = if x_axis.starts_with('+') {
            (0..width).collect()
        } else {
            (0..width).rev().collect()
        };
        let rows: Vec<usize> = if y_axis.starts_with('-') {
            (0..height).collect()
        } else {
            (0..height).rev().collect()
        };
        if first.ends_with('Y') {
            rows.iter()
                .flat_map(|&row| columns.iter().map(move |&column| (column, row)))
                .collect()
        } else {
            columns
                .iter()
                .flat_map(|&column| rows.iter().map(move |&row| (column, row)))
                .collect()
        }
    }

    /// Кодирует канонические RGBE-пиксели (строки сверху вниз) в файл с ориентацией `first`/`second`.
    fn encode_hdr(
        first: &str,
        second: &str,
        width: usize,
        height: usize,
        rle: bool,
        canonical: &[[u8; 4]],
    ) -> Vec<u8> {
        let scanlen = if first.ends_with('Y') { width } else { height };
        let mut file = format!(
            "#?RADIANCE\nFORMAT=32-bit_rle_rgbe\n\n{}\n",
            hdr_resolution(first, second, width, height)
        )
        .into_bytes();
        let order = hdr_file_order(first, second, width, height);
        for scanline in order.chunks(scanlen) {
            let pixels: Vec<[u8; 4]> = scanline
                .iter()
                .map(|&(column, row)| canonical[row * width + column])
                .collect();
            if rle {
                file.extend_from_slice(&[2, 2, (scanlen >> 8) as u8, scanlen as u8]);
                for channel in 0..4 {
                    let values: Vec<u8> = pixels.iter().map(|pixel| pixel[channel]).collect();
                    for literal in values.chunks(128) {
                        file.push(literal.len() as u8);
                        file.extend_from_slice(literal);
                    }
                }
            } else {
                file.extend(pixels.iter().flatten());
            }
        }
        file
    }

    /// Ожидаемый результат декодирования: экспонента 136 даёт масштаб 1, поэтому канал равен байту + 0.5.
    fn canonical_hdr(width: u32, height: u32, canonical: &[[u8; 4]]) -> HdrImageData {
        let rgb = canonical
            .iter()
            .flat_map(|pixel| pixel[..3].iter().map(|&value| f32::from(value) + 0.5))
            .collect();
        HdrImageData { width, height, rgb }
    }

    #[test]
    fn decodes_flat_3x2_in_all_eight_orientations_to_canonical_layout() {
        // Явная таблица канонической раскладки 3×2: строка сверху, пиксели слева направо.
        let canonical = [
            [10, 40, 70, 136],
            [11, 41, 71, 136],
            [12, 42, 72, 136],
            [13, 43, 73, 136],
            [14, 44, 74, 136],
            [15, 45, 75, 136],
        ];
        let expected = canonical_hdr(3, 2, &canonical);
        for (first, second) in HDR_ORIENTATIONS {
            let file = encode_hdr(first, second, 3, 2, false, &canonical);
            assert_eq!(
                decode_hdr(&file).unwrap(),
                expected,
                "ориентация {first} {second}"
            );
        }
    }

    #[test]
    fn decodes_rle_9x8_in_all_eight_orientations_to_canonical_layout() {
        // Обе оси длиной не меньше 8, поэтому scanline в любой ориентации кодируется RLE.
        let canonical: Vec<[u8; 4]> = (0..9 * 8)
            .map(|index| [10 + index as u8, 40 + index as u8, 70 + index as u8, 136])
            .collect();
        let expected = canonical_hdr(9, 8, &canonical);
        for (first, second) in HDR_ORIENTATIONS {
            let file = encode_hdr(first, second, 9, 8, true, &canonical);
            assert_eq!(
                decode_hdr(&file).unwrap(),
                expected,
                "ориентация {first} {second}"
            );
        }
    }

    #[test]
    fn rejects_invalid_radiance_resolution_strings() {
        for resolution in [
            "-Y 2 +Y 3",
            "+X 2 -X 3",
            "Y 2 +X 3",
            "-Y 2 X 3",
            "-y 2 +x 3",
            "-XY 2 +X 3",
            "-Y 2 +X",
            "-Y 2 +X 3 1",
            "-Y 2 +X 3 -Y 4",
        ] {
            let error = decode_hdr(&hdr_file(resolution)).unwrap_err();
            assert!(error.contains("ориентация"), "{resolution}: {error}");
        }
        assert!(
            decode_hdr(&hdr_file("-Y 0 +X 3"))
                .unwrap_err()
                .contains("больше нуля")
        );
        assert!(
            decode_hdr(&hdr_file("+X 0 -Y 2"))
                .unwrap_err()
                .contains("больше нуля")
        );
        assert!(
            decode_hdr(&hdr_file("-Y two +X 3"))
                .unwrap_err()
                .contains("высота")
        );
        assert!(
            decode_hdr(&hdr_file("-Y 2 +X -3"))
                .unwrap_err()
                .contains("ширина")
        );
    }

    #[test]
    fn rejects_truncated_x_major_flat_and_rle_data() {
        // `+X 2 +Y 3`: scanline — столбцы по 3 пикселя, нужно 2 × 3 × 4 байта.
        let mut flat = hdr_file("+X 2 +Y 3");
        flat.extend_from_slice(&[1; 16]);
        assert!(decode_hdr(&flat).unwrap_err().contains("обрезаны"));

        // `+X 9 -Y 8`: scanline длиной 8 кодируются RLE, но данных первого канала не хватает.
        let mut rle = hdr_file("+X 9 -Y 8");
        rle.extend_from_slice(&[2, 2, 0, 8, 8, 1, 2, 3]);
        assert!(decode_hdr(&rle).unwrap_err().contains("обрезаны"));
    }

    #[test]
    fn loads_hdr_from_resource_backed_loose_and_packed_sources() {
        let mut fixture = hdr_header(1, 1);
        fixture.extend_from_slice(&[128, 64, 32, 129]);
        let loose = TempAssets::new("hdr-loose");
        loose.write("environment/sky.hdr", &fixture);
        let (_system, assets) = loose.resource_assets();
        let expected = assets.load_hdr("/environment/sky.hdr").unwrap();
        assert_eq!((expected.width, expected.height), (1, 1));

        let packed = TempAssets::new("hdr-packed");
        write_archive(
            &packed.root,
            "assets.rvepak",
            &[("environment/sky.hdr", &fixture)],
        );
        let (_system, assets) = packed.resource_assets();
        assert_eq!(assets.load_hdr("environment/sky.hdr").unwrap(), expected);
        assert!(matches!(
            assets.load_hdr("environment/sky.png"),
            Err(AssetError::UnsupportedFormat { .. })
        ));
    }

    /// Квадрат 2x2 в плоскости XY с нормалью +Z и UV на весь квадрат.
    #[cfg(feature = "fbx")]
    fn quad_fbx(texture: Option<&str>) -> String {
        let mut objects = String::from(
            r#"	Geometry: 1000, "Geometry::Quad", "Mesh" {
		Vertices: *12 {
			a: -1,-1,0,1,-1,0,1,1,0,-1,1,0
		}
		PolygonVertexIndex: *4 {
			a: 0,1,2,-4
		}
		GeometryVersion: 124
		LayerElementNormal: 0 {
			Version: 101
			Name: ""
			MappingInformationType: "ByPolygonVertex"
			ReferenceInformationType: "Direct"
			Normals: *12 {
				a: 0,0,1,0,0,1,0,0,1,0,0,1
			}
		}
		LayerElementUV: 0 {
			Version: 101
			Name: "UVMap"
			MappingInformationType: "ByPolygonVertex"
			ReferenceInformationType: "IndexToDirect"
			UV: *8 {
				a: 0,0,1,0,1,1,0,1
			}
			UVIndex: *4 {
				a: 0,1,2,3
			}
		}
		LayerElementMaterial: 0 {
			Version: 101
			Name: ""
			MappingInformationType: "AllSame"
			ReferenceInformationType: "IndexToDirect"
			Materials: *1 {
				a: 0
			}
		}
		Layer: 0 {
			Version: 100
			LayerElement:  {
				Type: "LayerElementNormal"
				TypedIndex: 0
			}
			LayerElement:  {
				Type: "LayerElementUV"
				TypedIndex: 0
			}
			LayerElement:  {
				Type: "LayerElementMaterial"
				TypedIndex: 0
			}
		}
	}
	Model: 2000, "Model::Quad", "Mesh" {
		Version: 232
		Properties70:  {
			P: "Lcl Translation", "Lcl Translation", "", "A",0,0,0
		}
	}
	Material: 3000, "Material::Paint", "" {
		Version: 102
		ShadingModel: "lambert"
	}
"#,
        );
        let mut connections =
            String::from("\tC: \"OO\",1000,2000\n\tC: \"OO\",2000,0\n\tC: \"OO\",3000,2000\n");
        if let Some(texture) = texture {
            objects.push_str(&format!(
                "\tTexture: 4000, \"Texture::Paint\", \"\" {{\n\t\tType: \"TextureVideoClip\"\n\t\tFileName: \"{texture}\"\n\t\tRelativeFilename: \"{texture}\"\n\t}}\n"
            ));
            connections.push_str("\tC: \"OP\",4000,3000, \"DiffuseColor\"\n");
        }
        format!(
            "; FBX 7.4.0 project file\nFBXHeaderExtension:  {{\n\tFBXHeaderVersion: 1003\n\tFBXVersion: 7400\n}}\nGlobalSettings:  {{\n\tVersion: 1000\n\tProperties70:  {{\n\t\tP: \"UpAxis\", \"int\", \"Integer\", \"\",1\n\t\tP: \"UpAxisSign\", \"int\", \"Integer\", \"\",1\n\t\tP: \"FrontAxis\", \"int\", \"Integer\", \"\",2\n\t\tP: \"FrontAxisSign\", \"int\", \"Integer\", \"\",1\n\t\tP: \"CoordAxis\", \"int\", \"Integer\", \"\",0\n\t\tP: \"CoordAxisSign\", \"int\", \"Integer\", \"\",1\n\t\tP: \"UnitScaleFactor\", \"double\", \"Number\", \"\",1\n\t}}\n}}\nObjects:  {{\n{objects}}}\nConnections:  {{\n{connections}}}\n"
        )
    }

    #[cfg(feature = "fbx")]
    fn arnold_quad_fbx(
        properties: &str,
        texture_connections: &[(&str, &str)],
        duplicate_mesh: bool,
    ) -> String {
        let mut fbx = quad_fbx(None).replace(
            "ShadingModel: \"lambert\"",
            &format!("ShadingModel: \"lambert\"\n\t\tProperties70: {{\n{properties}\n\t\t}}"),
        );
        let mut extra_objects = String::from(
            "\tImplementation: 5000, \"Implementation::ArnoldStandardSurface\", \"Implementation\" {\n\t\tProperties70: {\n\t\t\tP: \"RenderAPI\", \"KString\", \"\", \"U\", \"ARNOLD_SHADER_ID\"\n\t\t}\n\t}\n",
        );
        let mut extra_connections = String::from("\tC: \"OO\",3000,5000\n");
        for (index, &(property, filename)) in texture_connections.iter().enumerate() {
            let id = 4000 + index as u32;
            extra_objects.push_str(&format!(
                "\tTexture: {id}, \"Texture::{property}\", \"\" {{\n\t\tType: \"TextureVideoClip\"\n\t\tFileName: \"{filename}\"\n\t\tRelativeFilename: \"{filename}\"\n\t}}\n"
            ));
            extra_connections.push_str(&format!("\tC: \"OP\",{id},3000,\"{property}\"\n"));
        }
        if duplicate_mesh {
            extra_objects.push_str(
                "\tModel: 2001, \"Model::QuadCopy\", \"Mesh\" {\n\t\tVersion: 232\n\t}\n",
            );
            extra_connections
                .push_str("\tC: \"OO\",1000,2001\n\tC: \"OO\",2001,0\n\tC: \"OO\",3000,2001\n");
        }
        fbx = fbx.replace(
            "\n}\nConnections:  {\n",
            &format!("\n{extra_objects}}}\nConnections:  {{\n"),
        );
        fbx.replace(
            "\tC: \"OO\",3000,2000\n",
            &format!("\tC: \"OO\",3000,2000\n{extra_connections}"),
        )
    }

    #[test]
    fn asset_paths_resolve_inside_the_assets_directory() {
        let assets = Assets::new("assets");
        let expected = Path::new("assets").join("model").join("cube.fbx");
        assert_eq!(assets.resolve("/model/cube.fbx").unwrap(), expected);
        assert_eq!(assets.resolve("model/cube.fbx").unwrap(), expected);
        assert_eq!(
            assets.resolve("/cube.fbx").unwrap(),
            Path::new("assets").join("cube.fbx")
        );
        assert_eq!(assets.root(), Path::new("assets"));
    }

    #[test]
    fn asset_paths_reject_escapes_and_malformed_components() {
        let assets = Assets::new("assets");
        for path in [
            "",
            "/",
            "//model/cube.fbx",
            "/model//cube.fbx",
            "/model/",
            "/../secret.png",
            "/model/../../secret.png",
            "/./cube.fbx",
            "/model\\cube.fbx",
            "C:/model/cube.fbx",
        ] {
            assert!(
                matches!(assets.resolve(path), Err(AssetError::InvalidPath { .. })),
                "{path:?}"
            );
        }
    }

    #[test]
    fn png_of_every_color_type_decodes_to_rgba8() {
        let cases = [
            (
                png::ColorType::Rgba,
                png::BitDepth::Eight,
                vec![10, 20, 30, 40],
                [10, 20, 30, 40],
            ),
            (
                png::ColorType::Rgb,
                png::BitDepth::Eight,
                vec![10, 20, 30],
                [10, 20, 30, 255],
            ),
            (
                png::ColorType::Grayscale,
                png::BitDepth::Eight,
                vec![77],
                [77, 77, 77, 255],
            ),
            (
                png::ColorType::GrayscaleAlpha,
                png::BitDepth::Eight,
                vec![77, 9],
                [77, 77, 77, 9],
            ),
            // 16 бит усекаются до старшего байта.
            (
                png::ColorType::Rgb,
                png::BitDepth::Sixteen,
                vec![1, 2, 3, 4, 5, 6],
                [1, 3, 5, 255],
            ),
        ];
        for (color, depth, data, expected) in cases {
            let texture = decode_png(&encode_png(1, 1, color, depth, &data)).unwrap();
            assert_eq!((texture.width, texture.height), (1, 1), "{color:?}");
            assert_eq!(texture.rgba, expected, "{color:?} {depth:?}");
        }
    }

    #[test]
    fn png_palette_expands_to_rgba8() {
        let mut bytes = Vec::new();
        let mut encoder = png::Encoder::new(&mut bytes, 2, 1);
        encoder.set_color(png::ColorType::Indexed);
        encoder.set_depth(png::BitDepth::Eight);
        encoder.set_palette(vec![255, 0, 0, 0, 0, 255]);
        let mut writer = encoder.write_header().unwrap();
        writer.write_image_data(&[1, 0]).unwrap();
        writer.finish().unwrap();
        assert_eq!(
            decode_png(&bytes).unwrap().rgba,
            [0, 0, 255, 255, 255, 0, 0, 255]
        );
    }

    #[test]
    fn invalid_png_bytes_report_a_decode_error() {
        assert!(decode_png(b"not a png").is_err());
        let truncated = rgba_png();
        assert!(decode_png(&truncated[..truncated.len() / 2]).is_err());
    }

    #[test]
    fn textures_load_from_assets_and_check_format_and_existence() {
        let temp = TempAssets::new("textures");
        temp.write("texture/paint.png", &rgba_png());
        temp.write("texture/paint.jpg", b"jpeg");
        temp.write("texture/broken.png", b"broken");
        let assets = temp.assets();

        let texture = assets.load_texture("/texture/paint.png").unwrap();
        assert_eq!((texture.width, texture.height), (2, 1));
        assert_eq!(texture.rgba, [1, 2, 3, 4, 5, 6, 7, 8]);
        assert!(matches!(
            assets.load_texture("/texture/paint.jpg"),
            Err(AssetError::UnsupportedFormat { .. })
        ));
        assert!(matches!(
            assets.load_texture("/texture/missing.png"),
            Err(AssetError::Io { .. })
        ));
        assert!(matches!(
            assets.load_texture("/texture/broken.png"),
            Err(AssetError::Decode { .. })
        ));
    }

    #[test]
    fn resource_budget_and_stopped_errors_keep_their_typed_source() {
        let temp = TempAssets::new("resource-errors");
        temp.write("texture/paint.png", &rgba_png());
        let budget_system = ResourceSystem::start(ResourceConfig {
            root: temp.root.clone(),
            max_result_bytes: 1,
            ..ResourceConfig::default()
        })
        .unwrap();
        let budget_assets = Assets::with_resources(budget_system.handle());
        let budget_error = budget_assets
            .load_texture("/texture/paint.png")
            .unwrap_err();
        assert_eq!(
            resource_error(&budget_error).map(ResourceError::kind),
            Some(ResourceErrorKind::BudgetExceeded)
        );
        drop(budget_assets);
        drop(budget_system);

        let mut stopped_system = ResourceSystem::start(ResourceConfig {
            root: temp.root.clone(),
            ..ResourceConfig::default()
        })
        .unwrap();
        let stopped_handle = stopped_system.handle();
        stopped_system.shutdown().unwrap();
        let stopped_assets = Assets::with_resources(stopped_handle);
        let stopped_error = stopped_assets
            .load_texture("/texture/paint.png")
            .unwrap_err();
        assert_eq!(
            resource_error(&stopped_error).map(ResourceError::kind),
            Some(ResourceErrorKind::Stopped)
        );
    }

    #[cfg(feature = "fbx")]
    #[test]
    fn cancelled_fbx_load_stops_with_a_cancelled_error() {
        let temp = TempAssets::new("cancel");
        temp.write("model/quad.fbx", quad_fbx(None).as_bytes());
        let assets = temp.assets();
        use std::sync::atomic::{AtomicBool, Ordering};
        let cancel = AtomicBool::new(true);
        let error = assets
            .load_model_cancellable("/model/quad.fbx", &cancel)
            .unwrap_err();
        assert!(matches!(error, AssetError::Cancelled { .. }), "{error}");
        assert!(error.to_string().contains("отменена"));

        // Флаг, выставленный во время разбора, тоже даёт отмену, а не ошибку FBX.
        let bytes = quad_fbx(None);
        let Err(error) = super::load_fbx_scene_cancellable(
            bytes.as_bytes(),
            "model/quad.fbx",
            Path::new("quad.fbx"),
            &cancel,
        ) else {
            panic!("отменённый разбор вернул сцену");
        };
        assert!(matches!(error, AssetError::Cancelled { .. }), "{error}");

        cancel.store(false, Ordering::Relaxed);
        assert_eq!(
            assets
                .load_model_cancellable("/model/quad.fbx", &cancel)
                .unwrap()
                .meshes
                .len(),
            1
        );
    }

    #[cfg(feature = "fbx")]
    #[test]
    fn prebuilt_dispatch_reports_decode_errors_and_honors_cancellation() {
        use std::sync::atomic::AtomicBool;

        let temp = TempAssets::new("prebuilt-dispatch-errors");
        temp.write("model/quad.fbx", quad_fbx(None).as_bytes());
        let assets = temp.assets();
        let mut stale = assets.convert_model("/model/quad.fbx").unwrap();
        stale[8..12].copy_from_slice(&0_u32.to_le_bytes());
        temp.write("model/stale.fbx", &stale);

        let model_error = assets.load_model("/model/stale.fbx").unwrap_err();
        assert!(matches!(model_error, AssetError::Decode { .. }));
        assert!(model_error.to_string().contains("формата v0"));
        for error in [
            assets.load_animations("/model/stale.fbx").unwrap_err(),
            assets.model_dependencies("/model/stale.fbx").unwrap_err(),
        ] {
            assert!(matches!(error, AssetError::Decode { .. }), "{error}");
            assert!(error.to_string().contains("формата v0"), "{error}");
        }

        let mut foreign = stale;
        foreign[0] = b'X';
        temp.write("model/foreign.fbx", &foreign);
        for error in [
            assets.load_model("/model/foreign.fbx").unwrap_err(),
            assets.load_animations("/model/foreign.fbx").unwrap_err(),
            assets.model_dependencies("/model/foreign.fbx").unwrap_err(),
        ] {
            assert!(matches!(error, AssetError::Decode { .. }), "{error}");
        }

        let ready = assets.convert_model("/model/quad.fbx").unwrap();
        temp.write("model/ready.fbx", &ready);
        let cancel = AtomicBool::new(true);
        let error = assets
            .load_model_cancellable("/model/ready.fbx", &cancel)
            .unwrap_err();
        assert!(matches!(error, AssetError::Cancelled { .. }), "{error}");
    }

    #[cfg(feature = "fbx")]
    #[test]
    fn fbx_quad_is_triangulated_with_normals_flipped_uv_and_meters() {
        let temp = TempAssets::new("quad");
        temp.write("model/quad.fbx", quad_fbx(None).as_bytes());
        let model = temp.assets().load_model("/model/quad.fbx").unwrap();

        assert_eq!(model.meshes.len(), 1);
        assert!(model.textures.is_empty());
        // Статическая модель не получает скелет, skin и клипы.
        assert!(model.skeleton.is_none());
        assert!(model.animations.is_empty());
        assert!(model.meshes.iter().all(|mesh| mesh.skin.is_none()));
        let mesh = &model.meshes[0];
        assert_eq!(mesh.material.base_color_texture, None);
        assert!(!mesh.material.double_sided);
        // Общие углы двух треугольников квадрата объединены.
        assert_eq!(mesh.indices, [0, 1, 2, 2, 3, 0]);
        assert_eq!(mesh.vertices.len(), 4);
        for vertex in &mesh.vertices {
            // Единица файла — сантиметр, поэтому координата 1 становится 0.01 м.
            for coordinate in vertex.position {
                assert!(
                    coordinate.abs() < 1e-6 || (coordinate.abs() - 0.01).abs() < 1e-6,
                    "{vertex:?}"
                );
            }
            assert_eq!(vertex.normal, [0.0, 0.0, 1.0]);
            // Верхний край квадрата (y > 0) соответствует первой строке текстуры (v = 0).
            assert_eq!(vertex.uv[1] == 0.0, vertex.position[1] > 0.0, "{vertex:?}");
            assert_eq!(vertex.uv[0] == 1.0, vertex.position[0] > 0.0, "{vertex:?}");
        }
        assert_eq!(
            model.bounds(),
            Some(([-0.01, -0.01, 0.0], [0.01, 0.01, 0.0]))
        );
    }

    #[cfg(feature = "fbx")]
    #[test]
    fn fbx_double_sided_material_and_mirrored_node_preserve_triangle_winding() {
        let temp = TempAssets::new("double-sided-mirrored");
        let fbx = quad_fbx(None)
            .replace(
                "P: \"Lcl Translation\", \"Lcl Translation\", \"\", \"A\",0,0,0",
                "P: \"Lcl Translation\", \"Lcl Translation\", \"\", \"A\",0,0,0\n\t\t\tP: \"Lcl Scaling\", \"Lcl Scaling\", \"\", \"A\",-1,1,1",
            )
            .replace(
                "ShadingModel: \"lambert\"",
                "ShadingModel: \"gltf\"\n\t\tProperties70: {\n\t\t\tP: \"3dsMax|ClassIDa\", \"int\", \"Integer\", \"\",943849874\n\t\t\tP: \"3dsMax|ClassIDb\", \"int\", \"Integer\", \"\",1174294043\n\t\t\tP: \"3dsMax|main|DoubleSided\", \"bool\", \"Boolean\", \"\",1\n\t\t}",
            );
        temp.write("model/mirrored.fbx", fbx.as_bytes());

        let model = temp.assets().load_model("/model/mirrored.fbx").unwrap();

        let mesh = &model.meshes[0];
        assert!(mesh.material.double_sided);
        assert_eq!(mesh.indices, [0, 2, 1, 2, 0, 3]);
        for triangle in mesh.indices.as_chunks::<3>().0 {
            let a = mesh.vertices[triangle[0] as usize].position;
            let b = mesh.vertices[triangle[1] as usize].position;
            let c = mesh.vertices[triangle[2] as usize].position;
            let normal = mesh.vertices[triangle[0] as usize].normal;
            let ab = [b[0] - a[0], b[1] - a[1], b[2] - a[2]];
            let ac = [c[0] - a[0], c[1] - a[1], c[2] - a[2]];
            let face = [
                ab[1] * ac[2] - ab[2] * ac[1],
                ab[2] * ac[0] - ab[0] * ac[2],
                ab[0] * ac[1] - ab[1] * ac[0],
            ];
            assert!(face[0] * normal[0] + face[1] * normal[1] + face[2] * normal[2] > 0.0);
        }
    }

    /// Квадрат из двух треугольных полигонов; углы второго получают UV `second_uvs`.
    #[cfg(feature = "fbx")]
    fn two_triangle_fbx(second_uvs: &str) -> String {
        quad_fbx(None)
            .replace("a: 0,1,2,-4", "a: 0,1,-3,0,2,-4")
            .replace(
                "Normals: *12 {\n\t\t\t\ta: 0,0,1,0,0,1,0,0,1,0,0,1",
                "Normals: *18 {\n\t\t\t\ta: 0,0,1,0,0,1,0,0,1,0,0,1,0,0,1,0,0,1",
            )
            .replace(
                "UV: *8 {\n\t\t\t\ta: 0,0,1,0,1,1,0,1",
                &format!("UV: *12 {{\n\t\t\t\ta: 0,0,1,0,1,1,{second_uvs}"),
            )
            .replace(
                "UVIndex: *4 {\n\t\t\t\ta: 0,1,2,3",
                "UVIndex: *6 {\n\t\t\t\ta: 0,1,2,3,4,5",
            )
    }

    #[cfg(feature = "fbx")]
    #[test]
    fn fbx_corners_with_equal_attributes_share_vertices_but_uv_seams_stay_split() {
        let temp = TempAssets::new("uv-seam");
        // Второй треугольник повторяет UV общих углов 0 и 2: шва нет.
        temp.write(
            "model/shared.fbx",
            two_triangle_fbx("0,0,1,1,0,1").as_bytes(),
        );
        // Второй треугольник смещает UV общих углов: шов по диагонали.
        temp.write(
            "model/seam.fbx",
            two_triangle_fbx("0.5,0,1,0.5,0,1").as_bytes(),
        );

        let shared = temp.assets().load_model("/model/shared.fbx").unwrap();
        let mesh = &shared.meshes[0];
        assert_eq!(mesh.vertices.len(), 4);
        assert_eq!(mesh.indices, [0, 1, 2, 0, 2, 3]);

        let seam = temp.assets().load_model("/model/seam.fbx").unwrap();
        let mesh = &seam.meshes[0];
        assert_eq!(mesh.vertices.len(), 6);
        assert_eq!(mesh.indices, [0, 1, 2, 3, 4, 5]);
        assert_eq!(mesh.vertices[0].position, mesh.vertices[3].position);
        assert_ne!(mesh.vertices[0].uv, mesh.vertices[3].uv);
    }

    #[cfg(feature = "fbx")]
    fn corner(position: [f32; 3], normal: [f32; 3], uv: [f32; 2]) -> ModelVertex {
        ModelVertex {
            position,
            normal,
            uv,
            tangent: [0.0; 4],
        }
    }

    /// Углы треугольников куба ±1: 12 треугольников, наружный winding.
    /// `smooth` даёт общую нормаль в каждой вершине, иначе нормаль грани.
    #[cfg(feature = "fbx")]
    fn cube_corners(smooth: bool) -> Vec<ModelVertex> {
        let faces: [([f32; 3], [f32; 3], [f32; 3]); 6] = [
            ([1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]),
            ([-1.0, 0.0, 0.0], [0.0, 0.0, 1.0], [0.0, 1.0, 0.0]),
            ([0.0, 1.0, 0.0], [0.0, 0.0, 1.0], [1.0, 0.0, 0.0]),
            ([0.0, -1.0, 0.0], [1.0, 0.0, 0.0], [0.0, 0.0, 1.0]),
            ([0.0, 0.0, 1.0], [1.0, 0.0, 0.0], [0.0, 1.0, 0.0]),
            ([0.0, 0.0, -1.0], [0.0, 1.0, 0.0], [1.0, 0.0, 0.0]),
        ];
        let mut corners = Vec::new();
        for (normal, u, v) in faces {
            let point = |a: f32, b: f32| -> [f32; 3] {
                std::array::from_fn(|axis| normal[axis] + a * u[axis] + b * v[axis])
            };
            let quad = [
                point(-1.0, -1.0),
                point(1.0, -1.0),
                point(1.0, 1.0),
                point(-1.0, 1.0),
            ];
            for index in [0, 1, 2, 0, 2, 3] {
                let position = quad[index];
                let normal = if smooth { normalize(position) } else { normal };
                corners.push(corner(position, normal, [0.0, 0.0]));
            }
        }
        corners
    }

    #[cfg(feature = "fbx")]
    fn build_part(
        corners: &[ModelVertex],
        influences: Option<&[([u32; 4], [f32; 4])]>,
        mirrored: bool,
    ) -> ModelMesh {
        let mut builder = MeshPartBuilder::new(corners.len(), influences.is_some());
        for (index, &vertex) in corners.iter().enumerate() {
            builder
                .push(vertex, influences.map(|influences| influences[index]))
                .unwrap();
        }
        builder.finish(ModelMaterial::default(), mirrored)
    }

    /// Треугольники части как последовательность углов: то, что увидит rasterizer.
    #[cfg(feature = "fbx")]
    fn expanded_corners(mesh: &ModelMesh) -> Vec<ModelVertex> {
        mesh.indices
            .iter()
            .map(|&index| mesh.vertices[index as usize])
            .collect()
    }

    #[cfg(feature = "fbx")]
    #[test]
    fn mesh_part_builder_shares_cube_corners_and_keeps_the_same_triangles() {
        let smooth = cube_corners(true);
        let mesh = build_part(&smooth, None, false);
        assert_eq!(mesh.vertices.len(), 8);
        assert_eq!(mesh.indices.len(), 36);
        assert_eq!(expanded_corners(&mesh), smooth);
        assert!(mesh.skin.is_none());

        // Нормали граней — шов на каждом ребре: по 4 вершины на грань.
        let flat = cube_corners(false);
        let mesh = build_part(&flat, None, false);
        assert_eq!(mesh.vertices.len(), 24);
        assert_eq!(expanded_corners(&mesh), flat);
    }

    #[cfg(feature = "fbx")]
    #[test]
    fn mesh_part_builder_keeps_uv_seams_and_compares_exact_bits() {
        let a = corner([0.0, 0.0, 0.0], [0.0, 0.0, 1.0], [0.0, 0.0]);
        let b = corner([1.0, 0.0, 0.0], [0.0, 0.0, 1.0], [1.0, 0.0]);
        let c = corner([0.0, 1.0, 0.0], [0.0, 0.0, 1.0], [0.0, 1.0]);
        let seam = ModelVertex {
            uv: [0.5, 0.0],
            ..a
        };
        let negative_zero = ModelVertex {
            position: [-0.0, 0.0, 0.0],
            ..a
        };
        let corners = [a, b, c, seam, b, c, negative_zero, b, c];
        let mesh = build_part(&corners, None, false);
        assert_eq!(mesh.indices, [0, 1, 2, 3, 1, 2, 4, 1, 2]);
        assert_eq!(expanded_corners(&mesh), corners);
    }

    #[cfg(feature = "fbx")]
    #[test]
    fn mesh_part_builder_splits_vertices_with_different_skin_influences() {
        let a = corner([0.0, 0.0, 0.0], [0.0, 0.0, 1.0], [0.0, 0.0]);
        let b = corner([1.0, 0.0, 0.0], [0.0, 0.0, 1.0], [1.0, 0.0]);
        let c = corner([0.0, 1.0, 0.0], [0.0, 0.0, 1.0], [0.0, 1.0]);
        let rigid = ([0, 0, 0, 0], [1.0, 0.0, 0.0, 0.0]);
        let other_joint = ([1, 0, 0, 0], [1.0, 0.0, 0.0, 0.0]);
        let other_weight = ([0, 1, 0, 0], [0.5, 0.5, 0.0, 0.0]);
        let corners = [a, b, c, a, b, c, a, b, c];
        let influences = [
            rigid,
            rigid,
            rigid,
            other_joint,
            rigid,
            rigid,
            other_weight,
            rigid,
            rigid,
        ];
        let mesh = build_part(&corners, Some(&influences), false);
        assert_eq!(mesh.indices, [0, 1, 2, 3, 1, 2, 4, 1, 2]);
        let skin = mesh.skin.as_ref().unwrap();
        assert_eq!(skin.joints.len(), mesh.vertices.len());
        assert_eq!(skin.weights.len(), mesh.vertices.len());
        for (corner, &index) in mesh.indices.iter().enumerate() {
            let index = index as usize;
            assert_eq!(mesh.vertices[index], corners[corner]);
            assert_eq!(
                (skin.joints[index], skin.weights[index]),
                influences[corner]
            );
        }
    }

    #[cfg(feature = "fbx")]
    #[test]
    fn mesh_part_builder_reverses_winding_of_mirrored_parts() {
        let corners = cube_corners(true);
        let mesh = build_part(&corners, None, true);
        assert_eq!(mesh.vertices.len(), 8);
        let expanded = expanded_corners(&mesh);
        for (triangle, source) in expanded
            .as_chunks::<3>()
            .0
            .iter()
            .zip(corners.as_chunks::<3>().0)
        {
            assert_eq!(*triangle, [source[0], source[2], source[1]]);
        }
    }

    #[cfg(feature = "fbx")]
    fn assert_tangent_close(actual: [f32; 4], expected: [f32; 4]) {
        for (a, e) in actual.iter().zip(expected) {
            assert!((a - e).abs() < 1e-5, "{actual:?} != {expected:?}");
        }
    }

    #[cfg(feature = "fbx")]
    #[test]
    fn fbx_quad_uv_tangent_follows_u_with_positive_handedness() {
        let temp = TempAssets::new("tangent-quad");
        temp.write("model/quad.fbx", quad_fbx(None).as_bytes());

        let model = temp.assets().load_model("/model/quad.fbx").unwrap();

        for vertex in &model.meshes[0].vertices {
            assert_tangent_close(vertex.tangent, [1.0, 0.0, 0.0, 1.0]);
        }
    }

    #[cfg(feature = "fbx")]
    #[test]
    fn fbx_mirrored_node_tangent_follows_mirrored_u_and_flips_handedness() {
        let temp = TempAssets::new("tangent-mirrored");
        let fbx = quad_fbx(None).replace(
            "P: \"Lcl Translation\", \"Lcl Translation\", \"\", \"A\",0,0,0",
            "P: \"Lcl Translation\", \"Lcl Translation\", \"\", \"A\",0,0,0\n\t\t\tP: \"Lcl Scaling\", \"Lcl Scaling\", \"\", \"A\",-1,1,1",
        );
        temp.write("model/mirrored.fbx", fbx.as_bytes());

        let model = temp.assets().load_model("/model/mirrored.fbx").unwrap();

        for vertex in &model.meshes[0].vertices {
            assert_tangent_close(vertex.tangent, [-1.0, 0.0, 0.0, -1.0]);
        }
    }

    #[cfg(feature = "fbx")]
    #[test]
    fn fbx_tangent_and_bitangent_layers_take_precedence_over_uv_tangent() {
        let temp = TempAssets::new("tangent-layers");
        // Слой файла повёрнут в плоскости квадрата: MikkTSpace дал бы (1, 0, 0, 1).
        let fbx = quad_fbx(None)
            .replace(
                "\t\tLayerElementMaterial: 0 {",
                "\t\tLayerElementTangent: 0 {\n\t\t\tVersion: 101\n\t\t\tName: \"UVMap\"\n\t\t\tMappingInformationType: \"ByPolygonVertex\"\n\t\t\tReferenceInformationType: \"Direct\"\n\t\t\tTangents: *12 {\n\t\t\t\ta: 0.6,0.8,0,0.6,0.8,0,0.6,0.8,0,0.6,0.8,0\n\t\t\t}\n\t\t}\n\t\tLayerElementBinormal: 0 {\n\t\t\tVersion: 101\n\t\t\tName: \"UVMap\"\n\t\t\tMappingInformationType: \"ByPolygonVertex\"\n\t\t\tReferenceInformationType: \"Direct\"\n\t\t\tBinormals: *12 {\n\t\t\t\ta: -0.8,0.6,0,-0.8,0.6,0,-0.8,0.6,0,-0.8,0.6,0\n\t\t\t}\n\t\t}\n\t\tLayerElementMaterial: 0 {",
            )
            .replace(
                "LayerElement:  {\n\t\t\t\tType: \"LayerElementMaterial\"",
                "LayerElement:  {\n\t\t\t\tType: \"LayerElementTangent\"\n\t\t\t\tTypedIndex: 0\n\t\t\t}\n\t\t\tLayerElement:  {\n\t\t\t\tType: \"LayerElementBinormal\"\n\t\t\t\tTypedIndex: 0\n\t\t\t}\n\t\t\tLayerElement:  {\n\t\t\t\tType: \"LayerElementMaterial\"",
            );
        temp.write("model/layers.fbx", fbx.as_bytes());

        let model = temp.assets().load_model("/model/layers.fbx").unwrap();

        for vertex in &model.meshes[0].vertices {
            assert_tangent_close(vertex.tangent, [0.6, 0.8, 0.0, 1.0]);
        }
    }

    #[cfg(feature = "fbx")]
    #[test]
    fn fbx_without_uv_layer_has_zero_tangent() {
        let temp = TempAssets::new("tangent-no-uv");
        let mut fbx = quad_fbx(None);
        let uv_block = fbx.find("\t\tLayerElementUV: 0 {").unwrap();
        let material_block = fbx.find("\t\tLayerElementMaterial: 0 {").unwrap();
        fbx.replace_range(uv_block..material_block, "");
        let uv_entry = fbx
            .find("\t\t\tLayerElement:  {\n\t\t\t\tType: \"LayerElementUV\"")
            .unwrap();
        let uv_entry_end = uv_entry + fbx[uv_entry..].find("}\n").unwrap() + 2;
        fbx.replace_range(uv_entry..uv_entry_end, "");
        temp.write("model/no-uv.fbx", fbx.as_bytes());

        let model = temp.assets().load_model("/model/no-uv.fbx").unwrap();

        for vertex in &model.meshes[0].vertices {
            assert_eq!(vertex.uv, [0.0, 0.0]);
            assert_eq!(vertex.tangent, [0.0; 4]);
        }
    }

    #[cfg(feature = "fbx")]
    #[test]
    fn fbx_degenerate_uv_gives_zero_tangent_without_failing_the_load() {
        let temp = TempAssets::new("tangent-degenerate-uv");
        let fbx = quad_fbx(None).replace(
            "UV: *8 {\n\t\t\t\ta: 0,0,1,0,1,1,0,1",
            "UV: *8 {\n\t\t\t\ta: 0,0,0,0,0,0,0,0",
        );
        temp.write("model/flat-uv.fbx", fbx.as_bytes());

        let model = temp.assets().load_model("/model/flat-uv.fbx").unwrap();

        let mesh = &model.meshes[0];
        assert_eq!(mesh.indices.len(), 6);
        for vertex in &mesh.vertices {
            assert_eq!(vertex.tangent, [0.0; 4]);
        }
    }

    #[cfg(feature = "fbx")]
    #[test]
    fn mesh_part_builder_keeps_tangent_seams_apart() {
        let base = corner([0.0, 0.0, 0.0], [0.0, 0.0, 1.0], [0.0, 0.0]);
        let positive = ModelVertex {
            tangent: [1.0, 0.0, 0.0, 1.0],
            ..base
        };
        let mirrored = ModelVertex {
            tangent: [1.0, 0.0, 0.0, -1.0],
            ..base
        };
        let corners = [positive, mirrored, positive, mirrored, positive, positive];
        let mesh = build_part(&corners, None, false);
        assert_eq!(mesh.vertices.len(), 2);
        assert_eq!(mesh.indices, [0, 1, 0, 1, 0, 0]);
        assert_eq!(expanded_corners(&mesh), corners);
    }

    #[cfg(feature = "fbx")]
    #[test]
    fn tangent_from_layers_orthogonalizes_and_takes_sign_from_bitangent() {
        assert_tangent_close(
            tangent_from_layers([0.0, 0.0, 1.0], [1.0, 0.0, 1.0], [0.0, 1.0, 0.0]),
            [1.0, 0.0, 0.0, 1.0],
        );
        assert_tangent_close(
            tangent_from_layers([0.0, 0.0, 1.0], [2.0, 0.0, 0.0], [0.0, -3.0, 0.0]),
            [1.0, 0.0, 0.0, -1.0],
        );
    }

    #[cfg(feature = "fbx")]
    #[test]
    fn tangent_from_layers_degenerate_bases_have_zero_tangent() {
        // Tangent вдоль нормали, нулевой bitangent и bitangent вдоль tangent не задают базис.
        assert_eq!(
            tangent_from_layers([0.0, 0.0, 1.0], [0.0, 0.0, 2.0], [0.0, 1.0, 0.0]),
            [0.0; 4]
        );
        assert_eq!(
            tangent_from_layers([0.0, 0.0, 1.0], [1.0, 0.0, 0.0], [0.0, 0.0, 0.0]),
            [0.0; 4]
        );
        assert_eq!(
            tangent_from_layers([0.0, 0.0, 1.0], [1.0, 0.0, 0.0], [1.0, 0.0, 0.0]),
            [0.0; 4]
        );
    }

    #[cfg(feature = "fbx")]
    #[test]
    fn fbx_texture_is_loaded_relative_to_the_model_or_by_file_name() {
        let temp = TempAssets::new("textured");
        temp.write(
            "model/relative.fbx",
            quad_fbx(Some("../texture/paint.png")).as_bytes(),
        );
        temp.write(
            "model/absolute.fbx",
            quad_fbx(Some("C:\\\\Users\\\\artist\\\\paint.png")).as_bytes(),
        );
        temp.write("texture/paint.png", &rgba_png());
        temp.write("model/paint.png", &rgba_png());
        let assets = temp.assets();
        let (_system, resource_assets) = temp.resource_assets();

        for model in ["/model/relative.fbx", "/model/absolute.fbx"] {
            let expected = assets.load_model(model).unwrap();
            let actual = resource_assets.load_model(model).unwrap();
            assert_eq!(actual, expected);
            assert_eq!(
                actual.meshes[0].material.base_color_texture,
                Some(0),
                "{actual:?}"
            );
            assert_eq!(actual.textures.len(), 1);
            assert_eq!(actual.textures[0].rgba, [1, 2, 3, 4, 5, 6, 7, 8]);
        }
    }

    #[cfg(feature = "fbx")]
    #[test]
    fn model_dependencies_returns_external_paths_for_legacy_and_resource_sources() {
        let temp = TempAssets::new("model-dependencies");
        temp.write(
            "model/relative.fbx",
            quad_fbx(Some("../texture/paint.png")).as_bytes(),
        );
        temp.write("texture/paint.png", b"not decoded as PNG");
        let assets = temp.assets();
        let dependencies = assets.model_dependencies("/model/relative.fbx").unwrap();
        assert_eq!(dependencies, ["/texture/paint.png"]);

        let (_system, resource_assets) = temp.resource_assets();
        assert_eq!(
            resource_assets
                .model_dependencies("/model/relative.fbx")
                .unwrap(),
            dependencies
        );
    }

    #[cfg(feature = "fbx")]
    #[test]
    fn prebuilt_round_trip_matches_fbx_for_fixtures_and_demo_game() {
        fn collect_fbx(root: &Path, directory: &Path, result: &mut Vec<PathBuf>) {
            let mut entries = fs::read_dir(directory)
                .unwrap()
                .map(|entry| entry.unwrap().path())
                .collect::<Vec<_>>();
            entries.sort();
            for path in entries {
                if path.is_dir() {
                    collect_fbx(root, &path, result);
                } else if path
                    .extension()
                    .is_some_and(|extension| extension.eq_ignore_ascii_case("fbx"))
                {
                    result.push(path.strip_prefix(root).unwrap().to_owned());
                }
            }
        }

        fn assert_same<T: std::fmt::Debug + PartialEq>(
            expected: Result<T, AssetError>,
            actual: Result<T, AssetError>,
            asset_path: &str,
        ) {
            match (expected, actual) {
                (Ok(expected), Ok(actual)) => assert_eq!(actual, expected, "{asset_path}"),
                (Err(expected), Err(actual)) => {
                    assert_eq!(actual.to_string(), expected.to_string(), "{asset_path}")
                }
                (expected, actual) => {
                    panic!("{asset_path}: результат отличается: {expected:?} против {actual:?}")
                }
            }
        }

        let fixture_root = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
        let demo_root =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples/demo-game/Assets");
        let mut cases = Vec::new();
        collect_fbx(&fixture_root, &fixture_root, &mut cases);
        for relative in cases.iter_mut() {
            *relative = PathBuf::from("fixtures").join(&*relative);
        }
        let mut demo_cases = Vec::new();
        collect_fbx(&demo_root, &demo_root, &mut demo_cases);
        for relative in demo_cases {
            cases.push(PathBuf::from("demo").join(relative));
        }

        for relative in cases {
            let (source_root, source_relative) = if relative.starts_with("fixtures") {
                (&fixture_root, relative.strip_prefix("fixtures").unwrap())
            } else {
                (&demo_root, relative.strip_prefix("demo").unwrap())
            };
            let virtual_path = format!("/{}", source_relative.to_string_lossy().replace('\\', "/"));
            let temp = TempAssets::new("prebuilt-round-trip");
            let source_path = source_root.join(source_relative);
            temp.write(
                source_relative.to_str().unwrap(),
                &fs::read(&source_path).unwrap(),
            );
            let original = Assets::new(source_root);
            if let Ok(dependencies) = original.model_dependencies(&virtual_path) {
                for dependency in dependencies {
                    let relative_dependency = dependency.trim_start_matches('/');
                    let dependency_bytes = fs::read(source_root.join(relative_dependency)).unwrap();
                    temp.write(relative_dependency, &dependency_bytes);
                }
            }
            let source = temp.assets();
            let direct_model = source.load_model(&virtual_path);
            let direct_animations = source.load_animations(&virtual_path);
            let direct_dependencies = source.model_dependencies(&virtual_path);
            let ready = source
                .convert_model(&virtual_path)
                .unwrap_or_else(|error| panic!("convert {virtual_path}: {error}"));
            temp.write(source_relative.to_str().unwrap(), &ready);

            let ready_assets = temp.assets();
            assert_same(
                direct_model,
                ready_assets.load_model(&virtual_path),
                &virtual_path,
            );
            assert_same(
                direct_animations,
                ready_assets.load_animations(&virtual_path),
                &virtual_path,
            );
            assert_same(
                direct_dependencies,
                ready_assets.model_dependencies(&virtual_path),
                &virtual_path,
            );

            let (mut resources, resource_assets) = temp.resource_assets();
            assert_same(
                ready_assets.load_model(&virtual_path),
                resource_assets.load_model(&virtual_path),
                &virtual_path,
            );
            assert_same(
                ready_assets.load_animations(&virtual_path),
                resource_assets.load_animations(&virtual_path),
                &virtual_path,
            );
            assert_same(
                ready_assets.model_dependencies(&virtual_path),
                resource_assets.model_dependencies(&virtual_path),
                &virtual_path,
            );
            resources.shutdown().unwrap();
        }
    }

    #[cfg(feature = "fbx")]
    #[test]
    fn prebuilt_golden_artifacts_match_the_current_converter() {
        let assets = Assets::new(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures"));
        let golden_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/golden");
        let update =
            std::env::var_os("RVE_UPDATE_FIXTURES").as_deref() == Some(std::ffi::OsStr::new("1"));
        if update {
            fs::create_dir_all(&golden_dir).unwrap();
        }
        for (asset_path, golden_name) in [
            ("/skin_chain_textured.fbx", "skin_chain_textured.rvemodel"),
            ("/bend_clip.fbx", "bend_clip.rvemodel"),
        ] {
            let bytes = assets.convert_model(asset_path).unwrap();
            let golden_path = golden_dir.join(golden_name);
            if update {
                fs::write(&golden_path, bytes).unwrap();
            } else {
                assert_eq!(
                    fs::read(&golden_path).unwrap(),
                    bytes,
                    "{}",
                    golden_path.display()
                );
            }
        }
    }

    #[cfg(not(feature = "fbx"))]
    #[test]
    fn prebuilt_golden_artifacts_load_without_fbx() {
        let fixture_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
        let golden_dir = fixture_dir.join("golden");
        let temp = TempAssets::new("prebuilt-no-fbx");
        temp.write(
            "skin_chain_textured.fbx",
            &fs::read(golden_dir.join("skin_chain_textured.rvemodel")).unwrap(),
        );
        temp.write(
            "bend_clip.fbx",
            &fs::read(golden_dir.join("bend_clip.rvemodel")).unwrap(),
        );
        temp.write(
            "textures/golden_texture.png",
            &fs::read(fixture_dir.join("golden_texture.png")).unwrap(),
        );
        let assets = temp.assets();

        let model = assets.load_model("/skin_chain_textured.fbx").unwrap();
        assert!(!model.meshes.is_empty());
        assert!(model.skeleton.is_some());
        assert!(model.meshes.iter().any(|mesh| mesh.skin.is_some()));
        assert!(!model.textures.is_empty());
        assert_eq!(
            assets
                .model_dependencies("/skin_chain_textured.fbx")
                .unwrap(),
            ["/textures/golden_texture.png"]
        );

        assert!(matches!(
            assets.load_model("/bend_clip.fbx"),
            Err(AssetError::Decode { message, .. }) if message.contains("нет треугольников")
        ));
        let clips = assets.load_animations("/bend_clip.fbx").unwrap();
        assert_eq!(clips.len(), 1);
        assert_eq!(clips[0].name, "bend_clip");
        assert!(!clips[0].tracks.is_empty());
        assert!(
            assets
                .model_dependencies("/bend_clip.fbx")
                .unwrap()
                .is_empty()
        );
    }

    #[cfg(not(feature = "fbx"))]
    #[test]
    fn fbx_bytes_report_a_clear_error_without_the_feature() {
        let temp = TempAssets::new("no-fbx");
        temp.write("model/unsupported.fbx", b"FBX source bytes");
        let assets = temp.assets();

        for error in [
            assets.load_model("/model/unsupported.fbx").unwrap_err(),
            assets
                .load_animations("/model/unsupported.fbx")
                .unwrap_err(),
            assets
                .model_dependencies("/model/unsupported.fbx")
                .unwrap_err(),
        ] {
            assert!(matches!(error, AssetError::Decode { .. }), "{error}");
            assert!(
                error
                    .to_string()
                    .contains("FBX не поддерживается этой сборкой")
            );
        }
    }

    #[cfg(feature = "fbx")]
    #[test]
    fn first_parts_texture_error_precedes_a_later_material_recipe_error() {
        let mut fbx = fs::read_to_string(
            Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/skin_chain_textured.fbx"),
        )
        .unwrap()
        .replace("textures/golden_texture.png", "missing.png");
        let second_material = "\tMaterial: 601, \"Material::Broken\", \"\" {\n\t\tVersion: 102\n\t\tShadingModel: \"lambert\"\n\t}\n\tLayeredTexture: 701, \"LayeredTexture::BrokenOpacity\", \"\" {\n\t}\n";
        assert_eq!(fbx.matches("\n}\nConnections:  {\n").count(), 1);
        fbx = fbx.replace(
            "\n}\nConnections:  {\n",
            &format!("\n{second_material}}}\nConnections:  {{\n"),
        );
        assert_eq!(fbx.matches("\tC: \"OO\",600,200\n").count(), 1);
        fbx = fbx.replace(
            "\tC: \"OO\",600,200\n",
            "\tC: \"OO\",600,200\n\tC: \"OO\",601,201\n\tC: \"OP\",701,601, \"TransparencyFactor\"\n",
        );
        let temp = TempAssets::new("material-error-order");
        temp.write("skin_chain_textured.fbx", fbx.as_bytes());

        let error = temp
            .assets()
            .load_model("/skin_chain_textured.fbx")
            .unwrap_err();
        assert!(
            matches!(
                error,
                AssetError::MissingTexture { ref texture, .. } if texture == "missing.png"
            ),
            "{error}"
        );
    }

    #[cfg(feature = "fbx")]
    #[test]
    fn model_dependencies_preserves_missing_texture_error_and_returns_empty_without_textures() {
        let temp = TempAssets::new("model-dependencies-missing");
        temp.write("model/missing.fbx", quad_fbx(Some("lost.png")).as_bytes());
        temp.write("model/plain.fbx", quad_fbx(None).as_bytes());
        let video = format!(
            "\tVideo: 5000, \"Video::Paint\", \"Clip\" {{\n\t\tType: \"Clip\"\n\t\tFileName: \"embedded.png\"\n\t\tRelativeFilename: \"embedded.png\"\n\t\tContent: \"{}\"\n\t}}\n",
            base64(&rgba_png())
        );
        let embedded_fbx = quad_fbx(Some("embedded.png"))
            .replace("Objects:  {\n", &format!("Objects:  {{\n{video}"))
            .replace(
                "Connections:  {\n",
                "Connections:  {\n\tC: \"OO\",5000,4000\n",
            );
        temp.write("model/embedded.fbx", embedded_fbx.as_bytes());
        let assets = temp.assets();
        assert!(
            assets
                .model_dependencies("/model/plain.fbx")
                .unwrap()
                .is_empty()
        );
        assert!(
            assets
                .model_dependencies("/model/embedded.fbx")
                .unwrap()
                .is_empty()
        );

        let load_error = assets.load_model("/model/missing.fbx").unwrap_err();
        let dependencies_error = assets.model_dependencies("/model/missing.fbx").unwrap_err();
        match (load_error, dependencies_error) {
            (
                AssetError::MissingTexture {
                    model: load_model,
                    texture: load_texture,
                    tried: load_tried,
                },
                AssetError::MissingTexture {
                    model: dependencies_model,
                    texture: dependencies_texture,
                    tried: dependencies_tried,
                },
            ) => {
                assert_eq!(dependencies_model, load_model);
                assert_eq!(dependencies_texture, load_texture);
                assert_eq!(dependencies_tried, load_tried);
            }
            (load_error, dependency_error) => {
                panic!(
                    "expected matching MissingTexture errors, got {load_error:?} and {dependency_error:?}"
                )
            }
        }
    }

    #[cfg(feature = "fbx")]
    #[test]
    fn legacy_loose_resource_loose_and_packed_paths_return_same_cpu_assets() {
        let model_bytes = quad_fbx(Some("../texture/paint.png")).into_bytes();
        let texture_bytes = rgba_png();
        let loose = TempAssets::new("resource-equivalence-loose");
        loose.write("model/relative.fbx", &model_bytes);
        loose.write("texture/paint.png", &texture_bytes);

        let legacy = loose.assets();
        let expected_model = legacy.load_model("/model/relative.fbx").unwrap();
        let expected_texture = legacy.load_texture("/texture/paint.png").unwrap();
        let (_loose_system, loose_assets) = loose.resource_assets();
        assert_eq!(
            loose_assets.load_model("/model/relative.fbx").unwrap(),
            expected_model
        );
        assert_eq!(
            loose_assets.load_texture("/texture/paint.png").unwrap(),
            expected_texture
        );

        let packed = TempAssets::new("resource-equivalence-packed");
        write_archive(
            &packed.root,
            "assets.rvepak",
            &[
                ("model/relative.fbx", &model_bytes),
                ("texture/paint.png", &texture_bytes),
            ],
        );
        let (_packed_system, packed_assets) = packed.resource_assets();
        assert_eq!(
            packed_assets.load_model("/model/relative.fbx").unwrap(),
            expected_model
        );
        assert_eq!(
            packed_assets.load_texture("/texture/paint.png").unwrap(),
            expected_texture
        );
    }

    #[cfg(feature = "fbx")]
    #[test]
    fn packed_external_texture_reference_reads_its_owner_archive() {
        let model_bytes = quad_fbx(Some("paint.png")).into_bytes();
        let texture_bytes = rgba_png();
        let packed = TempAssets::new("packed-external-owner");
        let (owner_id, owner_blobs) = write_archive(
            &packed.root,
            "a-owner.rvepak",
            &[("texture/paint.png", &texture_bytes)],
        );
        let mut model_writer = ArchiveWriter::new(
            fs::File::create(packed.root.join("b-model.rvepak")).unwrap(),
            WriterOptions::default(),
        )
        .unwrap();
        model_writer
            .add_local(
                VirtualPath::parse("model/quad.fbx").unwrap(),
                Cursor::new(&model_bytes),
                None,
            )
            .unwrap();
        model_writer
            .add_external(
                VirtualPath::parse("model/paint.png").unwrap(),
                owner_id,
                owner_blobs[0],
            )
            .unwrap();
        drop(model_writer.finish().unwrap().into_inner());

        let (_system, assets) = packed.resource_assets();
        let model = assets.load_model("/model/quad.fbx").unwrap();
        assert_eq!(model.meshes[0].material.base_color_texture, Some(0));
        assert_eq!(model.textures[0].rgba, [1, 2, 3, 4, 5, 6, 7, 8]);
    }

    #[cfg(feature = "fbx")]
    #[test]
    fn external_host_texture_path_never_reads_outside_the_assets_root() {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let external = std::env::temp_dir().join(format!(
            "rve-assets-host-texture-{}-{nonce}.png",
            std::process::id(),
        ));
        fs::write(&external, rgba_png()).unwrap();
        let temp = TempAssets::new("host-texture-path");
        temp.write(
            "model/quad.fbx",
            quad_fbx(Some(external.to_str().unwrap())).as_bytes(),
        );
        let (_system, assets) = temp.resource_assets();
        let result = assets.load_model("/model/quad.fbx");
        fs::remove_file(external).unwrap();
        assert!(matches!(result, Err(AssetError::MissingTexture { .. })));
    }

    #[cfg(feature = "fbx")]
    #[test]
    fn external_texture_decode_error_does_not_fall_back_to_the_basename() {
        let temp = TempAssets::new("external-texture-error");
        temp.write(
            "model/quad.fbx",
            quad_fbx(Some("../texture/paint.png")).as_bytes(),
        );
        temp.write("texture/paint.png", b"corrupt png");
        temp.write("model/paint.png", &rgba_png());
        let (_system, assets) = temp.resource_assets();
        match assets.load_model("/model/quad.fbx") {
            Err(AssetError::Decode { path, .. }) => {
                assert_eq!(path, temp.root.join("texture/paint.png"));
            }
            other => panic!("expected first candidate decode failure, got {other:?}"),
        }
    }

    #[test]
    fn fbx_texture_search_skips_only_typed_missing_resources() {
        let path = PathBuf::from("texture/paint.png");
        let missing_virtual_path = map_resource_error(
            &path,
            ResourceError::NotFound("texture/paint.png".to_owned()),
        );
        let missing_backing_file = map_resource_error(
            &path,
            ResourceError::Io {
                path: Some(path.clone()),
                source: io::Error::new(io::ErrorKind::NotFound, "archive backing file vanished"),
            },
        );

        assert!(is_missing_texture_candidate(&missing_virtual_path));
        assert!(!is_missing_texture_candidate(&missing_backing_file));
        assert_eq!(
            resource_error(&missing_backing_file).unwrap().kind(),
            ResourceErrorKind::Io
        );
    }

    #[cfg(feature = "fbx")]
    #[test]
    fn texture_directory_candidate_stops_search_for_legacy_and_service_assets() {
        let temp = TempAssets::new("texture-directory-candidate");
        temp.write(
            "model/quad.fbx",
            quad_fbx(Some("../texture/paint.png")).as_bytes(),
        );
        fs::create_dir_all(temp.root.join("texture/paint.png")).unwrap();
        temp.write("model/paint.png", &rgba_png());
        let legacy_assets = temp.assets();
        let (_system, service_assets) = temp.resource_assets();

        for assets in [&legacy_assets, &service_assets] {
            match assets.load_model("/model/quad.fbx") {
                Err(AssetError::Io { error, .. }) => {
                    assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
                }
                other => panic!("expected the directory candidate to stop search, got {other:?}"),
            }
        }
    }

    #[cfg(feature = "fbx")]
    #[test]
    fn fbx_z_up_axes_are_converted_to_y_up() {
        let temp = TempAssets::new("z-up");
        // Соглашение Blender: Z вверх, «вперёд» вдоль -Y.
        let z_up = quad_fbx(None)
            .replace(
                "\"UpAxis\", \"int\", \"Integer\", \"\",1",
                "\"UpAxis\", \"int\", \"Integer\", \"\",2",
            )
            .replace(
                "\"FrontAxis\", \"int\", \"Integer\", \"\",2",
                "\"FrontAxis\", \"int\", \"Integer\", \"\",1",
            )
            .replace(
                "\"FrontAxisSign\", \"int\", \"Integer\", \"\",1",
                "\"FrontAxisSign\", \"int\", \"Integer\", \"\",-1",
            );
        temp.write("model/z_up.fbx", z_up.as_bytes());
        let model = temp.assets().load_model("/model/z_up.fbx").unwrap();
        for vertex in &model.meshes[0].vertices {
            let [x, y, z] = vertex.normal;
            assert!(
                x.abs() < 1e-6 && (y - 1.0).abs() < 1e-6 && z.abs() < 1e-6,
                "{vertex:?}"
            );
            assert!(vertex.position[1].abs() < 1e-6, "{vertex:?}");
        }
    }

    #[cfg(feature = "fbx")]
    #[test]
    fn fbx_embedded_png_is_decoded_without_external_file() {
        let temp = TempAssets::new("embedded");
        let video = format!(
            "\tVideo: 5000, \"Video::Paint\", \"Clip\" {{\n\t\tType: \"Clip\"\n\t\tFileName: \"embedded.png\"\n\t\tRelativeFilename: \"embedded.png\"\n\t\tContent: \"{}\"\n\t}}\n",
            base64(&rgba_png())
        );
        let fbx = quad_fbx(Some("embedded.png"))
            .replace("Objects:  {\n", &format!("Objects:  {{\n{video}"))
            .replace(
                "Connections:  {\n",
                "Connections:  {\n\tC: \"OO\",5000,4000\n",
            );
        temp.write("model/embedded.fbx", fbx.as_bytes());
        let expected = temp.assets().load_model("/model/embedded.fbx").unwrap();
        let ready = temp.assets().convert_model("/model/embedded.fbx").unwrap();
        temp.write("model/embedded.fbx", &ready);
        let (_system, assets) = temp.resource_assets();
        let model = assets.load_model("/model/embedded.fbx").unwrap();
        assert_eq!(model, expected);
        assert_eq!(model.meshes[0].material.base_color_texture, Some(0));
        assert_eq!(model.textures[0].rgba, [1, 2, 3, 4, 5, 6, 7, 8]);
    }

    #[cfg(feature = "fbx")]
    fn base64(bytes: &[u8]) -> String {
        const ALPHABET: &[u8; 64] =
            b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
        let mut text = String::new();
        for chunk in bytes.chunks(3) {
            let value = chunk
                .iter()
                .enumerate()
                .fold(0_u32, |value, (index, &byte)| {
                    value | u32::from(byte) << (16 - 8 * index)
                });
            for index in 0..4 {
                if index <= chunk.len() {
                    text.push(ALPHABET[(value >> (18 - 6 * index) & 63) as usize] as char);
                } else {
                    text.push('=');
                }
            }
        }
        text
    }

    #[cfg(feature = "fbx")]
    #[test]
    fn fbx_with_missing_texture_lists_searched_paths() {
        let temp = TempAssets::new("missing-texture");
        temp.write("model/quad.fbx", quad_fbx(Some("lost.png")).as_bytes());
        match temp.assets().load_model("/model/quad.fbx") {
            Err(AssetError::MissingTexture { texture, tried, .. }) => {
                assert_eq!(texture, "lost.png");
                assert_eq!(tried, [temp.root.join("model").join("lost.png")]);
            }
            other => panic!("{other:?}"),
        }
    }

    #[cfg(feature = "fbx")]
    #[test]
    fn model_loading_reports_missing_unsupported_and_corrupt_files() {
        let temp = TempAssets::new("bad-models");
        temp.write("model/broken.fbx", b"definitely not fbx");
        temp.write("model/cube.obj", b"v 0 0 0");
        let assets = temp.assets();
        assert!(matches!(
            assets.load_model("/model/missing.fbx"),
            Err(AssetError::Io { .. })
        ));
        assert!(matches!(
            assets.load_model("/model/cube.obj"),
            Err(AssetError::UnsupportedFormat { .. })
        ));
        assert!(matches!(
            assets.load_model("/model/broken.fbx"),
            Err(AssetError::Decode { .. })
        ));
        assert!(matches!(
            assets.load_model("/../model.fbx"),
            Err(AssetError::InvalidPath { .. })
        ));
    }

    #[test]
    fn texture_candidates_deduplicate_and_accept_windows_separators() {
        let model = "/model/cube.fbx";
        assert_eq!(
            texture_candidates(model, "model/paint.png", "paint.png", "C:\\art\\paint.png"),
            ["model/paint.png"]
        );
        assert_eq!(
            texture_candidates(model, "", "../texture/paint.png", ""),
            ["texture/paint.png", "model/paint.png"]
        );
        assert!(texture_candidates(model, "", "", "").is_empty());
    }

    #[cfg(feature = "fbx")]
    #[test]
    fn arnold_pbr_factors_are_read_from_ufbx_material_maps_without_gamma_conversion() {
        let temp = TempAssets::new("pbr-factors");
        let fbx = arnold_quad_fbx(
            "\t\t\tP: \"baseColor\", \"Color\", \"\", \"A\", 0.2, 0.4, 0.8\n\t\t\tP: \"metalness\", \"Number\", \"\", \"A\", 0.65\n\t\t\tP: \"specularRoughness\", \"Number\", \"\", \"A\", 0.35",
            &[],
            false,
        );
        temp.write("model/pbr.fbx", fbx.as_bytes());

        let model = temp.assets().load_model("/model/pbr.fbx").unwrap();
        let material = &model.meshes[0].material;
        assert_eq!(material.base_color, [0.2, 0.4, 0.8]);
        assert_eq!(material.metallic, 0.65);
        assert_eq!(material.roughness, 0.35);
        assert_eq!(material.normal_scale, 1.0);
        assert!(material.base_color_texture.is_none());
        assert!(material.metallic_roughness_texture.is_none());
        assert!(material.normal_texture.is_none());
        assert!(model.textures.is_empty());
    }

    #[cfg(feature = "fbx")]
    #[test]
    fn pbr_emissive_and_opacity_maps_load_and_join_opacity_into_base_color_alpha() {
        let temp = TempAssets::new("pbr-emissive-opacity");
        let fbx = arnold_quad_fbx(
            "\t\t\tP: \"emission\", \"Number\", \"\", \"A\", 2.5\n\t\t\tP: \"emissionColor\", \"Color\", \"\", \"A\", 0.2, 0.4, 0.8\n\t\t\tP: \"opacity\", \"Number\", \"\", \"A\", 0.6",
            &[
                ("emissionColor", "emissive.png"),
                ("opacity", "opacity.png"),
            ],
            false,
        );
        temp.write("model/pbr.fbx", fbx.as_bytes());
        temp.write("model/emissive.png", &rgba_png());
        temp.write(
            "model/opacity.png",
            &encode_png(
                2,
                1,
                png::ColorType::Rgba,
                png::BitDepth::Eight,
                &[64, 0, 0, 255, 128, 0, 0, 255],
            ),
        );

        let assets = temp.assets();
        let model = assets.load_model("/model/pbr.fbx").unwrap();
        let material = &model.meshes[0].material;
        assert_eq!(material.emissive, [0.5, 1.0, 2.0]);
        assert_eq!(material.opacity, 0.6);
        assert_eq!(material.alpha_mode, ModelAlphaMode::Blend);
        let base_color = &model.textures[material.base_color_texture.unwrap()];
        assert_eq!(base_color.rgba, [255, 255, 255, 64, 255, 255, 255, 128]);
        assert_eq!(
            assets.model_dependencies("/model/pbr.fbx").unwrap(),
            ["/model/emissive.png", "/model/opacity.png"]
        );
        let ready = convert_in_place(&assets, "/model/pbr.fbx");
        assert_eq!(ready, model);
    }

    #[cfg(feature = "fbx")]
    #[test]
    fn pbr_occlusion_map_uses_linear_red_channel_and_is_a_builder_dependency() {
        let temp = TempAssets::new("pbr-occlusion");
        let fbx = arnold_quad_fbx(
            "\t\t\tP: \"3dsMax|ClassIDa\", \"int\", \"Integer\", \"\", -804315648\n\t\t\tP: \"3dsMax|ClassIDb\", \"int\", \"Integer\", \"\", -1099438848\n\t\t\tP: \"3dsMax|main|ao\", \"Number\", \"\", \"A\", 0.35",
            &[("3dsMax|main|ao", "ao.png")],
            false,
        )
        .replace("\tC: \"OO\",3000,5000\n", "")
        .replace("ShadingModel: \"lambert\"", "ShadingModel: \"unknown\"");
        temp.write("model/pbr.fbx", fbx.as_bytes());
        temp.write("model/ao.png", &rgba_png());

        let assets = temp.assets();
        let model = assets.load_model("/model/pbr.fbx").unwrap();
        let material = &model.meshes[0].material;
        assert_eq!(material.occlusion_strength, 0.35);
        assert_eq!(material.occlusion_texture, Some(0));
        assert_eq!(
            assets.model_dependencies("/model/pbr.fbx").unwrap(),
            ["/model/ao.png"]
        );
    }

    #[cfg(feature = "fbx")]
    #[test]
    fn opacity_maps_with_incompatible_dimensions_fail_instead_of_being_ignored() {
        let temp = TempAssets::new("pbr-opacity-size-mismatch");
        let fbx = arnold_quad_fbx(
            "",
            &[("baseColor", "base.png"), ("opacity", "opacity.png")],
            false,
        );
        temp.write("model/pbr.fbx", fbx.as_bytes());
        temp.write("model/base.png", &rgba_png());
        temp.write(
            "model/opacity.png",
            &encode_png(
                1,
                1,
                png::ColorType::Rgba,
                png::BitDepth::Eight,
                &[128, 128, 128, 255],
            ),
        );

        let error = temp.assets().load_model("/model/pbr.fbx").unwrap_err();
        assert!(matches!(error, AssetError::Decode { .. }));
        assert!(error.to_string().contains("разные размеры"));
    }

    #[cfg(feature = "fbx")]
    #[test]
    fn pbr_base_and_normal_maps_load_with_white_base_factor_and_default_normal_scale() {
        let temp = TempAssets::new("pbr-base-normal-maps");
        let fbx = arnold_quad_fbx(
            "",
            &[("baseColor", "base.png"), ("normalCamera", "normal.png")],
            false,
        );
        temp.write("model/pbr.fbx", fbx.as_bytes());
        temp.write("model/base.png", &rgba_png());
        temp.write(
            "model/normal.png",
            &encode_png(
                1,
                1,
                png::ColorType::Rgba,
                png::BitDepth::Eight,
                &[128, 128, 255, 255],
            ),
        );

        let model = temp.assets().load_model("/model/pbr.fbx").unwrap();
        let material = &model.meshes[0].material;
        assert_eq!(material.base_color, [1.0; 3]);
        assert_eq!(material.base_color_texture, Some(0));
        assert_eq!(material.normal_texture, Some(1));
        assert_eq!(material.normal_scale, 1.0);
        assert_eq!(model.textures.len(), 2);
    }

    #[cfg(feature = "fbx")]
    #[test]
    fn classic_transparency_multiplies_color_and_factor_before_inverting() {
        let temp = TempAssets::new("classic-transparency");
        for (color, factor, expected) in [(0.0, 1.0, 1.0), (0.5, 0.5, 0.75), (1.0, 0.0, 1.0)] {
            let properties = format!(
                "\t\t\tP: \"TransparentColor\", \"Color\", \"\", \"A\", {color}, {color}, {color}\n\t\t\tP: \"TransparencyFactor\", \"Number\", \"\", \"A\", {factor}"
            );
            let fbx =
                arnold_quad_fbx(&properties, &[], false).replace("\tC: \"OO\",3000,5000\n", "");
            temp.write("model/classic.fbx", fbx.as_bytes());
            let model = temp.assets().load_model("/model/classic.fbx").unwrap();
            assert_eq!(model.meshes[0].material.opacity, expected);
        }
    }

    #[cfg(feature = "fbx")]
    #[test]
    fn classic_transparency_maps_invert_the_combined_scalar_and_map() {
        let temp = TempAssets::new("classic-transparency-map");
        for map in ["TransparentColor", "TransparencyFactor"] {
            let properties = "\t\t\tP: \"TransparentColor\", \"Color\", \"\", \"A\", 0.5, 0.5, 0.5\n\t\t\tP: \"TransparencyFactor\", \"Number\", \"\", \"A\", 0.5";
            let fbx = arnold_quad_fbx(properties, &[(map, "transparency.png")], false)
                .replace("\tC: \"OO\",3000,5000\n", "");
            temp.write("model/classic.fbx", fbx.as_bytes());
            temp.write(
                "model/transparency.png",
                &encode_png(
                    1,
                    1,
                    png::ColorType::Rgba,
                    png::BitDepth::Eight,
                    &[128, 128, 128, 255],
                ),
            );
            let model = temp.assets().load_model("/model/classic.fbx").unwrap();
            let material = &model.meshes[0].material;
            assert_eq!(material.opacity, 1.0);
            assert_eq!(
                model.textures[material.base_color_texture.unwrap()].rgba[3],
                191
            );
            assert_eq!(material.alpha_mode, ModelAlphaMode::Blend);
        }
    }

    #[cfg(feature = "fbx")]
    #[test]
    fn classic_phong_reflection_and_shininess_never_become_metallic_roughness() {
        let temp = TempAssets::new("classic-phong-pbr-regression");
        let fbx = quad_fbx(None).replace(
            "ShadingModel: \"lambert\"",
            "ShadingModel: \"Phong\"\n\t\tProperties70: {\n\t\t\tP: \"ReflectionFactor\", \"Number\", \"\", \"A\", 1\n\t\t\tP: \"ShininessExponent\", \"Number\", \"\", \"A\", 20\n\t\t}",
        );
        temp.write("model/phong.fbx", fbx.as_bytes());

        let model = temp.assets().load_model("/model/phong.fbx").unwrap();
        let material = &model.meshes[0].material;
        assert_eq!(material.metallic, 0.0);
        assert_eq!(material.roughness, 1.0);
        assert!(material.metallic_roughness_texture.is_none());
    }

    #[cfg(feature = "fbx")]
    #[test]
    fn separate_metallic_roughness_maps_pack_red_channels_and_reuse_the_packed_pair() {
        let temp = TempAssets::new("pbr-packed-maps");
        let fbx = arnold_quad_fbx(
            "\t\t\tP: \"metalness\", \"Number\", \"\", \"A\", 0.2\n\t\t\tP: \"specularRoughness\", \"Number\", \"\", \"A\", 0.3",
            &[
                ("specularRoughness", "rough.png"),
                ("metalness", "metal.png"),
            ],
            true,
        );
        temp.write("model/pbr.fbx", fbx.as_bytes());
        temp.write(
            "model/rough.png",
            &encode_png(
                2,
                1,
                png::ColorType::Rgba,
                png::BitDepth::Eight,
                &[64, 8, 9, 255, 192, 10, 11, 255],
            ),
        );
        temp.write(
            "model/metal.png",
            &encode_png(
                2,
                1,
                png::ColorType::Rgba,
                png::BitDepth::Eight,
                &[16, 18, 19, 255, 32, 20, 21, 255],
            ),
        );

        let model = temp.assets().load_model("/model/pbr.fbx").unwrap();
        assert_eq!(model.meshes.len(), 2);
        let first = &model.meshes[0].material;
        let second = &model.meshes[1].material;
        assert_eq!(first.metallic, 1.0);
        assert_eq!(first.roughness, 1.0);
        assert_eq!(
            first.metallic_roughness_texture,
            second.metallic_roughness_texture
        );
        assert_eq!(model.textures.len(), 3);
        let packed = &model.textures[first.metallic_roughness_texture.unwrap()];
        assert_eq!((packed.width, packed.height), (2, 1));
        assert_eq!(packed.rgba, [0, 64, 16, 255, 0, 192, 32, 255]);
        assert_eq!(convert_in_place(&temp.assets(), "/model/pbr.fbx"), model);
    }

    #[cfg(feature = "fbx")]
    #[test]
    fn one_shared_metallic_roughness_texture_keeps_gltf_g_and_b_channels() {
        let temp = TempAssets::new("pbr-shared-map");
        let mut fbx = arnold_quad_fbx("", &[("specularRoughness", "packed.png")], false);
        fbx = fbx.replace(
            "\tC: \"OP\",4000,3000,\"specularRoughness\"\n",
            "\tC: \"OP\",4000,3000,\"specularRoughness\"\n\tC: \"OP\",4000,3000,\"metalness\"\n",
        );
        temp.write("model/pbr.fbx", fbx.as_bytes());
        temp.write(
            "model/packed.png",
            &encode_png(
                1,
                1,
                png::ColorType::Rgba,
                png::BitDepth::Eight,
                &[9, 64, 192, 255],
            ),
        );

        let model = temp.assets().load_model("/model/pbr.fbx").unwrap();
        assert_eq!(model.textures.len(), 1);
        let material = &model.meshes[0].material;
        assert_eq!(material.metallic, 1.0);
        assert_eq!(material.roughness, 1.0);
        assert_eq!(material.metallic_roughness_texture, Some(0));
        assert_eq!(model.textures[0].rgba, [9, 64, 192, 255]);
        assert_eq!(convert_in_place(&temp.assets(), "/model/pbr.fbx"), model);
    }

    #[cfg(feature = "fbx")]
    #[test]
    fn prebuilt_keeps_distinct_texture_elements_that_reference_the_same_png() {
        let temp = TempAssets::new("prebuilt-distinct-texture-elements");
        let fbx = arnold_quad_fbx(
            "",
            &[("baseColor", "same.png"), ("normalCamera", "same.png")],
            false,
        );
        temp.write("model/pbr.fbx", fbx.as_bytes());
        temp.write("model/same.png", &rgba_png());

        let assets = temp.assets();
        let model = assets.load_model("/model/pbr.fbx").unwrap();
        assert_eq!(model.textures.len(), 2);
        assert_eq!(model.meshes[0].material.base_color_texture, Some(0));
        assert_eq!(model.meshes[0].material.normal_texture, Some(1));
        assert_eq!(convert_in_place(&assets, "/model/pbr.fbx"), model);
    }

    #[cfg(feature = "fbx")]
    #[test]
    fn prebuilt_resolves_external_png_changes_when_loaded() {
        let temp = TempAssets::new("prebuilt-changing-texture");
        let fbx = quad_fbx(Some("paint.png"));
        temp.write("model/quad.fbx", fbx.as_bytes());
        let opaque = encode_png(
            1,
            1,
            png::ColorType::Rgba,
            png::BitDepth::Eight,
            &[10, 20, 30, 255],
        );
        temp.write("model/paint.png", &opaque);
        let assets = temp.assets();
        let ready = assets.convert_model("/model/quad.fbx").unwrap();
        temp.write("model/quad.fbx", &ready);
        assert_eq!(
            assets.load_model("/model/quad.fbx").unwrap().meshes[0]
                .material
                .alpha_mode,
            ModelAlphaMode::Opaque
        );

        let transparent = encode_png(
            1,
            1,
            png::ColorType::Rgba,
            png::BitDepth::Eight,
            &[10, 20, 30, 0],
        );
        temp.write("model/paint.png", &transparent);
        let actual = assets.load_model("/model/quad.fbx").unwrap();
        assert_eq!(actual.meshes[0].material.alpha_mode, ModelAlphaMode::Blend);

        let direct = TempAssets::new("prebuilt-changing-texture-direct");
        direct.write("model/quad.fbx", fbx.as_bytes());
        direct.write("model/paint.png", &transparent);
        assert_eq!(
            actual,
            direct.assets().load_model("/model/quad.fbx").unwrap()
        );
    }

    #[cfg(feature = "fbx")]
    #[test]
    fn metallic_roughness_maps_require_matching_dimensions() {
        let temp = TempAssets::new("pbr-size-mismatch");
        let fbx = arnold_quad_fbx(
            "",
            &[
                ("specularRoughness", "rough.png"),
                ("metalness", "metal.png"),
            ],
            false,
        );
        temp.write("model/pbr.fbx", fbx.as_bytes());
        temp.write("model/rough.png", &rgba_png());
        temp.write(
            "model/metal.png",
            &encode_png(
                1,
                1,
                png::ColorType::Rgba,
                png::BitDepth::Eight,
                &[1, 2, 3, 255],
            ),
        );

        assert!(matches!(
            temp.assets().load_model("/model/pbr.fbx"),
            Err(AssetError::Decode { message, .. })
                if message.contains("roughness и metallic карты имеют разные размеры")
        ));
    }

    #[test]
    fn a_single_metallic_roughness_map_packs_its_red_channel_and_defaults_the_missing_channel() {
        let roughness = TextureData {
            width: 1,
            height: 1,
            rgba: vec![64, 10, 20, 255],
        };
        let metalness = TextureData {
            width: 1,
            height: 1,
            rgba: vec![16, 30, 40, 255],
        };
        assert_eq!(
            pack_metallic_roughness(Some(&roughness), None)
                .unwrap()
                .rgba,
            [0, 64, 255, 255]
        );
        assert_eq!(
            pack_metallic_roughness(None, Some(&metalness))
                .unwrap()
                .rgba,
            [0, 255, 16, 255]
        );
    }

    #[cfg(feature = "fbx")]
    #[test]
    fn metallic_roughness_is_enabled_only_for_supported_shader_types() {
        for shader_type in [
            ufbx::ShaderType::OslStandardSurface,
            ufbx::ShaderType::ArnoldStandardSurface,
            ufbx::ShaderType::E3DsMaxPhysicalMaterial,
            ufbx::ShaderType::E3DsMaxPbrMetalRough,
            ufbx::ShaderType::E3DsMaxPbrSpecGloss,
            ufbx::ShaderType::GltfMaterial,
            ufbx::ShaderType::OpenpbrMaterial,
            ufbx::ShaderType::ShaderfxGraph,
        ] {
            assert!(supports_metallic_roughness(shader_type), "{shader_type:?}");
        }
        for shader_type in [
            ufbx::ShaderType::Unknown,
            ufbx::ShaderType::FbxLambert,
            ufbx::ShaderType::FbxPhong,
            ufbx::ShaderType::BlenderPhong,
            ufbx::ShaderType::WavefrontMtl,
        ] {
            assert!(!supports_metallic_roughness(shader_type), "{shader_type:?}");
        }
    }

    #[cfg(feature = "fbx")]
    #[test]
    fn pbr_dependencies_follow_base_normal_roughness_metalness_usage_without_decoding() {
        let temp = TempAssets::new("pbr-dependencies");
        let fbx = arnold_quad_fbx(
            "",
            &[
                ("baseColor", "base.png"),
                ("normalCamera", "normal.png"),
                ("specularRoughness", "rough.png"),
                ("metalness", "metal.png"),
            ],
            false,
        );
        temp.write("model/pbr.fbx", fbx.as_bytes());
        for name in ["base.png", "normal.png", "rough.png", "metal.png"] {
            temp.write(&format!("model/{name}"), b"not decoded as PNG");
        }

        assert_eq!(
            temp.assets().model_dependencies("/model/pbr.fbx").unwrap(),
            [
                "/model/base.png",
                "/model/normal.png",
                "/model/rough.png",
                "/model/metal.png"
            ]
        );
    }

    #[cfg(feature = "fbx")]
    #[test]
    fn pbr_missing_external_texture_preserves_missing_texture_error() {
        let temp = TempAssets::new("pbr-missing-texture");
        let fbx = arnold_quad_fbx("", &[("specularRoughness", "lost.png")], false);
        temp.write("model/pbr.fbx", fbx.as_bytes());

        let dependencies_error = temp
            .assets()
            .model_dependencies("/model/pbr.fbx")
            .unwrap_err();
        let model_error = temp.assets().load_model("/model/pbr.fbx").unwrap_err();
        let missing = |error: AssetError| match error {
            AssetError::MissingTexture {
                texture,
                tried,
                model,
            } => (texture, tried, model),
            other => panic!("expected MissingTexture, got {other:?}"),
        };
        let dependency_missing = missing(dependencies_error);
        assert_eq!(dependency_missing, missing(model_error));
        assert_eq!(dependency_missing.0, "lost.png");
    }

    #[cfg(feature = "fbx")]
    #[test]
    fn classic_phong_reflection_texture_is_not_a_model_dependency() {
        let temp = TempAssets::new("classic-phong-reflection-dependency");
        let fbx = quad_fbx(None)
            .replace(
                "ShadingModel: \"lambert\"",
                "ShadingModel: \"Phong\"\n\t\tProperties70: {\n\t\t\tP: \"ReflectionFactor\", \"Number\", \"\", \"A\", 1\n\t\t\tP: \"ShininessExponent\", \"Number\", \"\", \"A\", 20\n\t\t}",
            )
            .replace(
                "Objects:  {\n",
                "Objects:  {\n\tTexture: 4000, \"Texture::Reflection\", \"\" {\n\t\tType: \"TextureVideoClip\"\n\t\tFileName: \"lost.png\"\n\t\tRelativeFilename: \"lost.png\"\n\t}\n",
            )
            .replace(
                "\tC: \"OO\",3000,2000\n",
                "\tC: \"OO\",3000,2000\n\tC: \"OP\",4000,3000,\"ReflectionFactor\"\n",
            );
        temp.write("model/phong.fbx", fbx.as_bytes());

        assert!(
            temp.assets()
                .model_dependencies("/model/phong.fbx")
                .unwrap()
                .is_empty()
        );
        let model = temp.assets().load_model("/model/phong.fbx").unwrap();
        assert_eq!(model.meshes[0].material.metallic, 0.0);
        assert_eq!(model.meshes[0].material.roughness, 1.0);
        assert!(
            model.meshes[0]
                .material
                .metallic_roughness_texture
                .is_none()
        );
    }

    #[test]
    fn bounds_cover_all_meshes_and_are_absent_for_empty_models() {
        let vertex = |position| ModelVertex {
            position,
            normal: [0.0, 1.0, 0.0],
            uv: [0.0, 0.0],
            tangent: [0.0; 4],
        };
        let mesh = |positions: [[f32; 3]; 3]| ModelMesh {
            vertices: positions.map(vertex).to_vec(),
            indices: vec![0, 1, 2],
            material: ModelMaterial::default(),
            skin: None,
        };
        let model = Model {
            meshes: vec![
                mesh([[0.0, 0.0, 0.0], [1.0, 2.0, 0.0], [0.0, 0.0, 3.0]]),
                mesh([[-1.0, 0.0, 0.0], [0.0, -2.0, 0.0], [0.0, 0.0, -3.0]]),
            ],
            textures: Vec::new(),
            skeleton: None,
            animations: Vec::new(),
        };
        assert_eq!(model.bounds(), Some(([-1.0, -2.0, -3.0], [1.0, 2.0, 3.0])));
        let empty = Model {
            meshes: Vec::new(),
            textures: Vec::new(),
            skeleton: None,
            animations: Vec::new(),
        };
        assert_eq!(empty.bounds(), None);
    }

    #[test]
    fn source_fbx_is_not_subject_to_the_prebuilt_model_size_limit() {
        let path = Path::new("/model/large.fbx");
        assert!(ensure_model_size_if_prebuilt(1024, b"source FBX", path, 0).is_ok());
        assert!(ensure_model_data_size(b"source FBX", path, 0).is_ok());
        assert!(matches!(
            ensure_model_size_if_prebuilt(1024, b"RVEMODEL", path, 0),
            Err(AssetError::Decode { .. })
        ));
        assert!(matches!(
            ensure_model_data_size(b"RVEMODEL", path, 0),
            Err(AssetError::Decode { .. })
        ));
    }

    #[test]
    fn missing_texture_without_filename_keeps_an_empty_error_name() {
        let temp = TempAssets::new("missing-texture-name");
        let assets = temp.assets();
        let error = resolve_external_texture_source(
            &TextureResolveContext {
                model_virtual_path: "/model/test.fbx",
                model_path: Path::new("test.fbx"),
                assets: &assets,
            },
            ExternalTextureSource {
                filename: "",
                relative_filename: "",
                absolute_filename: "",
            },
            |_, _| Ok(()),
        )
        .unwrap_err();
        assert!(matches!(
            error,
            AssetError::MissingTexture { ref texture, .. } if texture.is_empty()
        ));
    }

    #[test]
    fn cancellation_is_checked_between_prebuilt_parts_and_before_textures() {
        use std::sync::atomic::{AtomicBool, Ordering};

        let path = Path::new("ready.fbx");
        let cancel = AtomicBool::new(false);
        let mut visited = Vec::new();
        let error = for_each_cancellable([0, 1], &cancel, path, |part| {
            visited.push(part);
            if part == 0 {
                cancel.store(true, Ordering::Relaxed);
            }
            Ok(())
        })
        .unwrap_err();
        assert_eq!(visited, [0]);
        assert!(matches!(error, AssetError::Cancelled { .. }));

        let temp = TempAssets::new("prebuilt-texture-cancel");
        let assets = temp.assets();
        let cancel = AtomicBool::new(true);
        let mut model = Model {
            meshes: Vec::new(),
            textures: Vec::new(),
            skeleton: None,
            animations: Vec::new(),
        };
        let mut texture_cache = super::ModelTextureCache::default();
        let mut loader = super::ModelMaterialLoader {
            model_virtual_path: "/model/ready.fbx",
            model_path: path,
            assets: &assets,
            model: &mut model,
            texture_cache: &mut texture_cache,
            cancel: &cancel,
        };
        let references = [super::prebuilt::TextureReference::Embedded {
            name: "texture.png".to_owned(),
            png: Vec::new(),
        }];
        let error = loader.load_texture(0, &references).unwrap_err();
        assert!(matches!(error, AssetError::Cancelled { .. }));
    }
}
