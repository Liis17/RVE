//! GPU-skinning скелетных мешей.
//!
//! Compute-проход `skin.comp` переводит вершины меша в пространство модели до теней и
//! основной сцены, после чего mesh-пайплайны рисуют выходные буферы как обычные вершины.
//! Для каждого экземпляра есть кольцо из [`SKIN_RING`] выходных копий: кадр пишет следующую,
//! а предыдущая нужна только для motion vectors. Три копии нужны, потому что кадры в полёте
//! (`FRAMES_IN_FLIGHT`) могут ещё читать предыдущую копию, когда кадр начинает запись.
//! Палитры кадра копируются в host-visible буфер слота, поэтому кадры в полёте не пересекаются.

use crate::{
    DeviceBuffer, FRAMES_IN_FLIGHT, Frame, HostBuffer, MeshId, OwnedDescriptorPool, RenderError,
    Renderer, RetiredResource, Vertex,
};
use ash::vk;

/// Число выходных копий экземпляра: текущая, предыдущая и одна в запасе.
pub(crate) const SKIN_RING: usize = 3;
/// Размер workgroup `skin.comp`; совпадает с `local_size_x` шейдера.
pub(crate) const SKIN_COMPUTE_LOCAL_SIZE: u32 = 64;
/// Выравнивание области палитры в буфере слота; смещения дескрипторов кратны ему.
const PALETTE_ALIGNMENT: usize = 256;
/// Размер одной матрицы палитры в байтах.
const MATRIX_BYTES: usize = size_of::<[[f32; 4]; 4]>();
/// Размер вершины `Vertex` в буфере: позиция, нормаль, uv и цвет.
const VERTEX_BYTES: usize = size_of::<Vertex>();

/// Вершина skinning: до четырёх костей и их весов. Веса конечны и неотрицательны.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct SkinVertex {
    pub joints: [u32; 4],
    pub weights: [f32; 4],
}

/// Экземпляр skinning одного меша. Выходные вершины живут отдельно от исходного меша.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct SkinId(pub(crate) usize);

/// Привязка draw к экземпляру skinning и палитре кадра.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SkinBinding {
    pub instance: SkinId,
    /// Индекс в [`Frame::joint_palettes`].
    pub palette: u32,
}

/// Буферы вершин для одного draw: текущая и предыдущая позиции.
#[derive(Clone, Copy, Debug)]
pub(crate) struct SkinDrawBuffers {
    pub current: vk::Buffer,
    pub previous: vk::Buffer,
}

/// Исходные данные skinning, которые живут вместе с мешем.
pub(crate) struct SkinnedMesh {
    pub skin: HostBuffer,
    pub joint_count: u32,
    pub vertex_count: u32,
}

/// Состояние экземпляра: выходные буферы, дескрипторы по слотам и история кольца.
pub(crate) struct SkinInstance {
    pub mesh: MeshId,
    pub outputs: [DeviceBuffer; SKIN_RING],
    pub vertex_count: u32,
    /// Владеет наборами `sets`; читается только при освобождении экземпляра.
    #[allow(dead_code)]
    pub pool: OwnedDescriptorPool,
    pub sets: [vk::DescriptorSet; FRAMES_IN_FLIGHT],
    pub history: SkinHistory,
}

/// История выходных копий одного экземпляра.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct SkinHistory {
    /// Индекс копии, записанной последним отправленным вызовом.
    last_written: Option<usize>,
    /// Серийный номер отправленного вызова, который обновил экземпляр.
    dispatched_in: Option<u64>,
}

/// План записи экземпляра в текущем вызове.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct SkinStep {
    /// Копия, в которую пишет compute.
    pub write: usize,
    /// Копия с позой прошлого кадра; `None` — предыдущая поза равна текущей.
    pub previous: Option<usize>,
}

