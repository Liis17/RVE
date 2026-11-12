//! Пакетная загрузка сеток, текстур и изображений через staging-буферы.

use crate::*;

impl Renderer {
    /// Записывает пиксели через batch staging-буфер в device-local image.
    pub(crate) fn upload_texture(
        &mut self,
        width: u32,
        height: u32,
        rgba: &[u8],
        color_space: TextureColorSpace,
    ) -> Result<Texture, RenderError> {
        let mip_levels = mip_level_count(width, height);
        let format = texture_format(color_space);
        let (image, upload_serial) = self.upload_image(format, mip_levels, width, height, rgba)?;
        Ok(Texture {
            image,
            color_space,
            upload_serial,
        })
    }

    pub(crate) fn upload_image(
        &mut self,
        format: vk::Format,
        mip_levels: u32,
        width: u32,
        height: u32,
        rgba: &[u8],
    ) -> Result<(DeviceImage, u64), RenderError> {
        let instance = self.instance()?.clone();
        let device = self.device()?.clone();
        let layout = upload_data_layout(0, rgba.len(), self.upload_alignment).ok_or(
            RenderError::InvalidTexture("размер данных текстуры переполняет DeviceSize".to_owned()),
        )?;
        let capacity = upload_batch_capacity(layout.1, self.upload_alignment).ok_or(
            RenderError::InvalidTexture("размер данных текстуры переполняет DeviceSize".to_owned()),
        )?;
        let must_flush = self.pending_upload.as_ref().is_some_and(|batch| {
            batch.oversized
                || upload_data_layout(batch.cursor, rgba.len(), self.upload_alignment)
                    .is_none_or(|(_, end)| end > batch.capacity)
        });
        if must_flush {
            self.flush_uploads()?;
        }
        if self.pending_upload.is_none() {
            self.poll_uploads()?;
            let serial = self.next_upload_serial;
            let next_serial = serial
                .checked_add(1)
                .ok_or(RenderError::InvalidState("upload serial исчерпан"))?;
            self.pending_upload = Some(UploadBatch::new(
                &instance,
                &device,
                self.physical_device,
                self.queue_family_index,
                capacity,
                capacity > DEFAULT_UPLOAD_BATCH_CAPACITY,
                serial,
            )?);
            self.next_upload_serial = next_serial;
        }
        let image = DeviceImage::new(
            &instance,
            &device,
            self.physical_device,
            vk::Extent2D { width, height },
            format,
            vk::ImageUsageFlags::SAMPLED
                | vk::ImageUsageFlags::TRANSFER_SRC
                | vk::ImageUsageFlags::TRANSFER_DST,
            color_mip_subresource_range(0, mip_levels),
        )?;
        let batch = self
            .pending_upload
            .as_mut()
            .ok_or(RenderError::InvalidState("upload batch отсутствует"))?;
        let layout = upload_data_layout(batch.cursor, rgba.len(), self.upload_alignment).ok_or(
            RenderError::InvalidTexture("размер данных текстуры переполняет DeviceSize".to_owned()),
        )?;
        batch.staging.write_at(layout.0, rgba)?;
        batch.record_image_upload(image.image, width, height, mip_levels, layout.0);
        batch.cursor = layout.1;
        let upload_serial = batch.serial;
        Ok((image, upload_serial))
    }

