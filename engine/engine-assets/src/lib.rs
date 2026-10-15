//! Загрузка и хранение текстур, моделей и других ресурсов.
//!
//! Ресурсы лежат в каталоге [`ASSETS_DIR`] рабочего каталога и адресуются путями
//! вида `/model/cube.fbx`: такой путь указывает на файл `assets/model/cube.fbx`.

use std::collections::HashMap;
use std::error::Error;
use std::fmt;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

/// Каталог ресурсов относительно рабочего каталога приложения.
pub const ASSETS_DIR: &str = "assets";

/// Предел памяти декодера PNG; хватает на текстуру 8192x8192 RGBA с запасом.
const PNG_DECODE_LIMIT: usize = 512 * 1024 * 1024;

/// Пиксели RGBA8 в sRGB по строкам сверху вниз.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TextureData {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}

/// Вершина модели в метрах; оси: +X вправо, +Y вверх, +Z к зрителю.
///
/// `uv` отсчитывается от левого верхнего угла текстуры.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ModelVertex {
    pub position: [f32; 3],
    pub normal: [f32; 3],
    pub uv: [f32; 2],
}

/// Часть модели с одним материалом; каждые три индекса образуют треугольник.
#[derive(Clone, Debug, PartialEq)]
pub struct ModelMesh {
    pub vertices: Vec<ModelVertex>,
    pub indices: Vec<u32>,
    /// Индекс в [`Model::textures`].
    pub texture: Option<usize>,
}

/// Модель, собранная из всех мешей файла в мировом пространстве сцены.
#[derive(Clone, Debug, PartialEq)]
pub struct Model {
    pub meshes: Vec<ModelMesh>,
    pub textures: Vec<TextureData>,
}

impl Model {
    /// Минимальный и максимальный углы bounding box всех вершин.
    pub fn bounds(&self) -> Option<([f32; 3], [f32; 3])> {
        let mut positions = self
            .meshes
            .iter()
            .flat_map(|mesh| &mesh.vertices)
            .map(|vertex| vertex.position);
        let first = positions.next()?;
        Some(positions.fold((first, first), |(min, max), position| {
            (
                std::array::from_fn(|axis| min[axis].min(position[axis])),
                std::array::from_fn(|axis| max[axis].max(position[axis])),
            )
        }))
    }
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
}

impl Assets {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
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
        let bytes = read(&path)?;
        decode_png(&bytes).map_err(|message| AssetError::Decode { path, message })
    }

    /// Загружает FBX-модель с её PNG-текстурами.
    ///
    /// Внешняя текстура ищется по пути, записанному в FBX относительно файла модели,
    /// а затем по имени файла рядом с моделью. Встроенные в FBX PNG тоже поддерживаются.
    pub fn load_model(&self, asset_path: &str) -> Result<Model, AssetError> {
        let path = self.resolve(asset_path)?;
        require_extension(&path, "fbx")?;
        load_fbx(&path)
    }
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

