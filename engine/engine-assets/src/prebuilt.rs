//! Versioned, pointer-free representation of a prepared FBX model.

use crate::{
    ModelAnimation, ModelInheritMode, ModelJoint, ModelKey, ModelNode, ModelSkin, ModelTrack,
    ModelTransform, ModelVertex,
};
use std::collections::HashMap;
use std::mem::size_of;

pub(crate) const MAGIC: &[u8; 8] = b"RVEMODEL";
pub const PREBUILT_MODEL_VERSION: u32 = 1;
pub const PREBUILT_MODEL_MAX_BYTES: usize = 256 * 1024 * 1024;

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum TextureReference {
    External {
        name: String,
        filename: String,
        relative_filename: String,
        absolute_filename: String,
    },
    Embedded {
        name: String,
        png: Vec<u8>,
    },
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct OpacityRecipe {
    pub(crate) source: usize,
    pub(crate) invert: bool,
    pub(crate) transparency_scale: f32,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct MaterialRecipe {
    pub(crate) base_color: [f32; 3],
    pub(crate) metallic: f32,
    pub(crate) roughness: f32,
    pub(crate) emissive: [f32; 3],
    pub(crate) opacity: f32,
    pub(crate) double_sided: bool,
    pub(crate) occlusion_strength: f32,
    pub(crate) base_color_source: Option<usize>,
    pub(crate) normal_source: Option<usize>,
    pub(crate) roughness_source: Option<usize>,
    pub(crate) metalness_source: Option<usize>,
    pub(crate) emissive_source: Option<usize>,
    pub(crate) occlusion_source: Option<usize>,
    pub(crate) opacity_source: Option<OpacityRecipe>,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct PreparedMesh {
    pub(crate) vertices: Vec<ModelVertex>,
    pub(crate) indices: Vec<u32>,
    pub(crate) skin: Option<ModelSkin>,
    pub(crate) material: MaterialRecipe,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct PreparedModel {
    pub(crate) texture_references: Vec<TextureReference>,
    /// `None` is a clip-only FBX. `Some` contains all model parts in load order.
    pub(crate) meshes: Option<Vec<PreparedMesh>>,
    pub(crate) skeleton: Option<crate::ModelSkeleton>,
    pub(crate) model_animations: Vec<ModelAnimation>,
    pub(crate) has_animation_stacks: bool,
    pub(crate) clips: Vec<ModelAnimation>,
}

#[derive(Default)]
pub(crate) struct TextureSources {
    indices: HashMap<u32, usize>,
    pub(crate) references: Vec<TextureReference>,
}

impl TextureSources {
    pub(crate) fn intern(&mut self, texture: &ufbx::Texture) -> usize {
        let id = texture.element.element_id;
        if let Some(&index) = self.indices.get(&id) {
            return index;
        }
        let name = texture.element.name.to_string();
        let reference = if texture.content.is_empty() {
            TextureReference::External {
                name,
                filename: texture.filename.to_string(),
                relative_filename: texture.relative_filename.to_string(),
                absolute_filename: texture.absolute_filename.to_string(),
            }
        } else {
            TextureReference::Embedded {
                name,
                png: texture.content.to_vec(),
            }
        };
        let index = self.references.len();
        self.references.push(reference);
        self.indices.insert(id, index);
        index
    }
}

pub(crate) fn is_prebuilt_model(bytes: &[u8]) -> bool {
    bytes.starts_with(MAGIC)
}

pub(crate) fn encode(model: &PreparedModel) -> Result<Vec<u8>, String> {
    let mut writer = Writer::new(PREBUILT_MODEL_MAX_BYTES);
    writer.bytes(MAGIC)?;
    writer.u32(PREBUILT_MODEL_VERSION)?;
    let bounds = model_bounds(model.meshes.as_deref());
    writer.bool(bounds.is_some())?;
    if let Some((min, max)) = bounds {
        for value in min.into_iter().chain(max) {
            writer.f32(value)?;
        }
    }
    writer.count(model.texture_references.len())?;
    for reference in &model.texture_references {
        match reference {
            TextureReference::External {
                name,
                filename,
                relative_filename,
                absolute_filename,
            } => {
                writer.u8(0)?;
                writer.string(name)?;
                writer.string(filename)?;
                writer.string(relative_filename)?;
                writer.string(absolute_filename)?;
            }
            TextureReference::Embedded { name, png } => {
                writer.u8(1)?;
                writer.string(name)?;
                writer.count(png.len())?;
                writer.bytes(png)?;
            }
        }
    }
    writer.bool(model.meshes.is_some())?;
    if let Some(meshes) = &model.meshes {
        writer.count(meshes.len())?;
        for mesh in meshes {
            writer.count(mesh.vertices.len())?;
            for vertex in &mesh.vertices {
                for value in vertex
                    .position
                    .into_iter()
                    .chain(vertex.normal)
                    .chain(vertex.uv)
                    .chain(vertex.tangent)
                {
                    writer.f32(value)?;
                }
            }
            writer.count(mesh.indices.len())?;
            for &index in &mesh.indices {
                writer.u32(index)?;
            }
            writer.bool(mesh.skin.is_some())?;
            if let Some(skin) = &mesh.skin {
                writer.count(skin.joints.len())?;
                for joints in &skin.joints {
                    for &joint in joints {
                        writer.u32(joint)?;
                    }
                }
                writer.count(skin.weights.len())?;
                for weights in &skin.weights {
                    for &weight in weights {
                        writer.f32(weight)?;
                    }
                }
            }
            encode_material(&mut writer, &mesh.material)?;
        }
        writer.bool(model.skeleton.is_some())?;
        if let Some(skeleton) = &model.skeleton {
            writer.count(skeleton.nodes.len())?;
            for node in &skeleton.nodes {
                writer.string(&node.name)?;
                writer.option_index(node.parent)?;
                writer.u8(match node.inherit_mode {
                    ModelInheritMode::Normal => 0,
                    ModelInheritMode::IgnoreParentScale => 1,
                    ModelInheritMode::ComponentwiseScale => 2,
                })?;
                writer.option_index(node.inherit_scale_node)?;
                encode_transform(&mut writer, node.rest)?;
            }
            writer.count(skeleton.joints.len())?;
            for joint in &skeleton.joints {
                writer.index(joint.node)?;
                for column in joint.inverse_bind {
                    for value in column {
                        writer.f32(value)?;
                    }
                }
            }
        }
        encode_animations(&mut writer, &model.model_animations)?;
    }
    writer.bool(model.has_animation_stacks)?;
    encode_animations(&mut writer, &model.clips)?;
    Ok(writer.into_inner())
}

pub(crate) fn decode(bytes: &[u8]) -> Result<PreparedModel, String> {
    decode_with_limits(bytes, PREBUILT_MODEL_MAX_BYTES, PREBUILT_MODEL_MAX_BYTES)
}

fn decode_with_limits(
    bytes: &[u8],
    max_result_bytes: usize,
    max_allocation_bytes: usize,
) -> Result<PreparedModel, String> {
    if bytes.len() > max_result_bytes {
        return Err(format!(
            "готовая модель превышает предел {} MiB",
            max_result_bytes / (1024 * 1024)
        ));
    }
    let mut reader = Reader::new(bytes, max_allocation_bytes);
    if reader.take(MAGIC.len())? != MAGIC {
        return Err("неверная сигнатура готовой модели".to_owned());
    }
    let version = reader.u32()?;
    if version != PREBUILT_MODEL_VERSION {
        return Err(format!(
            "готовая модель формата v{version}, поддерживается v{PREBUILT_MODEL_VERSION}; пересоберите релиз rve-build"
        ));
    }
    let header_bounds = if reader.bool()? {
        let mut min = [0.0; 3];
        let mut max = [0.0; 3];
        for value in &mut min {
            *value = reader.f32()?;
        }
        for value in &mut max {
            *value = reader.f32()?;
        }
        Some((min, max))
    } else {
        None
    };
    let texture_count = reader.count(1, "текстурных ссылок")?;
    let mut texture_references = reader.vector::<TextureReference>(texture_count)?;
    for _ in 0..texture_count {
        let kind = reader.u8()?;
        let name = reader.string()?;
        let reference = match kind {
            0 => TextureReference::External {
                name,
                filename: reader.string()?,
                relative_filename: reader.string()?,
                absolute_filename: reader.string()?,
            },
            1 => {
                let length = reader.count(1, "байтов встроенной текстуры")?;
                let png = reader.byte_vector(length)?;
                TextureReference::Embedded { name, png }
            }
            _ => return Err(format!("неизвестный тип ссылки на текстуру {kind}")),
        };
        texture_references.push(reference);
    }
    let meshes = if reader.bool()? {
        let count = reader.count(1, "частей модели")?;
        let mut meshes = reader.vector::<PreparedMesh>(count)?;
        for _ in 0..count {
            let vertex_count = reader.count(48, "вершин")?;
            let mut vertices = reader.vector::<ModelVertex>(vertex_count)?;
            for _ in 0..vertex_count {
                let mut values = [0.0; 12];
                for value in &mut values {
                    *value = reader.f32()?;
                }
                vertices.push(ModelVertex {
                    position: values[0..3].try_into().expect("fixed slice"),
                    normal: values[3..6].try_into().expect("fixed slice"),
                    uv: values[6..8].try_into().expect("fixed slice"),
                    tangent: values[8..12].try_into().expect("fixed slice"),
                });
            }
            let index_count = reader.count(4, "индексов")?;
            let mut indices = reader.vector::<u32>(index_count)?;
            for _ in 0..index_count {
                indices.push(reader.u32()?);
            }
            let skin = if reader.bool()? {
                let joint_count = reader.count(16, "наборов skin joints")?;
                let mut joints = reader.vector::<[u32; 4]>(joint_count)?;
                for _ in 0..joint_count {
                    joints.push([reader.u32()?, reader.u32()?, reader.u32()?, reader.u32()?]);
                }
                let weight_count = reader.count(16, "наборов skin weights")?;
                let mut weights = reader.vector::<[f32; 4]>(weight_count)?;
                for _ in 0..weight_count {
                    weights.push([reader.f32()?, reader.f32()?, reader.f32()?, reader.f32()?]);
                }
                Some(ModelSkin { joints, weights })
            } else {
                None
            };
            let material = decode_material(&mut reader)?;
            meshes.push(PreparedMesh {
                vertices,
                indices,
                skin,
                material,
            });
        }
        Some(meshes)
    } else {
        None
    };
    let (skeleton, model_animations) = if meshes.is_some() {
        let skeleton = if reader.bool()? {
            let count = reader.count(4, "узлов скелета")?;
            let mut nodes = reader.vector::<ModelNode>(count)?;
            for _ in 0..count {
                let name = reader.string()?;
                let parent = reader.option_index()?;
                let inherit_mode = match reader.u8()? {
                    0 => ModelInheritMode::Normal,
                    1 => ModelInheritMode::IgnoreParentScale,
                    2 => ModelInheritMode::ComponentwiseScale,
                    value => {
                        return Err(format!("неизвестный режим наследования масштаба {value}"));
                    }
                };
                let inherit_scale_node = reader.option_index()?;
                let rest = decode_transform(&mut reader)?;
                nodes.push(ModelNode {
                    name,
                    parent,
                    inherit_mode,
                    inherit_scale_node,
                    rest,
                });
            }
            let count = reader.count(68, "суставов скелета")?;
            let mut joints = reader.vector::<ModelJoint>(count)?;
            for _ in 0..count {
                let node = reader.index()?;
                let mut inverse_bind = [[0.0; 4]; 4];
                for column in &mut inverse_bind {
                    for value in column {
                        *value = reader.f32()?;
                    }
                }
                joints.push(ModelJoint { node, inverse_bind });
            }
            Some(crate::ModelSkeleton { nodes, joints })
        } else {
            None
        };
        (skeleton, decode_animations(&mut reader)?)
    } else {
        (None, Vec::new())
    };
    let has_animation_stacks = reader.bool()?;
    let clips = decode_animations(&mut reader)?;
    if reader.remaining() != 0 {
        return Err("готовая модель содержит лишние байты в конце".to_owned());
    }

    let model = PreparedModel {
        texture_references,
        meshes,
        skeleton,
        model_animations,
        has_animation_stacks,
        clips,
    };
    validate(&model)?;
    let actual_bounds = model_bounds(model.meshes.as_deref());
    if !same_bounds(header_bounds, actual_bounds) {
        return Err("границы модели в заголовке не совпадают с вершинами".to_owned());
    }
    Ok(model)
}

fn encode_material(writer: &mut Writer, material: &MaterialRecipe) -> Result<(), String> {
    for value in material
        .base_color
        .into_iter()
        .chain([material.metallic, material.roughness])
        .chain(material.emissive)
        .chain([material.opacity])
    {
        writer.f32(value)?;
    }
    writer.bool(material.double_sided)?;
    writer.f32(material.occlusion_strength)?;
    for source in [
        material.base_color_source,
        material.normal_source,
        material.roughness_source,
        material.metalness_source,
        material.emissive_source,
        material.occlusion_source,
    ] {
        writer.option_index(source)?;
    }
    writer.bool(material.opacity_source.is_some())?;
    if let Some(opacity) = &material.opacity_source {
        writer.index(opacity.source)?;
        writer.bool(opacity.invert)?;
        writer.f32(opacity.transparency_scale)?;
    }
    Ok(())
}

fn decode_material(reader: &mut Reader<'_>) -> Result<MaterialRecipe, String> {
    let base_color = [reader.f32()?, reader.f32()?, reader.f32()?];
    let metallic = reader.f32()?;
    let roughness = reader.f32()?;
    let emissive = [reader.f32()?, reader.f32()?, reader.f32()?];
    let opacity = reader.f32()?;
    let double_sided = reader.bool()?;
    let occlusion_strength = reader.f32()?;
    let base_color_source = reader.option_index()?;
    let normal_source = reader.option_index()?;
    let roughness_source = reader.option_index()?;
    let metalness_source = reader.option_index()?;
    let emissive_source = reader.option_index()?;
    let occlusion_source = reader.option_index()?;
    let opacity_source = if reader.bool()? {
        Some(OpacityRecipe {
            source: reader.index()?,
            invert: reader.bool()?,
            transparency_scale: reader.f32()?,
        })
    } else {
        None
    };
    Ok(MaterialRecipe {
        base_color,
        metallic,
        roughness,
        emissive,
        opacity,
        double_sided,
        occlusion_strength,
        base_color_source,
        normal_source,
        roughness_source,
        metalness_source,
        emissive_source,
        occlusion_source,
        opacity_source,
    })
}

fn encode_transform(writer: &mut Writer, transform: ModelTransform) -> Result<(), String> {
    for value in transform
        .translation
        .into_iter()
        .chain(transform.rotation)
        .chain(transform.scale)
    {
        writer.f32(value)?;
    }
    Ok(())
}

fn decode_transform(reader: &mut Reader<'_>) -> Result<ModelTransform, String> {
    Ok(ModelTransform {
        translation: [reader.f32()?, reader.f32()?, reader.f32()?],
        rotation: [reader.f32()?, reader.f32()?, reader.f32()?, reader.f32()?],
        scale: [reader.f32()?, reader.f32()?, reader.f32()?],
    })
}

fn encode_animations(writer: &mut Writer, animations: &[ModelAnimation]) -> Result<(), String> {
    writer.count(animations.len())?;
    for animation in animations {
        writer.string(&animation.name)?;
        writer.f32(animation.duration)?;
        writer.count(animation.tracks.len())?;
        for track in &animation.tracks {
            writer.string(&track.node)?;
            encode_keys(writer, &track.translation, |value| value.into_iter())?;
            encode_keys(writer, &track.rotation, |value| value.into_iter())?;
            encode_keys(writer, &track.scale, |value| value.into_iter())?;
        }
    }
    Ok(())
}

fn encode_keys<const N: usize>(
    writer: &mut Writer,
    keys: &[ModelKey<[f32; N]>],
    values: impl Fn([f32; N]) -> std::array::IntoIter<f32, N>,
) -> Result<(), String> {
    writer.count(keys.len())?;
    for key in keys {
        writer.f32(key.time)?;
        for value in values(key.value) {
            writer.f32(value)?;
        }
    }
    Ok(())
}

fn decode_animations(reader: &mut Reader<'_>) -> Result<Vec<ModelAnimation>, String> {
    let count = reader.count(8, "анимаций")?;
    let mut animations = reader.vector::<ModelAnimation>(count)?;
    for _ in 0..count {
        let name = reader.string()?;
        let duration = reader.f32()?;
        let track_count = reader.count(4, "дорожек анимации")?;
        let mut tracks = reader.vector::<ModelTrack>(track_count)?;
        for _ in 0..track_count {
            let node = reader.string()?;
            tracks.push(ModelTrack {
                node,
                translation: decode_keys::<3>(reader)?,
                rotation: decode_keys::<4>(reader)?,
                scale: decode_keys::<3>(reader)?,
            });
        }
        animations.push(ModelAnimation {
            name,
            duration,
            tracks,
        });
    }
    Ok(animations)
}

fn decode_keys<const N: usize>(reader: &mut Reader<'_>) -> Result<Vec<ModelKey<[f32; N]>>, String> {
    let count = reader.count(4 + N * 4, "ключей анимации")?;
    let mut keys = reader.vector::<ModelKey<[f32; N]>>(count)?;
    for _ in 0..count {
        let time = reader.f32()?;
        let mut value = [0.0; N];
        for component in &mut value {
            *component = reader.f32()?;
        }
        keys.push(ModelKey { time, value });
    }
    Ok(keys)
}

fn validate(model: &PreparedModel) -> Result<(), String> {
    if let Some(meshes) = &model.meshes {
        if meshes.is_empty() {
            return Err("готовая модель не содержит частей".to_owned());
        }
        let joint_count = model
            .skeleton
            .as_ref()
            .map_or(0, |skeleton| skeleton.joints.len());
        for mesh in meshes {
            if mesh.indices.len() % 3 != 0 {
                return Err("число индексов части модели не кратно трём".to_owned());
            }
            if mesh
                .indices
                .iter()
                .any(|&index| index as usize >= mesh.vertices.len())
            {
                return Err("индекс вершины выходит за пределы части модели".to_owned());
            }
            if mesh.skin.as_ref().is_some_and(|skin| {
                skin.joints.len() != mesh.vertices.len()
                    || skin.weights.len() != mesh.vertices.len()
            }) {
                return Err("skin части модели не совпадает с числом вершин".to_owned());
            }
            if let Some(skin) = &mesh.skin {
                if model.skeleton.is_none() {
                    return Err("skin части модели требует скелет".to_owned());
                }
                if skin
                    .joints
                    .iter()
                    .flatten()
                    .any(|&joint| joint as usize >= joint_count)
                {
                    return Err("индекс сустава skin выходит за пределы палитры".to_owned());
                }
            }
            for source in [
                mesh.material.base_color_source,
                mesh.material.normal_source,
                mesh.material.roughness_source,
                mesh.material.metalness_source,
                mesh.material.emissive_source,
                mesh.material.occlusion_source,
                mesh.material
                    .opacity_source
                    .as_ref()
                    .map(|opacity| opacity.source),
            ]
            .into_iter()
            .flatten()
            {
                if source >= model.texture_references.len() {
                    return Err("индекс текстурной ссылки выходит за пределы таблицы".to_owned());
                }
            }
        }
    } else if model.skeleton.is_some() || !model.model_animations.is_empty() {
        return Err("клип-only файл содержит секции модели".to_owned());
    }
    if let Some(skeleton) = &model.skeleton {
        for (index, node) in skeleton.nodes.iter().enumerate() {
            if node.parent.is_some_and(|parent| parent >= index)
                || node
                    .inherit_scale_node
                    .is_some_and(|parent| parent >= index)
            {
                return Err("родитель узла скелета должен предшествовать узлу".to_owned());
            }
        }
        if skeleton
            .joints
            .iter()
            .any(|joint| joint.node >= skeleton.nodes.len())
        {
            return Err("индекс узла сустава выходит за пределы скелета".to_owned());
        }
    }
    Ok(())
}

fn model_bounds(meshes: Option<&[PreparedMesh]>) -> Option<([f32; 3], [f32; 3])> {
    let mut positions = meshes?
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

fn same_bounds(
    expected: Option<([f32; 3], [f32; 3])>,
    actual: Option<([f32; 3], [f32; 3])>,
) -> bool {
    match (expected, actual) {
        (None, None) => true,
        (Some((a_min, a_max)), Some((b_min, b_max))) => a_min
            .into_iter()
            .chain(a_max)
            .zip(b_min.into_iter().chain(b_max))
            .all(|(a, b)| a.to_bits() == b.to_bits()),
        _ => false,
    }
}

struct Writer {
    bytes: Vec<u8>,
    limit: usize,
}

impl Writer {
    fn new(limit: usize) -> Self {
        Self {
            bytes: Vec::new(),
            limit,
        }
    }

    fn ensure(&mut self, additional: usize) -> Result<(), String> {
        let size = self
            .bytes
            .len()
            .checked_add(additional)
            .ok_or_else(|| "готовая модель превышает предел размера".to_owned())?;
        if size > self.limit {
            return Err(format!(
                "готовая модель превышает предел {} MiB",
                self.limit / (1024 * 1024)
            ));
        }
        self.bytes
            .try_reserve(additional)
            .map_err(|_| "недостаточно памяти для готовой модели".to_owned())
    }

    fn bytes(&mut self, bytes: &[u8]) -> Result<(), String> {
        self.ensure(bytes.len())?;
        self.bytes.extend_from_slice(bytes);
        Ok(())
    }

    fn u8(&mut self, value: u8) -> Result<(), String> {
        self.bytes(&[value])
    }

    fn bool(&mut self, value: bool) -> Result<(), String> {
        self.u8(u8::from(value))
    }

    fn u32(&mut self, value: u32) -> Result<(), String> {
        self.bytes(&value.to_le_bytes())
    }

    fn index(&mut self, value: usize) -> Result<(), String> {
        self.u32(
            u32::try_from(value).map_err(|_| "индекс готовой модели превышает u32".to_owned())?,
        )
    }

    fn count(&mut self, value: usize) -> Result<(), String> {
        self.index(value)
    }

    fn option_index(&mut self, value: Option<usize>) -> Result<(), String> {
        self.bool(value.is_some())?;
        if let Some(value) = value {
            self.index(value)?;
        }
        Ok(())
    }

    fn f32(&mut self, value: f32) -> Result<(), String> {
        self.u32(value.to_bits())
    }

    fn string(&mut self, value: &str) -> Result<(), String> {
        self.count(value.len())?;
        self.bytes(value.as_bytes())
    }

    fn into_inner(self) -> Vec<u8> {
        self.bytes
    }
}

struct Reader<'a> {
    bytes: &'a [u8],
    offset: usize,
    allocation_bytes: usize,
    allocation_limit: usize,
}

impl<'a> Reader<'a> {
    fn new(bytes: &'a [u8], allocation_limit: usize) -> Self {
        Self {
            bytes,
            offset: 0,
            allocation_bytes: 0,
            allocation_limit,
        }
    }

    fn remaining(&self) -> usize {
        self.bytes.len() - self.offset
    }

    fn take(&mut self, count: usize) -> Result<&'a [u8], String> {
        let end = self
            .offset
            .checked_add(count)
            .ok_or_else(|| "повреждённая готовая модель: неверная длина".to_owned())?;
        if end > self.bytes.len() {
            return Err("повреждённая готовая модель: неожиданный конец файла".to_owned());
        }
        let value = &self.bytes[self.offset..end];
        self.offset = end;
        Ok(value)
    }

    fn u8(&mut self) -> Result<u8, String> {
        Ok(self.take(1)?[0])
    }

    fn bool(&mut self) -> Result<bool, String> {
        match self.u8()? {
            0 => Ok(false),
            1 => Ok(true),
            _ => Err("повреждённая готовая модель: неверное логическое значение".to_owned()),
        }
    }

    fn u32(&mut self) -> Result<u32, String> {
        Ok(u32::from_le_bytes(
            self.take(4)?.try_into().expect("fixed slice"),
        ))
    }

    fn f32(&mut self) -> Result<f32, String> {
        Ok(f32::from_bits(self.u32()?))
    }

    fn index(&mut self) -> Result<usize, String> {
        usize::try_from(self.u32()?)
            .map_err(|_| "повреждённая готовая модель: индекс слишком велик".to_owned())
    }

    fn count(&mut self, item_bytes: usize, what: &str) -> Result<usize, String> {
        let count = self.index()?;
        if item_bytes == 0 || count > self.remaining() / item_bytes {
            return Err(format!(
                "повреждённая готовая модель: счётчик {what} превышает оставшиеся байты"
            ));
        }
        Ok(count)
    }

    fn reserve(&mut self, count: usize, element_bytes: usize) -> Result<(), String> {
        let bytes = count.checked_mul(element_bytes).ok_or_else(|| {
            "повреждённая готовая модель: переполнение размера аллокации".to_owned()
        })?;
        let total = self.allocation_bytes.checked_add(bytes).ok_or_else(|| {
            "повреждённая готовая модель: переполнение бюджета аллокаций".to_owned()
        })?;
        if total > self.allocation_limit {
            return Err("готовая модель превышает предел памяти декодера 256 MiB".to_owned());
        }
        self.allocation_bytes = total;
        Ok(())
    }

    fn vector<T>(&mut self, count: usize) -> Result<Vec<T>, String> {
        self.reserve(count, size_of::<T>())?;
        let mut result = Vec::new();
        result
            .try_reserve_exact(count)
            .map_err(|_| "недостаточно памяти для готовой модели".to_owned())?;
        Ok(result)
    }

    fn byte_vector(&mut self, count: usize) -> Result<Vec<u8>, String> {
        self.reserve(count, size_of::<u8>())?;
        let bytes = self.take(count)?;
        let mut result = Vec::new();
        result
            .try_reserve_exact(count)
            .map_err(|_| "недостаточно памяти для готовой модели".to_owned())?;
        result.extend_from_slice(bytes);
        Ok(result)
    }

    fn string(&mut self) -> Result<String, String> {
        let length = self.count(1, "байтов строки")?;
        self.reserve(length, size_of::<u8>())?;
        let value = std::str::from_utf8(self.take(length)?)
            .map_err(|_| "повреждённая готовая модель: строка не является UTF-8".to_owned())?;
        let mut string = String::new();
        string
            .try_reserve_exact(length)
            .map_err(|_| "недостаточно памяти для готовой модели".to_owned())?;
        string.push_str(value);
        Ok(string)
    }

    fn option_index(&mut self) -> Result<Option<usize>, String> {
        if self.bool()? {
            Ok(Some(self.index()?))
        } else {
            Ok(None)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        MaterialRecipe, PREBUILT_MODEL_MAX_BYTES, PreparedMesh, PreparedModel, TextureReference,
        decode, decode_with_limits, encode, is_prebuilt_model,
    };
    use crate::ModelVertex;

    fn skeleton_node(
        name: &str,
        parent: Option<usize>,
        inherit_scale_node: Option<usize>,
    ) -> crate::ModelNode {
        crate::ModelNode {
            name: name.to_owned(),
            parent,
            inherit_mode: crate::ModelInheritMode::Normal,
            inherit_scale_node,
            rest: crate::ModelTransform {
                translation: [0.0; 3],
                rotation: [0.0, 0.0, 0.0, 1.0],
                scale: [1.0; 3],
            },
        }
    }

    fn model() -> PreparedModel {
        PreparedModel {
            texture_references: vec![TextureReference::Embedded {
                name: "texture.png".to_owned(),
                png: vec![1, 2, 3],
            }],
            meshes: Some(vec![PreparedMesh {
                vertices: vec![ModelVertex {
                    position: [1.0, 2.0, 3.0],
                    normal: [0.0, 1.0, 0.0],
                    uv: [0.0, 1.0],
                    tangent: [1.0, 0.0, 0.0, 1.0],
                }],
                indices: vec![0, 0, 0],
                skin: None,
                material: MaterialRecipe {
                    base_color: [1.0; 3],
                    metallic: 0.0,
                    roughness: 1.0,
                    emissive: [0.0; 3],
                    opacity: 1.0,
                    double_sided: false,
                    occlusion_strength: 1.0,
                    base_color_source: Some(0),
                    normal_source: None,
                    roughness_source: None,
                    metalness_source: None,
                    emissive_source: None,
                    occlusion_source: None,
                    opacity_source: None,
                },
            }]),
            skeleton: None,
            model_animations: Vec::new(),
            has_animation_stacks: false,
            clips: Vec::new(),
        }
    }

    #[test]
    fn round_trips_le_fields_and_float_bits() {
        let input = model();
        let bytes = encode(&input).unwrap();
        assert!(is_prebuilt_model(&bytes));
        assert_eq!(decode(&bytes).unwrap(), input);
        assert_eq!(
            PREBUILT_MODEL_MAX_BYTES as u64,
            engine_resources::ResourceConfig::default().max_result_bytes
        );
    }

    #[test]
    fn rejects_wrong_magic_stale_version_truncation_and_trailing_bytes() {
        let bytes = encode(&model()).unwrap();
        let mut foreign = bytes.clone();
        foreign[0] = b'X';
        assert!(decode(&foreign).unwrap_err().contains("сигнатура"));
        let mut stale = bytes.clone();
        stale[8..12].copy_from_slice(&0_u32.to_le_bytes());
        assert!(decode(&stale).unwrap_err().contains("формата v0"));
        assert!(decode(&bytes[..bytes.len() - 1]).is_err());
        let mut trailing = bytes;
        trailing.push(0);
        assert!(decode(&trailing).unwrap_err().contains("лишние байты"));
    }

    #[test]
    fn rejects_oversized_input_before_parsing() {
        let bytes = encode(&model()).unwrap();
        assert!(
            decode_with_limits(&bytes, bytes.len() - 1, PREBUILT_MODEL_MAX_BYTES)
                .unwrap_err()
                .contains("предел")
        );
    }

    #[test]
    fn rejects_every_truncated_prefix_and_counts_that_exceed_remaining_bytes() {
        let bytes = encode(&model()).unwrap();
        for end in 0..bytes.len() {
            assert!(
                decode(&bytes[..end]).is_err(),
                "accepted prefix length {end}"
            );
        }
        let mut huge_count = bytes;
        // Magic + version + bounds flag and six f32 bounds fields.
        huge_count[37..41].copy_from_slice(&u32::MAX.to_le_bytes());
        assert!(decode(&huge_count).unwrap_err().contains("счётчик"));
    }

    #[test]
    fn enforces_a_reduced_allocation_budget_before_reserving_vectors() {
        let bytes = encode(&model()).unwrap();
        assert!(
            decode_with_limits(&bytes, PREBUILT_MODEL_MAX_BYTES, 1)
                .unwrap_err()
                .contains("предел памяти декодера")
        );
    }

    #[test]
    fn rejects_invalid_indices_skin_links_skeleton_order_and_material_sources() {
        let mut invalid = model();
        invalid.meshes.as_mut().unwrap()[0].indices[0] = 1;
        assert!(
            decode(&encode(&invalid).unwrap())
                .unwrap_err()
                .contains("индекс вершины")
        );

        let mut invalid = model();
        invalid.meshes.as_mut().unwrap()[0]
            .material
            .base_color_source = Some(1);
        assert!(
            decode(&encode(&invalid).unwrap())
                .unwrap_err()
                .contains("текстурной ссылки")
        );

        let mut invalid = model();
        invalid.meshes.as_mut().unwrap()[0].skin = Some(crate::ModelSkin {
            joints: Vec::new(),
            weights: vec![[1.0, 0.0, 0.0, 0.0]],
        });
        assert!(
            decode(&encode(&invalid).unwrap())
                .unwrap_err()
                .contains("числом вершин")
        );

        let mut invalid = model();
        invalid.meshes.as_mut().unwrap()[0].skin = Some(crate::ModelSkin {
            joints: vec![[0; 4]],
            weights: vec![[1.0, 0.0, 0.0, 0.0]],
        });
        assert!(
            decode(&encode(&invalid).unwrap())
                .unwrap_err()
                .contains("требует скелет")
        );

        let mut invalid = model();
        invalid.skeleton = Some(crate::ModelSkeleton {
            nodes: vec![skeleton_node("root", Some(0), None)],
            joints: vec![crate::ModelJoint {
                node: 0,
                inverse_bind: [[0.0; 4]; 4],
            }],
        });
        assert!(
            decode(&encode(&invalid).unwrap())
                .unwrap_err()
                .contains("родитель узла")
        );

        let mut invalid = model();
        invalid.skeleton = Some(crate::ModelSkeleton {
            nodes: vec![
                skeleton_node("root", None, None),
                skeleton_node("child", Some(0), Some(1)),
            ],
            joints: Vec::new(),
        });
        assert!(
            decode(&encode(&invalid).unwrap())
                .unwrap_err()
                .contains("родитель узла")
        );

        let mut invalid = model();
        invalid.skeleton = Some(crate::ModelSkeleton {
            nodes: vec![skeleton_node("root", None, None)],
            joints: vec![crate::ModelJoint {
                node: 1,
                inverse_bind: [[0.0; 4]; 4],
            }],
        });
        assert!(
            decode(&encode(&invalid).unwrap())
                .unwrap_err()
                .contains("узла сустава")
        );

        let mut invalid = model();
        invalid.skeleton = Some(crate::ModelSkeleton {
            nodes: vec![skeleton_node("root", None, None)],
            joints: vec![crate::ModelJoint {
                node: 0,
                inverse_bind: [[0.0; 4]; 4],
            }],
        });
        invalid.meshes.as_mut().unwrap()[0].skin = Some(crate::ModelSkin {
            joints: vec![[1, 0, 0, 0]],
            weights: vec![[1.0, 0.0, 0.0, 0.0]],
        });
        assert!(
            decode(&encode(&invalid).unwrap())
                .unwrap_err()
                .contains("индекс сустава skin")
        );
    }

    #[test]
    fn rejects_header_bounds_that_do_not_match_model_vertices() {
        let mut bytes = encode(&model()).unwrap();
        // First bound value starts after magic, version and the bounds presence byte.
        bytes[13] ^= 1;
        assert!(decode(&bytes).unwrap_err().contains("границы модели"));
    }
}