    pub(crate) fn upload_16bit_image(
        &self,
        width: u32,
        height: u32,
        format: vk::Format,
        bytes_per_texel: usize,
        pixels: &[u8],
    ) -> Result<DeviceImage, RenderError> {
        let expected = (width as usize)
            .checked_mul(height as usize)
            .and_then(|count| count.checked_mul(bytes_per_texel));
        if width == 0 || height == 0 || expected != Some(pixels.len()) {
            return Err(RenderError::InvalidEnvironment(
                "16-bit image data does not match its dimensions".to_owned(),
            ));
        }
        let device = self.device()?;
        let image = DeviceImage::new(
            self.instance()?,
            device,
            self.physical_device,
            vk::Extent2D { width, height },
            format,
            vk::ImageUsageFlags::SAMPLED | vk::ImageUsageFlags::TRANSFER_DST,
            color_mip_subresource_range(0, 1),
        )?;
        let staging = HostBuffer::new(
            self.instance()?,
            device,
            self.physical_device,
            vk::BufferUsageFlags::TRANSFER_SRC,
            pixels,
        )?;
        self.submit_once(|command_buffer| {
            pipeline_barrier(
                device,
                command_buffer,
                image_layout_barrier(
                    image.image,
                    vk::ImageLayout::UNDEFINED,
                    vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                    vk::PipelineStageFlags2::NONE,
                    vk::AccessFlags2::NONE,
                    vk::PipelineStageFlags2::TRANSFER,
                    vk::AccessFlags2::TRANSFER_WRITE,
                    color_subresource_range(),
                ),
            );
            let region = [vk::BufferImageCopy::default()
                .buffer_offset(0)
                .image_subresource(color_subresource_layers())
                .image_extent(vk::Extent3D {
                    width,
                    height,
                    depth: 1,
                })];
            // SAFETY: staging contains the checked texel byte count and the image is transfer-dst.
            unsafe {
                device.cmd_copy_buffer_to_image(
                    command_buffer,
                    staging.buffer,
                    image.image,
                    vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                    &region,
                )
            };
            pipeline_barrier(
                device,
                command_buffer,
                image_layout_barrier(
                    image.image,
                    vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                    vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL,
                    vk::PipelineStageFlags2::TRANSFER,
                    vk::AccessFlags2::TRANSFER_WRITE,
                    vk::PipelineStageFlags2::COMPUTE_SHADER
                        | vk::PipelineStageFlags2::FRAGMENT_SHADER,
                    vk::AccessFlags2::SHADER_SAMPLED_READ,
                    color_subresource_range(),
                ),
            );
        })?;
        Ok(image)
    }

    pub(crate) fn upload_cube_constant(&self, texel: [u16; 4]) -> Result<DeviceImage, RenderError> {
        let device = self.device()?;
        let image = DeviceImage::new_cube(
            self.instance()?,
            device,
            self.physical_device,
            1,
            OFFSCREEN_FORMAT,
            vk::ImageUsageFlags::SAMPLED | vk::ImageUsageFlags::TRANSFER_DST,
            1,
        )?;
        let mut bytes = Vec::with_capacity(6 * size_of::<[u16; 4]>());
        for _ in 0..6 {
            for channel in texel {
                bytes.extend_from_slice(&channel.to_ne_bytes());
            }
        }
        let staging = HostBuffer::new(
            self.instance()?,
            device,
            self.physical_device,
            vk::BufferUsageFlags::TRANSFER_SRC,
            &bytes,
        )?;
        self.submit_once(|command_buffer| {
            pipeline_barrier(
                device,
                command_buffer,
                image_layout_barrier(
                    image.image,
                    vk::ImageLayout::UNDEFINED,
                    vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                    vk::PipelineStageFlags2::NONE,
                    vk::AccessFlags2::NONE,
                    vk::PipelineStageFlags2::TRANSFER,
                    vk::AccessFlags2::TRANSFER_WRITE,
                    color_mip_subresource_range(0, 1).layer_count(6),
                ),
            );
            let region = [vk::BufferImageCopy::default()
                .buffer_offset(0)
                .image_subresource(color_subresource_layers().layer_count(6))
                .image_extent(vk::Extent3D {
                    width: 1,
                    height: 1,
                    depth: 1,
                })];
            // SAFETY: the staging buffer contains one RGBA16F texel for each cube face.
            unsafe {
                device.cmd_copy_buffer_to_image(
                    command_buffer,
                    staging.buffer,
                    image.image,
                    vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                    &region,
                )
            };
            pipeline_barrier(
                device,
                command_buffer,
                image_layout_barrier(
                    image.image,
                    vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                    vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL,
                    vk::PipelineStageFlags2::TRANSFER,
                    vk::AccessFlags2::TRANSFER_WRITE,
                    vk::PipelineStageFlags2::FRAGMENT_SHADER
                        | vk::PipelineStageFlags2::COMPUTE_SHADER,
                    vk::AccessFlags2::SHADER_SAMPLED_READ,
                    color_mip_subresource_range(0, 1).layer_count(6),
                ),
            );
        })?;
        Ok(image)
    }