fn read(path: &Path) -> Result<Vec<u8>, AssetError> {
    fs::read(path).map_err(|error| AssetError::Io {
        path: path.to_owned(),
        error,
    })
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

fn load_fbx(path: &Path) -> Result<Model, AssetError> {
    let decode_error = |message: String| AssetError::Decode {
        path: path.to_owned(),
        message,
    };
    // Существование файла проверяется заранее, чтобы ошибка была такой же, как у текстур.
    fs::metadata(path).map_err(|error| AssetError::Io {
        path: path.to_owned(),
        error,
    })?;
    let filename = path
        .to_str()
        .ok_or_else(|| decode_error("путь модели не в UTF-8".to_owned()))?;
    let options = ufbx::LoadOpts {
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
    let scene = ufbx::load_file(filename, options)
        .map_err(|error| decode_error(format!("ошибка FBX: {error:?}")))?;

    let mut model = Model {
        meshes: Vec::new(),
        textures: Vec::new(),
    };
    let mut loaded_textures = HashMap::new();
    let mut triangle = Vec::new();
    for node in scene.nodes.iter() {
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
        triangle.resize(mesh.max_face_triangles * 3, 0);
        for part in mesh.material_parts.iter() {
            let mut vertices = Vec::with_capacity(part.num_triangles * 3);
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
                    let uv = if mesh.vertex_uv.exists {
                        let uv = mesh.vertex_uv[index];
                        // В FBX ось v направлена вверх от нижнего края текстуры.
                        [uv.x as f32, 1.0 - uv.y as f32]
                    } else {
                        [0.0, 0.0]
                    };
                    vertices.push(ModelVertex {
                        position: [position.x as f32, position.y as f32, position.z as f32],
                        normal: normalize([normal.x as f32, normal.y as f32, normal.z as f32]),
                        uv,
                    });
                }
            }
            if vertices.is_empty() {
                continue;
            }
            let texture = match materials
                .get(part.index as usize)
                .and_then(|material| diffuse_texture(material))
            {
                Some(texture) => Some(match loaded_textures.get(&texture.element.element_id) {
                    Some(&index) => index,
                    None => {
                        model.textures.push(load_fbx_texture(path, texture)?);
                        let index = model.textures.len() - 1;
                        loaded_textures.insert(texture.element.element_id, index);
                        index
                    }
                }),
                None => None,
            };
            let vertex_count = u32::try_from(vertices.len())
                .map_err(|_| decode_error("в меше больше u32::MAX вершин".to_owned()))?;
            model.meshes.push(ModelMesh {
                vertices,
                indices: (0..vertex_count).collect(),
                texture,
            });
        }
    }
    if model.meshes.is_empty() {
        return Err(decode_error("в модели нет треугольников".to_owned()));
    }
    Ok(model)
}

/// Файловая текстура основного цвета: классический diffuse FBX или base color PBR.
fn diffuse_texture(material: &ufbx::Material) -> Option<&ufbx::Texture> {
    [&material.fbx.diffuse_color, &material.pbr.base_color]
        .into_iter()
        .filter_map(|map| map.texture.as_deref())
        .find(|texture| texture.type_ == ufbx::TextureType::File)
}

fn load_fbx_texture(model: &Path, texture: &ufbx::Texture) -> Result<TextureData, AssetError> {
    if !texture.content.is_empty() {
        return decode_png(&texture.content).map_err(|message| AssetError::Decode {
            path: model.to_owned(),
            message: format!(
                "встроенная текстура {:?}: {message}",
                texture.element.name.as_ref() as &str
            ),
        });
    }
    let tried = texture_candidates(
        model,
        &texture.filename,
        &texture.relative_filename,
        &texture.absolute_filename,
    );
    let Some(path) = tried.iter().find(|path| path.is_file()) else {
        let name = [
            &texture.relative_filename,
            &texture.absolute_filename,
            &texture.filename,
        ]
        .into_iter()
        .map(|name| name.as_ref() as &str)
        .find(|name| !name.is_empty())
        .unwrap_or_default()
        .to_owned();
        return Err(AssetError::MissingTexture {
            model: model.to_owned(),
            texture: name,
            tried,
        });
    };
    require_extension(path, "png")?;
    let bytes = read(path)?;
    decode_png(&bytes).map_err(|message| AssetError::Decode {
        path: path.clone(),
        message,
    })
}