impl SkinHistory {
    /// Выбирает копии для вызова. Предыдущая копия действительна, только если экземпляр
    /// обновлялся в непосредственно предыдущем отправленном вызове `last_submitted_call`.
    pub(crate) fn plan(&self, last_submitted_call: u64, reset_history: bool) -> SkinStep {
        let write = self.last_written.map_or(0, |index| (index + 1) % SKIN_RING);
        let previous = match (self.last_written, self.dispatched_in) {
            (Some(index), Some(serial)) if !reset_history && serial == last_submitted_call => {
                Some(index)
            }
            _ => None,
        };
        SkinStep { write, previous }
    }

    /// Фиксирует отправленный вызов. Неотправленный вызов состояние не меняет.
    pub(crate) fn commit(&mut self, step: SkinStep, call: u64) {
        self.last_written = Some(step.write);
        self.dispatched_in = Some(call);
    }
}

/// Один compute-dispatch на экземпляр за вызов.
pub(crate) struct SkinDispatch {
    pub instance: SkinId,
    pub step: SkinStep,
    pub vertex_count: u32,
    pub set: vk::DescriptorSet,
}

/// План skinning вызова: dispatch на каждый экземпляр и буферы draw'ов каждой сцены.
pub(crate) struct SkinPlan {
    pub call: u64,
    pub dispatches: Vec<SkinDispatch>,
    /// Для каждой сцены вызова и каждого её draw буферы вершин; `None` у статических draw'ов.
    pub draw_buffers: Vec<Vec<Option<SkinDrawBuffers>>>,
}

/// Смещения палитр в буфере слота и общий размер; каждое смещение кратно `PALETTE_ALIGNMENT`.
pub(crate) fn palette_offsets(lengths: &[usize]) -> (Vec<usize>, usize) {
    let mut cursor = 0;
    let offsets = lengths
        .iter()
        .map(|&length| {
            let offset = cursor;
            cursor = (offset + length * MATRIX_BYTES).next_multiple_of(PALETTE_ALIGNMENT);
            offset
        })
        .collect();
    (offsets, cursor)
}

/// Проверяет данные skinning меша до загрузки.
pub(crate) fn validate_skin_vertices(
    vertex_count: usize,
    skin: &[SkinVertex],
    joint_count: u32,
) -> Result<(), RenderError> {
    if skin.len() != vertex_count {
        return Err(RenderError::InvalidMesh(
            "число записей skin не совпадает с числом вершин",
        ));
    }
    if joint_count == 0 {
        return Err(RenderError::InvalidMesh("у skinned-меша нет костей"));
    }
    for vertex in skin {
        if vertex.joints.iter().any(|&joint| joint >= joint_count) {
            return Err(RenderError::InvalidMesh(
                "индекс кости выходит за пределы joint_count",
            ));
        }
        if vertex
            .weights
            .iter()
            .any(|weight| !weight.is_finite() || *weight < 0.0)
        {
            return Err(RenderError::InvalidMesh(
                "веса skinning должны быть конечными и неотрицательными",
            ));
        }
    }
    Ok(())
}

/// Проверяет палитру кадра: длина равна числу костей, матрицы конечны.
pub(crate) fn validate_palette(
    palette: &[[[f32; 4]; 4]],
    joint_count: u32,
) -> Result<(), RenderError> {
    if palette.len() != joint_count as usize {
        return Err(RenderError::InvalidConfig(
            "длина палитры не совпадает с числом костей меша",
        ));
    }
    if palette
        .iter()
        .any(|matrix| matrix.iter().flatten().any(|value| !value.is_finite()))
    {
        return Err(RenderError::InvalidConfig(
            "матрицы палитры должны быть конечными",
        ));
    }
    Ok(())
}

/// Палитра кадра: матрицы костей по столбцам.
type Palette<'p> = &'p [[[f32; 4]; 4]];