    /// Записывает одноразовый command buffer, отправляет его и ждёт завершения.
    pub(crate) fn submit_once(
        &self,
        record: impl FnOnce(vk::CommandBuffer),
    ) -> Result<(), RenderError> {
        let device = self.device()?;
        let command_pool = self.command_pool()?;
        let allocate_info = vk::CommandBufferAllocateInfo::default()
            .command_pool(command_pool)
            .level(vk::CommandBufferLevel::PRIMARY)
            .command_buffer_count(1);
        // SAFETY: the command pool belongs to this device and is used from this thread only.
        let command_buffer = unsafe { device.allocate_command_buffers(&allocate_info) }?[0];
        let result = (|| {
            let begin_info = vk::CommandBufferBeginInfo::default()
                .flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT);
            // SAFETY: the command buffer was just allocated and is in the initial state.
            unsafe { device.begin_command_buffer(command_buffer, &begin_info) }?;
            record(command_buffer);
            // SAFETY: recording started above and all commands have been recorded.
            unsafe { device.end_command_buffer(command_buffer) }?;
            // SAFETY: a default fence create info produces an unsignaled fence.
            let fence = unsafe { device.create_fence(&vk::FenceCreateInfo::default(), None) }?;
            let command_infos = [vk::CommandBufferSubmitInfo::default()
                .command_buffer(command_buffer)
                .device_mask(0)];
            let submit_info = [vk::SubmitInfo2::default().command_buffer_infos(&command_infos)];
            // SAFETY: the queue is used only by this renderer thread and the fence is unsignaled.
            let submitted = unsafe { device.queue_submit2(self.queue, &submit_info, fence) }
                // SAFETY: waiting on a fence passed to a successful submit.
                .and_then(|()| unsafe { device.wait_for_fences(&[fence], true, u64::MAX) });
            // SAFETY: the fence is either unsubmitted or already signaled.
            unsafe { device.destroy_fence(fence, None) };
            submitted.map_err(RenderError::from)
        })();
        if result.is_ok() {
            // SAFETY: the submission has completed, so the command buffer is no longer in use.
            unsafe { device.free_command_buffers(command_pool, &[command_buffer]) };
        }
        result
    }

    pub(crate) fn flush_uploads(&mut self) -> Result<(), RenderError> {
        let device = self.device()?.clone();
        let Some(batch) = self.pending_upload.as_mut() else {
            return Ok(());
        };
        if batch.cursor == 0 {
            self.pending_upload.take();
            return Ok(());
        }
        if batch.recording {
            // SAFETY: this batch's command buffer is recording on the renderer thread and has not been submitted.
            unsafe { device.end_command_buffer(batch.command_buffer) }?;
            batch.recording = false;
        }
        if batch.fence.is_none() {
            // SAFETY: a default fence is unsignaled and belongs to this upload submission.
            batch.fence =
                Some(unsafe { device.create_fence(&vk::FenceCreateInfo::default(), None) }?);
        }
        let command_infos = [vk::CommandBufferSubmitInfo::default()
            .command_buffer(batch.command_buffer)
            .device_mask(0)];
        let submit_info = [vk::SubmitInfo2::default().command_buffer_infos(&command_infos)];
        let fence = batch.fence.ok_or(RenderError::InvalidState(
            "upload fence отсутствует после создания",
        ))?;
        // SAFETY: the executable batch command buffer and unsignaled fence are owned by this renderer.
        unsafe { device.queue_submit2(self.queue, &submit_info, fence) }?;
        let batch = self.pending_upload.take().ok_or(RenderError::InvalidState(
            "upload batch отсутствует после submit",
        ))?;
        self.in_flight_uploads.push(batch);
        Ok(())
    }

    pub(crate) fn flush_upload_serial(&mut self, serial: u64) -> Result<(), RenderError> {
        if self
            .pending_upload
            .as_ref()
            .is_some_and(|batch| batch.serial == serial)
        {
            self.flush_uploads()?;
        }
        Ok(())
    }

    pub(crate) fn poll_uploads(&mut self) -> Result<(), RenderError> {
        let device = self.device()?.clone();
        let completed = completed_upload_count(self.in_flight_uploads.iter().map(|batch| {
            let fence = batch
                .fence
                .ok_or(RenderError::InvalidState("upload fence отсутствует"))?;
            // SAFETY: each fence belongs to an upload submission on this device.
            unsafe { device.get_fence_status(fence) }.map_err(RenderError::from)
        }))?;
        // Uploads share one queue: only the completed prefix can be reclaimed.
        for batch in self.in_flight_uploads.drain(..completed) {
            self.completed_upload_serial = self.completed_upload_serial.max(batch.serial);
        }
        self.retired.upload_completed(self.completed_upload_serial);
        Ok(())
    }
}