/// Пути поиска внешней текстуры без повторов: путь, который ufbx разрешил
/// относительно модели, затем файлы с тем же именем рядом с моделью.
fn texture_candidates(
    model: &Path,
    resolved: &str,
    relative: &str,
    absolute: &str,
) -> Vec<PathBuf> {
    let directory = model.parent().unwrap_or(Path::new(""));
    let mut candidates = Vec::new();
    if !resolved.is_empty() {
        candidates.push(PathBuf::from(resolved));
    }
    for name in [relative, absolute] {
        // В FBX из Windows разделителем может быть `\`, поэтому имя выделяется вручную.
        if let Some(file_name) = name
            .rsplit(['/', '\\'])
            .next()
            .filter(|name| !name.is_empty())
        {
            candidates.push(directory.join(file_name));
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
        AssetError, Assets, Model, ModelMesh, ModelVertex, decode_png, texture_candidates,
    };
    use std::fs;
    use std::path::{Path, PathBuf};

    /// Временный каталог ресурсов, удаляемый в конце теста.
    struct TempAssets {
        root: PathBuf,
    }

    impl TempAssets {
        fn new(name: &str) -> Self {
            let root =
                std::env::temp_dir().join(format!("rve-assets-{name}-{}", std::process::id()));
            let _ = fs::remove_dir_all(&root);
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
    }

    impl Drop for TempAssets {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
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

    /// Квадрат 2x2 в плоскости XY с нормалью +Z и UV на весь квадрат.
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
    fn fbx_quad_is_triangulated_with_normals_flipped_uv_and_meters() {
        let temp = TempAssets::new("quad");
        temp.write("model/quad.fbx", quad_fbx(None).as_bytes());
        let model = temp.assets().load_model("/model/quad.fbx").unwrap();

        assert_eq!(model.meshes.len(), 1);
        assert!(model.textures.is_empty());
        let mesh = &model.meshes[0];
        assert_eq!(mesh.texture, None);
        assert_eq!(mesh.indices, [0, 1, 2, 3, 4, 5]);
        assert_eq!(mesh.vertices.len(), 6);
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

        for model in ["/model/relative.fbx", "/model/absolute.fbx"] {
            let model = assets.load_model(model).unwrap();
            assert_eq!(model.meshes[0].texture, Some(0), "{model:?}");
            assert_eq!(model.textures.len(), 1);
            assert_eq!(model.textures[0].rgba, [1, 2, 3, 4, 5, 6, 7, 8]);
        }
    }

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
        let model = temp.assets().load_model("/model/embedded.fbx").unwrap();
        assert_eq!(model.meshes[0].texture, Some(0));
        assert_eq!(model.textures[0].rgba, [1, 2, 3, 4, 5, 6, 7, 8]);
    }

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
        let model = Path::new("assets/model/cube.fbx");
        assert_eq!(
            texture_candidates(
                model,
                "assets/model/paint.png",
                "paint.png",
                "C:\\art\\paint.png"
            ),
            [PathBuf::from("assets/model/paint.png")]
        );
        assert_eq!(
            texture_candidates(model, "", "../texture/paint.png", ""),
            [PathBuf::from("assets/model/paint.png")]
        );
        assert!(texture_candidates(model, "", "", "").is_empty());
    }

    #[test]
    fn bounds_cover_all_meshes_and_are_absent_for_empty_models() {
        let vertex = |position| ModelVertex {
            position,
            normal: [0.0, 1.0, 0.0],
            uv: [0.0, 0.0],
        };
        let mesh = |positions: [[f32; 3]; 3]| ModelMesh {
            vertices: positions.map(vertex).to_vec(),
            indices: vec![0, 1, 2],
            texture: None,
        };
        let model = Model {
            meshes: vec![
                mesh([[0.0, 0.0, 0.0], [1.0, 2.0, 0.0], [0.0, 0.0, 3.0]]),
                mesh([[-1.0, 0.0, 0.0], [0.0, -2.0, 0.0], [0.0, 0.0, -3.0]]),
            ],
            textures: Vec::new(),
        };
        assert_eq!(model.bounds(), Some(([-1.0, -2.0, -3.0], [1.0, 2.0, 3.0])));
        let empty = Model {
            meshes: Vec::new(),
            textures: Vec::new(),
        };
        assert_eq!(empty.bounds(), None);
    }
}