/// Уникальные экземпляры вызова с их палитрой в порядке первого использования.
///
/// Один экземпляр в одном вызове получает один dispatch, даже если его используют несколько
/// сцен. Палитры для него должны совпадать по содержимому, иначе ошибка: выходные вершины
/// одни на вызов.
pub(crate) fn unique_skin_dispatches<'p>(
    bindings: &[(SkinId, Palette<'p>)],
) -> Result<Vec<(SkinId, Palette<'p>)>, RenderError> {
    let mut unique: Vec<(SkinId, Palette<'p>)> = Vec::new();
    for &(instance, palette) in bindings {
        let Some(&(_, first)) = unique.iter().find(|(id, _)| *id == instance) else {
            unique.push((instance, palette));
            continue;
        };
        if first != palette {
            return Err(RenderError::InvalidConfig(
                "разные палитры для одного SkinId в одном вызове",
            ));
        }
    }
    Ok(unique)
}

impl Renderer {
    /// Создаёт меш с весами костей. Исходные вершины читает compute, а рисуются экземпляры.
    pub fn create_skinned_mesh(
        &mut self,
        vertices: &[Vertex],
        skin: &[SkinVertex],
        indices: &[u32],
        joint_count: u32,
    ) -> Result<MeshId, RenderError> {
        validate_skin_vertices(vertices.len(), skin, joint_count)?;
        let skin_bytes = skin_bytes(skin);
        let mesh = self.create_mesh_with_skin(vertices, indices)?;
        let instance = self.instance()?.clone();
        let device = self.device()?.clone();
        let physical_device = self.physical_device;
        let buffer = HostBuffer::new(
            &instance,
            &device,
            physical_device,
            vk::BufferUsageFlags::STORAGE_BUFFER,
            &skin_bytes,
        )?;
        let buffers = self
            .meshes
            .get_mut(mesh.0)
            .and_then(Option::as_mut)
            .ok_or(RenderError::InvalidState("созданный меш отсутствует"))?;
        buffers.skin = Some(SkinnedMesh {
            skin: buffer,
            joint_count,
            vertex_count: vertices.len() as u32,
        });
        Ok(mesh)
    }

    /// Создаёт экземпляр skinning для меша, созданного через [`Renderer::create_skinned_mesh`].
    pub fn create_skin_instance(&mut self, mesh: MeshId) -> Result<SkinId, RenderError> {
        let buffers = self
            .meshes
            .get(mesh.0)
            .and_then(Option::as_ref)
            .ok_or(RenderError::InvalidMesh("MeshId не принадлежит renderer"))?;
        let skinned = buffers
            .skin
            .as_ref()
            .ok_or(RenderError::InvalidMesh("меш создан без skinning"))?;
        let vertex_count = skinned.vertex_count;
        let output_size = vertex_count as usize * VERTEX_BYTES;
        let instance = self.instance()?.clone();
        let device = self.device()?.clone();
        let physical_device = self.physical_device;
        let outputs = [
            DeviceBuffer::new(
                &instance,
                &device,
                physical_device,
                vk::BufferUsageFlags::VERTEX_BUFFER | vk::BufferUsageFlags::STORAGE_BUFFER,
                output_size,
            )?,
            DeviceBuffer::new(
                &instance,
                &device,
                physical_device,
                vk::BufferUsageFlags::VERTEX_BUFFER | vk::BufferUsageFlags::STORAGE_BUFFER,
                output_size,
            )?,
            DeviceBuffer::new(
                &instance,
                &device,
                physical_device,
                vk::BufferUsageFlags::VERTEX_BUFFER | vk::BufferUsageFlags::STORAGE_BUFFER,
                output_size,
            )?,
        ];
        let pool_sizes = [vk::DescriptorPoolSize::default()
            .ty(vk::DescriptorType::STORAGE_BUFFER)
            .descriptor_count(4 * FRAMES_IN_FLIGHT as u32)];
        let pool_info = vk::DescriptorPoolCreateInfo::default()
            .max_sets(FRAMES_IN_FLIGHT as u32)
            .pool_sizes(&pool_sizes);
        let pool = OwnedDescriptorPool::new(&device, &pool_info)?;
        let layouts = [self.skin_set_layout.ok_or(RenderError::InvalidState(
            "descriptor layout skinning отсутствует",
        ))?; FRAMES_IN_FLIGHT];
        let allocate = vk::DescriptorSetAllocateInfo::default()
            .descriptor_pool(pool.handle)
            .set_layouts(&layouts);
        // SAFETY: the pool was sized for FRAMES_IN_FLIGHT sets of this layout.
        let sets = unsafe { device.allocate_descriptor_sets(&allocate) }?;
        let sets: [vk::DescriptorSet; FRAMES_IN_FLIGHT] = sets
            .try_into()
            .map_err(|_| RenderError::InvalidState("число наборов skinning не совпало"))?;
        let instance_state = SkinInstance {
            mesh,
            outputs,
            vertex_count,
            pool,
            sets,
            history: SkinHistory::default(),
        };
        let slot = self
            .skin_instances
            .iter()
            .position(Option::is_none)
            .unwrap_or_else(|| {
                self.skin_instances.push(None);
                self.skin_instances.len() - 1
            });
        self.skin_instances[slot] = Some(instance_state);
        Ok(SkinId(slot))
    }

    /// Освобождает экземпляр после того, как кадры, которые его читают, завершатся.
    pub fn destroy_skin_instance(&mut self, skin: SkinId) -> Result<(), RenderError> {
        let instance = self
            .skin_instances
            .get_mut(skin.0)
            .and_then(Option::take)
            .ok_or(RenderError::InvalidMesh("SkinId не принадлежит renderer"))?;
        let upload_serial = self
            .meshes
            .get(instance.mesh.0)
            .and_then(Option::as_ref)
            .map_or(0, |buffers| buffers.upload_serial);
        self.retired.retire(
            RetiredResource::SkinInstance(Box::new(instance)),
            upload_serial,
        );
        Ok(())
    }

    /// Проверяет skinned draw'ы вызова до начала кадра.
    pub(crate) fn validate_skin_draws(&self, frame: &Frame<'_>) -> Result<(), RenderError> {
        for draw in frame.draws {
            let Some(binding) = draw.skin else {
                continue;
            };
            let instance = self
                .skin_instances
                .get(binding.instance.0)
                .and_then(Option::as_ref)
                .ok_or(RenderError::InvalidMesh("SkinId не принадлежит renderer"))?;
            if instance.mesh != draw.mesh {
                return Err(RenderError::InvalidMesh(
                    "skin instance принадлежит другому мешу",
                ));
            }
            let palette = frame
                .joint_palettes
                .get(binding.palette as usize)
                .ok_or(RenderError::InvalidConfig("индекс палитры вне frame"))?;
            let joint_count = self
                .meshes
                .get(draw.mesh.0)
                .and_then(Option::as_ref)
                .and_then(|mesh| mesh.skin.as_ref())
                .map_or(0, |skin| skin.joint_count);
            validate_palette(palette, joint_count)?;
        }
        Ok(())
    }

    /// Готовит skinning вызова: палитры в буфер слота, наборы дескрипторов и кольцо копий.
    ///
    /// `scenes` — кадры вызова в порядке рисования. Состояние экземпляров не меняется до
    /// [`Renderer::commit_skin`] после успешной отправки.
    pub(crate) fn plan_skin(
        &mut self,
        scenes: &[&Frame<'_>],
        slot: usize,
    ) -> Result<Option<SkinPlan>, RenderError> {
        let call = self.skin_call_serial + 1;
        let mut bindings: Vec<(SkinId, Palette<'_>)> = Vec::new();
        for frame in scenes {
            for draw in frame.draws {
                if let Some(binding) = draw.skin {
                    let palette = frame
                        .joint_palettes
                        .get(binding.palette as usize)
                        .ok_or(RenderError::InvalidConfig("индекс палитры вне frame"))?;
                    bindings.push((binding.instance, palette));
                }
            }
        }
        if bindings.is_empty() {
            return Ok(None);
        }
        let unique = unique_skin_dispatches(&bindings)?;
        let lengths: Vec<usize> = unique.iter().map(|(_, palette)| palette.len()).collect();
        let (offsets, total) = palette_offsets(&lengths);
        let mut bytes = vec![0_u8; total];
        for ((_, palette), &offset) in unique.iter().zip(&offsets) {
            let source = palette_bytes(palette);
            bytes[offset..offset + source.len()].copy_from_slice(&source);
        }
        let instance = self.instance()?.clone();
        let device = self.device()?.clone();
        let physical_device = self.physical_device;
        self.frames[slot].skin_palettes.write(
            &instance,
            &device,
            physical_device,
            vk::BufferUsageFlags::STORAGE_BUFFER,
            &bytes,
        )?;
        let palette_buffer = self.frames[slot]
            .skin_palettes
            .buffer
            .as_ref()
            .map(|buffer| buffer.buffer)
            .ok_or(RenderError::InvalidState("буфер палитр кадра отсутствует"))?;
        let reset_history = scenes.iter().any(|frame| frame.reset_history);

        let mut dispatches = Vec::with_capacity(unique.len());
        for (index, &(skin, _)) in unique.iter().enumerate() {
            let state = self
                .skin_instances
                .get(skin.0)
                .and_then(Option::as_ref)
                .ok_or(RenderError::InvalidMesh("SkinId не принадлежит renderer"))?;
            let mesh = self
                .meshes
                .get(state.mesh.0)
                .and_then(Option::as_ref)
                .ok_or(RenderError::InvalidMesh("MeshId не принадлежит renderer"))?;
            let source = mesh.vertex_buffer.buffer;
            let skin_buffer = mesh
                .skin
                .as_ref()
                .map(|skin| skin.skin.buffer)
                .ok_or(RenderError::InvalidMesh("меш создан без skinning"))?;
            let step = state.history.plan(self.skin_call_serial, reset_history);
            let set = state.sets[slot];
            let palette_range = lengths[index] * MATRIX_BYTES;
            let infos = [
                vk::DescriptorBufferInfo::default()
                    .buffer(source)
                    .offset(0)
                    .range(vk::WHOLE_SIZE),
                vk::DescriptorBufferInfo::default()
                    .buffer(skin_buffer)
                    .offset(0)
                    .range(vk::WHOLE_SIZE),
                vk::DescriptorBufferInfo::default()
                    .buffer(palette_buffer)
                    .offset(offsets[index] as vk::DeviceSize)
                    .range(palette_range as vk::DeviceSize),
                vk::DescriptorBufferInfo::default()
                    .buffer(state.outputs[step.write].buffer)
                    .offset(0)
                    .range(vk::WHOLE_SIZE),
            ];
            let writes: Vec<vk::WriteDescriptorSet<'_>> = infos
                .iter()
                .enumerate()
                .map(|(binding, info)| {
                    vk::WriteDescriptorSet::default()
                        .dst_set(set)
                        .dst_binding(binding as u32)
                        .descriptor_type(vk::DescriptorType::STORAGE_BUFFER)
                        .buffer_info(std::slice::from_ref(info))
                })
                .collect();
            // SAFETY: the set belongs to this instance's pool and this frame slot, whose
            // previous submission finished before this call; every buffer is live.
            unsafe { device.update_descriptor_sets(&writes, &[]) };
            dispatches.push(SkinDispatch {
                instance: skin,
                step,
                vertex_count: state.vertex_count,
                set,
            });
        }

        let draw_buffers = scenes
            .iter()
            .map(|frame| {
                frame
                    .draws
                    .iter()
                    .map(|draw| {
                        let binding = draw.skin?;
                        let dispatch = dispatches
                            .iter()
                            .find(|dispatch| dispatch.instance == binding.instance)?;
                        let state = self.skin_instances.get(binding.instance.0)?.as_ref()?;
                        let current = state.outputs[dispatch.step.write].buffer;
                        let previous = dispatch
                            .step
                            .previous
                            .map_or(current, |index| state.outputs[index].buffer);
                        Some(SkinDrawBuffers { current, previous })
                    })
                    .collect()
            })
            .collect();
        Ok(Some(SkinPlan {
            call,
            dispatches,
            draw_buffers,
        }))
    }

    /// Фиксирует кольцо и номер вызова после успешной отправки.
    pub(crate) fn commit_skin(&mut self, plan: &SkinPlan) {
        for dispatch in &plan.dispatches {
            if let Some(state) = self
                .skin_instances
                .get_mut(dispatch.instance.0)
                .and_then(Option::as_mut)
            {
                state.history.commit(dispatch.step, plan.call);
            }
        }
        self.skin_call_serial = plan.call;
    }

    /// Запись skinning доступна через [`record_skin`]; здесь — выбор пайплайна.
    pub(crate) fn skin_record<'a>(&self, plan: Option<&'a SkinPlan>) -> Option<SkinRecord<'a>> {
        let plan = plan?;
        Some(SkinRecord {
            plan,
            pipeline: self.skin_pipeline?,
            layout: self.skin_pipeline_layout?,
        })
    }
}

/// Пайплайн и план skinning одного вызова для записи команд.
#[derive(Clone, Copy)]
pub(crate) struct SkinRecord<'a> {
    pub plan: &'a SkinPlan,
    pub pipeline: vk::Pipeline,
    pub layout: vk::PipelineLayout,
}

/// Буферы вершин skinned draw'ов сцены по её индексу в вызове; пусто, если skinning нет.
pub(crate) fn skin_draws_for(plan: Option<&SkinPlan>, scene: usize) -> &[Option<SkinDrawBuffers>] {
    plan.and_then(|plan| plan.draw_buffers.get(scene))
        .map_or(&[], Vec::as_slice)
}

/// Записывает compute-dispatch'и skinning до теней и сцены. Барьеры отделяют запись
/// палитр хостом, чтение вершин из прошлых кадров и публикацию выхода вершинному входу.
pub(crate) fn record_skin(
    device: &ash::Device,
    command_buffer: vk::CommandBuffer,
    record: SkinRecord<'_>,
) {
    let SkinRecord {
        plan,
        pipeline,
        layout,
    } = record;
    let host_barrier = vk::MemoryBarrier2::default()
        .src_stage_mask(vk::PipelineStageFlags2::HOST)
        .src_access_mask(vk::AccessFlags2::HOST_WRITE)
        .dst_stage_mask(vk::PipelineStageFlags2::COMPUTE_SHADER)
        .dst_access_mask(vk::AccessFlags2::SHADER_STORAGE_READ);
    let war_barrier = vk::MemoryBarrier2::default()
        .src_stage_mask(
            vk::PipelineStageFlags2::VERTEX_ATTRIBUTE_INPUT
                | vk::PipelineStageFlags2::VERTEX_SHADER,
        )
        .src_access_mask(
            vk::AccessFlags2::VERTEX_ATTRIBUTE_READ | vk::AccessFlags2::SHADER_STORAGE_READ,
        )
        .dst_stage_mask(vk::PipelineStageFlags2::COMPUTE_SHADER)
        .dst_access_mask(vk::AccessFlags2::SHADER_STORAGE_WRITE);
    let memory_barriers = [host_barrier, war_barrier];
    let dependency = vk::DependencyInfo::default().memory_barriers(&memory_barriers);
    // SAFETY: the barriers reference this frame's submission state; dispatches use live buffers.
    unsafe {
        device.cmd_pipeline_barrier2(command_buffer, &dependency);
        device.cmd_bind_pipeline(command_buffer, vk::PipelineBindPoint::COMPUTE, pipeline);
    }
    for dispatch in &plan.dispatches {
        let groups = dispatch.vertex_count.div_ceil(SKIN_COMPUTE_LOCAL_SIZE);
        // SAFETY: the set and layout match the skin pipeline; the push constant is one u32.
        unsafe {
            device.cmd_bind_descriptor_sets(
                command_buffer,
                vk::PipelineBindPoint::COMPUTE,
                layout,
                0,
                &[dispatch.set],
                &[],
            );
            device.cmd_push_constants(
                command_buffer,
                layout,
                vk::ShaderStageFlags::COMPUTE,
                0,
                &dispatch.vertex_count.to_ne_bytes(),
            );
            device.cmd_dispatch(command_buffer, groups, 1, 1);
        }
    }
    let publish_barrier = vk::MemoryBarrier2::default()
        .src_stage_mask(vk::PipelineStageFlags2::COMPUTE_SHADER)
        .src_access_mask(vk::AccessFlags2::SHADER_STORAGE_WRITE)
        .dst_stage_mask(
            vk::PipelineStageFlags2::VERTEX_ATTRIBUTE_INPUT
                | vk::PipelineStageFlags2::VERTEX_SHADER,
        )
        .dst_access_mask(
            vk::AccessFlags2::VERTEX_ATTRIBUTE_READ | vk::AccessFlags2::SHADER_STORAGE_READ,
        );
    let publish = [publish_barrier];
    let dependency = vk::DependencyInfo::default().memory_barriers(&publish);
    // SAFETY: the barrier orders compute writes before vertex fetches in this submission.
    unsafe { device.cmd_pipeline_barrier2(command_buffer, &dependency) };
}

/// Байты skinning-вершин для буфера: четыре `uint` и четыре `float` на вершину.
fn skin_bytes(skin: &[SkinVertex]) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(skin.len() * 32);
    for vertex in skin {
        for joint in vertex.joints {
            bytes.extend_from_slice(&joint.to_ne_bytes());
        }
        for weight in vertex.weights {
            bytes.extend_from_slice(&weight.to_ne_bytes());
        }
    }
    bytes
}