pub(crate) fn completed_upload_count<E>(
    statuses: impl IntoIterator<Item = Result<bool, E>>,
) -> Result<usize, E> {
    let mut count = 0;
    for status in statuses {
        if !status? {
            break;
        }
        count += 1;
    }
    Ok(count)
}

pub(crate) struct MeshBuffers {
    pub(crate) vertex_buffer: DeviceBuffer,
    pub(crate) index_buffer: DeviceBuffer,
    pub(crate) index_count: u32,
    pub(crate) bounds: Option<LocalBounds>,
    pub(crate) upload_serial: u64,
    /// Веса костей; `Some` только у мешей, созданных через `create_skinned_mesh`.
    pub(crate) skin: Option<skin::SkinnedMesh>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct MeshUploadLayout {
    pub(crate) vertex_offset: usize,
    pub(crate) index_offset: usize,
    pub(crate) end: usize,
}

pub(crate) fn upload_required_alignment(alignment: usize) -> Option<usize> {
    let alignment = alignment.max(1);
    let mut left = 16;
    let mut right = alignment;
    while right != 0 {
        (left, right) = (right, left % right);
    }
    16usize.checked_div(left)?.checked_mul(alignment)
}

pub(crate) fn align_upload_offset(cursor: usize, alignment: usize) -> Option<usize> {
    let alignment = upload_required_alignment(alignment)?;
    let remainder = cursor % alignment;
    let offset = if cursor < 16 && remainder == 0 {
        alignment
    } else if remainder == 0 {
        cursor
    } else {
        cursor.checked_add(alignment - remainder)?
    };
    u64::try_from(offset).ok()?;
    Some(offset)
}

pub(crate) fn upload_data_layout(
    cursor: usize,
    size: usize,
    alignment: usize,
) -> Option<(usize, usize)> {
    let offset = align_upload_offset(cursor, alignment)?;
    let end = offset.checked_add(size)?;
    u64::try_from(end).ok()?;
    Some((offset, end))
}

pub(crate) fn mesh_upload_layout(
    cursor: usize,
    vertex_size: usize,
    index_size: usize,
    alignment: usize,
) -> Option<MeshUploadLayout> {
    let (vertex_offset, vertex_end) = upload_data_layout(cursor, vertex_size, alignment)?;
    let (index_offset, end) = upload_data_layout(vertex_end, index_size, alignment)?;
    Some(MeshUploadLayout {
        vertex_offset,
        index_offset,
        end,
    })
}

pub(crate) fn upload_batch_capacity(required_size: usize, alignment: usize) -> Option<usize> {
    Some(DEFAULT_UPLOAD_BATCH_CAPACITY.max(align_upload_offset(required_size, alignment)?))
}

pub(crate) fn upload_batch_flush_required(
    cursor: usize,
    vertex_size: usize,
    index_size: usize,
    alignment: usize,
    capacity: usize,
) -> Option<bool> {
    Some(mesh_upload_layout(cursor, vertex_size, index_size, alignment)?.end > capacity)
}

/// Записываемая пачка копирований и её staging память.
pub(crate) struct UploadBatch {
    pub(crate) device: ash::Device,
    pub(crate) command_pool: vk::CommandPool,
    pub(crate) command_buffer: vk::CommandBuffer,
    pub(crate) staging: HostBuffer,
    pub(crate) capacity: usize,
    pub(crate) cursor: usize,
    pub(crate) serial: u64,
    pub(crate) oversized: bool,
    pub(crate) recording: bool,
    pub(crate) fence: Option<vk::Fence>,
}

impl UploadBatch {
    pub(crate) fn new(
        instance: &ash::Instance,
        device: &ash::Device,
        physical_device: vk::PhysicalDevice,
        queue_family_index: u32,
        capacity: usize,
        oversized: bool,
        serial: u64,
    ) -> Result<Self, RenderError> {
        let staging = HostBuffer::new_empty(
            instance,
            device,
            physical_device,
            vk::BufferUsageFlags::TRANSFER_SRC,
            capacity,
        )?;
        let pool_info = vk::CommandPoolCreateInfo::default()
            .queue_family_index(queue_family_index)
            .flags(vk::CommandPoolCreateFlags::RESET_COMMAND_BUFFER);
        // SAFETY: the upload pool uses this renderer's graphics queue family.
        let command_pool = unsafe { device.create_command_pool(&pool_info, None) }?;
        let result = (|| {
            let allocate_info = vk::CommandBufferAllocateInfo::default()
                .command_pool(command_pool)
                .level(vk::CommandBufferLevel::PRIMARY)
                .command_buffer_count(1);
            // SAFETY: command pool belongs to this device and is not used elsewhere.
            let command_buffer = unsafe { device.allocate_command_buffers(&allocate_info) }?[0];
            let begin_info = vk::CommandBufferBeginInfo::default()
                .flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT);
            // SAFETY: command buffer was just allocated and is in the initial state.
            unsafe { device.begin_command_buffer(command_buffer, &begin_info) }?;
            Ok(command_buffer)
        })();
        let command_buffer = match result {
            Ok(command_buffer) => command_buffer,
            Err(error) => {
                // SAFETY: failed initialization did not submit work from this pool.
                unsafe { device.destroy_command_pool(command_pool, None) };
                return Err(error);
            }
        };
        Ok(Self {
            device: device.clone(),
            command_pool,
            command_buffer,
            staging,
            capacity,
            cursor: 0,
            serial,
            oversized,
            recording: true,
            fence: None,
        })
    }

    pub(crate) fn record_mesh_copies(
        &self,
        vertex_buffer: &DeviceBuffer,
        index_buffer: &DeviceBuffer,
        layout: MeshUploadLayout,
        vertex_size: usize,
        index_size: usize,
        skinned: bool,
    ) {
        let vertex_regions = [vk::BufferCopy2::default()
            .src_offset(layout.vertex_offset as vk::DeviceSize)
            .dst_offset(0)
            .size(vertex_size as vk::DeviceSize)];
        let vertex_copy = vk::CopyBufferInfo2::default()
            .src_buffer(self.staging.buffer)
            .dst_buffer(vertex_buffer.buffer)
            .regions(&vertex_regions);
        // SAFETY: source and destination ranges were allocated for these exact byte counts.
        unsafe {
            self.device
                .cmd_copy_buffer2(self.command_buffer, &vertex_copy)
        };
        let index_regions = [vk::BufferCopy2::default()
            .src_offset(layout.index_offset as vk::DeviceSize)
            .dst_offset(0)
            .size(index_size as vk::DeviceSize)];
        let index_copy = vk::CopyBufferInfo2::default()
            .src_buffer(self.staging.buffer)
            .dst_buffer(index_buffer.buffer)
            .regions(&index_regions);
        // SAFETY: source and destination ranges were allocated for these exact byte counts.
        unsafe {
            self.device
                .cmd_copy_buffer2(self.command_buffer, &index_copy)
        };
        let barriers = [
            vk::BufferMemoryBarrier2::default()
                .src_stage_mask(vk::PipelineStageFlags2::COPY)
                .src_access_mask(vk::AccessFlags2::TRANSFER_WRITE)
                .dst_stage_mask(vk::PipelineStageFlags2::VERTEX_ATTRIBUTE_INPUT)
                .dst_access_mask(vk::AccessFlags2::VERTEX_ATTRIBUTE_READ)
                .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .buffer(vertex_buffer.buffer)
                .offset(0)
                .size(vertex_size as vk::DeviceSize),
            vk::BufferMemoryBarrier2::default()
                .src_stage_mask(vk::PipelineStageFlags2::COPY)
                .src_access_mask(vk::AccessFlags2::TRANSFER_WRITE)
                .dst_stage_mask(vk::PipelineStageFlags2::INDEX_INPUT)
                .dst_access_mask(vk::AccessFlags2::INDEX_READ)
                .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .buffer(index_buffer.buffer)
                .offset(0)
                .size(index_size as vk::DeviceSize),
        ];
        let dependency = vk::DependencyInfo::default().buffer_memory_barriers(&barriers);
        // SAFETY: barriers make each copy visible to its subsequent vertex/index input access.
        unsafe {
            self.device
                .cmd_pipeline_barrier2(self.command_buffer, &dependency)
        };
        if skinned {
            // Исходные вершины skinning читает compute, а не вершинный вход.
            let skin_barriers = [vk::BufferMemoryBarrier2::default()
                .src_stage_mask(vk::PipelineStageFlags2::COPY)
                .src_access_mask(vk::AccessFlags2::TRANSFER_WRITE)
                .dst_stage_mask(vk::PipelineStageFlags2::COMPUTE_SHADER)
                .dst_access_mask(vk::AccessFlags2::SHADER_STORAGE_READ)
                .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .buffer(vertex_buffer.buffer)
                .offset(0)
                .size(vertex_size as vk::DeviceSize)];
            let dependency = vk::DependencyInfo::default().buffer_memory_barriers(&skin_barriers);
            // SAFETY: the barrier orders the vertex copy before the skinning compute reads it.
            unsafe {
                self.device
                    .cmd_pipeline_barrier2(self.command_buffer, &dependency)
            };
        }
    }