/// Байты матриц палитры в порядке столбцов, как их читает `mat4` в std430.
fn palette_bytes(palette: &[[[f32; 4]; 4]]) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(palette.len() * MATRIX_BYTES);
    for matrix in palette {
        for column in matrix {
            for value in column {
                bytes.extend_from_slice(&value.to_ne_bytes());
            }
        }
    }
    bytes
}

#[cfg(test)]
mod tests {
    use super::*;

    fn matrix(value: f32) -> [[f32; 4]; 4] {
        [[value; 4]; 4]
    }

    #[test]
    fn ring_writes_advance_and_previous_needs_the_last_submitted_call() {
        let mut history = SkinHistory::default();
        let first = history.plan(0, false);
        assert_eq!(
            first,
            SkinStep {
                write: 0,
                previous: None
            }
        );
        history.commit(first, 1);

        let second = history.plan(1, false);
        assert_eq!(
            second,
            SkinStep {
                write: 1,
                previous: Some(0)
            }
        );
        history.commit(second, 2);

        let third = history.plan(2, false);
        assert_eq!(third.write, 2);
        assert_eq!(third.previous, Some(1));
        assert_ne!(third.write, third.previous.unwrap());
    }

    #[test]
    fn skipped_calls_do_not_change_the_history() {
        let mut history = SkinHistory::default();
        let step = history.plan(0, false);
        // Вызов не отправлен: commit не происходит, состояние остаётся прежним.
        assert_eq!(history.plan(0, false), step);
        history.commit(step, 1);
        // Следующий отправленный вызов видит предыдущий как отправленный сразу перед ним.
        assert_eq!(history.plan(1, false).previous, Some(0));
    }

    #[test]
    fn previous_is_current_after_a_gap_or_a_history_reset() {
        let mut history = SkinHistory::default();
        history.commit(history.plan(0, false), 1);
        // Между вызовами были другие отправки, экземпляр не обновлялся.
        assert_eq!(history.plan(5, false).previous, None);
        // Сброс истории отбрасывает предыдущую позу, но кольцо продолжает двигаться.
        let reset = history.plan(1, true);
        assert_eq!(
            reset,
            SkinStep {
                write: 1,
                previous: None
            }
        );
    }

    #[test]
    fn palettes_start_on_aligned_offsets() {
        // 64 байта одной матрицы, затем 192 и 128 байт; каждая палитра начинается на 256.
        let (offsets, total) = palette_offsets(&[1, 3, 2]);
        assert_eq!(offsets, [0, 256, 512]);
        assert_eq!(total, 768);
        assert!(offsets.iter().all(|offset| offset % PALETTE_ALIGNMENT == 0));
    }

    #[test]
    fn skin_vertices_reject_bad_joints_weights_and_counts() {
        let good = SkinVertex {
            joints: [0, 1, 0, 0],
            weights: [0.5, 0.5, 0.0, 0.0],
        };
        assert!(validate_skin_vertices(1, &[good], 2).is_ok());
        assert!(
            validate_skin_vertices(2, &[good], 2).is_err(),
            "число записей"
        );
        assert!(validate_skin_vertices(1, &[good], 0).is_err(), "нет костей");
        let joint = SkinVertex {
            joints: [2, 0, 0, 0],
            weights: [1.0, 0.0, 0.0, 0.0],
        };
        assert!(
            validate_skin_vertices(1, &[joint], 2).is_err(),
            "joint вне диапазона"
        );
        let negative = SkinVertex {
            joints: [0; 4],
            weights: [-0.1, 0.0, 0.0, 0.0],
        };
        assert!(validate_skin_vertices(1, &[negative], 1).is_err());
        let nan = SkinVertex {
            joints: [0; 4],
            weights: [f32::NAN, 0.0, 0.0, 0.0],
        };
        assert!(validate_skin_vertices(1, &[nan], 1).is_err());
    }