    pub(crate) fn record_image_upload(
        &self,
        image: vk::Image,
        width: u32,
        height: u32,
        mip_levels: u32,
        buffer_offset: usize,
    ) {
        pipeline_barrier(
            &self.device,
            self.command_buffer,
            texture_upload_barrier(image, mip_levels),
        );
        let region = [vk::BufferImageCopy::default()
            .buffer_offset(buffer_offset as vk::DeviceSize)
            .image_subresource(color_subresource_layers())
            .image_extent(vk::Extent3D {
                width,
                height,
                depth: 1,
            })];
        // SAFETY: the aligned staging range contains exactly the base-level RGBA8 texels.
        unsafe {
            self.device.cmd_copy_buffer_to_image(
                self.command_buffer,
                self.staging.buffer,
                image,
                vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                &region,
            )
        };
        let mut source_extent = vk::Extent2D { width, height };
        for mip_level in 1..mip_levels {
            let source_range = color_mip_subresource_range(mip_level - 1, 1);
            pipeline_barrier(
                &self.device,
                self.command_buffer,
                texture_mip_source_barrier(image, source_range),
            );
            let destination_extent = mip_extent(width, height, mip_level);
            let region = [texture_mip_blit_region(
                mip_level - 1,
                source_extent,
                destination_extent,
            )];
            // SAFETY: adjacent mip levels are distinct, in bounds, and use the same blit-capable format.
            unsafe {
                self.device.cmd_blit_image(
                    self.command_buffer,
                    image,
                    vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
                    image,
                    vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                    &region,
                    vk::Filter::LINEAR,
                )
            };
            pipeline_barrier(
                &self.device,
                self.command_buffer,
                texture_mip_shader_barrier(image, source_range),
            );
            source_extent = destination_extent;
        }
        pipeline_barrier(
            &self.device,
            self.command_buffer,
            texture_mip_destination_shader_barrier(
                image,
                color_mip_subresource_range(mip_levels - 1, 1),
            ),
        );
    }
}

impl Drop for UploadBatch {
    fn drop(&mut self) {
        if let Some(fence) = self.fence.take() {
            // SAFETY: submitted batches are dropped only after their fence signals or device idle.
            unsafe { self.device.destroy_fence(fence, None) };
        }
        // SAFETY: this batch is either not submitted or its fence has completed.
        unsafe { self.device.destroy_command_pool(self.command_pool, None) };
    }
}

pub(crate) fn overlay_vertex_bytes(vertices: &[OverlayVertex]) -> &[u8] {
    // SAFETY: OverlayVertex is repr(C) and consists only of f32 fields without padding.
    unsafe { std::slice::from_raw_parts(vertices.as_ptr().cast(), size_of_val(vertices)) }
}

pub(crate) fn vertex_bytes(vertices: &[Vertex]) -> &[u8] {
    // SAFETY: Vertex is repr(C) and consists only of f32 fields without padding.
    unsafe { std::slice::from_raw_parts(vertices.as_ptr().cast(), size_of_val(vertices)) }
}

pub(crate) fn index_bytes(indices: &[u32]) -> &[u8] {
    // SAFETY: u32 has no padding and any initialized u32 slice is readable as bytes.
    unsafe { std::slice::from_raw_parts(indices.as_ptr().cast(), size_of_val(indices)) }
}