    #[test]
    fn palette_must_match_joint_count_and_be_finite() {
        assert!(validate_palette(&[matrix(1.0), matrix(2.0)], 2).is_ok());
        assert!(validate_palette(&[matrix(1.0)], 2).is_err());
        let mut broken = matrix(1.0);
        broken[3][0] = f32::INFINITY;
        assert!(validate_palette(&[broken], 1).is_err());
    }

    #[test]
    fn one_instance_per_call_reuses_equal_palettes_and_rejects_different_ones() {
        let a = [matrix(1.0)];
        let a_copy = [matrix(1.0)];
        let b = [matrix(2.0)];
        let skin = SkinId(0);
        let other = SkinId(1);
        // Две сцены используют один экземпляр с одинаковой палитрой (даже из разных кадров).
        let shared =
            unique_skin_dispatches(&[(skin, &a[..]), (other, &b[..]), (skin, &a_copy[..])])
                .expect("одинаковые палитры допустимы");
        assert_eq!(shared.len(), 2);
        assert_eq!(shared[0].0, skin);
        assert_eq!(shared[1].0, other);
        assert!(unique_skin_dispatches(&[(skin, &a[..]), (skin, &b[..])]).is_err());
    }

    #[test]
    fn skin_bytes_follow_the_shader_layout() {
        let bytes = skin_bytes(&[SkinVertex {
            joints: [1, 2, 3, 4],
            weights: [0.25; 4],
        }]);
        assert_eq!(bytes.len(), 32);
        assert_eq!(&bytes[..4], &1_u32.to_ne_bytes());
        assert_eq!(&bytes[16..20], &0.25_f32.to_ne_bytes());
    }
}
