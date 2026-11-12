//! Захват окна и viewport в RGBA8: очередь заявок, запись копий и чтение результата.

use crate::*;

impl Renderer {
    /// Queues a readback of the viewport after its next rendered composite pass.
    pub fn request_viewport_capture(&mut self, viewport: ViewportId) -> CaptureTicket {
        let ticket = next_capture_ticket(&mut self.next_capture_ticket);
        let state = if self.viewports.get(viewport.0).is_some_and(Option::is_some) {
            CaptureState::Queued
        } else {
            CaptureState::Ready(Err(unknown_viewport()))
        };
        self.captures.push(CaptureRecord {
            ticket,
            viewport: Some(viewport),
            state,
        });
        ticket
    }

    /// Queues a readback of the window image presented by the next successfully recorded frame.
    ///
    /// Both [`Self::render_frame`] and [`Self::render_frame_with_ui`] paths serve the request.
    /// The result fails when the surface lacks `TRANSFER_SRC` usage or uses a non-8-bit format.
    /// The image is opaque RGBA8 at the swapchain size and includes UI drawn into that frame.
    pub fn request_window_capture(&mut self) -> CaptureTicket {
        let ticket = next_capture_ticket(&mut self.next_capture_ticket);
        self.captures.push(CaptureRecord {
            ticket,
            viewport: None,
            state: CaptureState::Queued,
        });
        ticket
    }

    /// Cancels a queued or submitted capture without releasing an in-flight staging buffer early.
    pub fn cancel_capture(&mut self, ticket: CaptureTicket) {
        let Some(capture) = self
            .captures
            .iter_mut()
            .find(|capture| capture.ticket == ticket)
        else {
            return;
        };
        match cancel_capture_lifecycle(capture_lifecycle(&capture.state)) {
            CaptureLifecycle::Cancelled => capture.state = CaptureState::Cancelled,
            CaptureLifecycle::Submitted => {
                if let CaptureState::Submitted(submission) = &mut capture.state {
                    submission.completion = CaptureCompletion::Cancelled;
                }
            }
            CaptureLifecycle::Queued | CaptureLifecycle::Completed | CaptureLifecycle::Failed => {
                unreachable!("cancel transition is terminal or retains submitted buffers")
            }
        }
    }

    /// Returns captures whose submission fences have signaled, without waiting for the GPU.
    pub fn take_captures(&mut self) -> Vec<(CaptureTicket, Result<CapturedImage, RenderError>)> {
        self.poll_captures(None);
        let mut pending = Vec::with_capacity(self.captures.len());
        let mut completed = Vec::new();
        for capture in self.captures.drain(..) {
            match capture.state {
                CaptureState::Ready(result) => completed.push((capture.ticket, result)),
                CaptureState::Cancelled => {}
                state => pending.push(CaptureRecord { state, ..capture }),
            }
        }
        self.captures = pending;
        completed
    }

    pub(crate) fn queued_capture_tickets(&self, viewport: ViewportId) -> Vec<CaptureTicket> {
        self.captures
            .iter()
            .filter(|capture| {
                capture.viewport == Some(viewport)
                    && capture_lifecycle(&capture.state) == CaptureLifecycle::Queued
            })
            .map(|capture| capture.ticket)
            .collect()
    }

    pub(crate) fn commit_capture_submissions(&mut self, submissions: Vec<CaptureSubmission>) {
        for submission in submissions {
            if let Some(capture) = self
                .captures
                .iter_mut()
                .find(|capture| capture.ticket == submission.ticket)
                && matches!(capture.state, CaptureState::Queued)
            {
                capture.state = CaptureState::Submitted(Box::new(submission));
            }
        }
    }

    pub(crate) fn fail_capture_submissions(
        &mut self,
        submissions: &[CaptureSubmission],
        message: &'static str,
    ) {
        for submission in submissions {
            if let Some(capture) = self
                .captures
                .iter_mut()
                .find(|capture| capture.ticket == submission.ticket)
                && matches!(capture.state, CaptureState::Queued)
            {
                capture.state = CaptureState::Ready(Err(RenderError::InvalidState(message)));
            }
        }
    }

    pub(crate) fn fail_queued_captures(&mut self, viewports: &[ViewportId], message: &'static str) {
        for capture in &mut self.captures {
            if capture
                .viewport
                .is_some_and(|viewport| viewports.contains(&viewport))
                && capture_lifecycle(&capture.state) == CaptureLifecycle::Queued
            {
                capture.state = CaptureState::Ready(Err(RenderError::InvalidState(message)));
            }
        }
    }

    pub(crate) fn fail_all_queued_captures(&mut self, message: &'static str) {
        for capture in &mut self.captures {
            if capture_lifecycle(&capture.state) == CaptureLifecycle::Queued {
                capture.state = CaptureState::Ready(Err(RenderError::InvalidState(message)));
            }
        }
    }

    pub(crate) fn poll_captures(&mut self, only_slot: Option<usize>) {
        // Без отправленных копий нет ни fence для опроса, ни клона device.
        if !self
            .captures
            .iter()
            .any(|capture| matches!(capture.state, CaptureState::Submitted(_)))
        {
            return;
        }
        let Some(device) = self.device.as_ref().cloned() else {
            return;
        };
        for index in 0..self.captures.len() {
            let frame_slot = match &self.captures[index].state {
                CaptureState::Submitted(submission) => submission.frame_slot,
                CaptureState::Queued | CaptureState::Ready(_) | CaptureState::Cancelled => continue,
            };
            if only_slot.is_some_and(|only_slot| only_slot != frame_slot) {
                continue;
            }
            let Some(fence) = self.frames.get(frame_slot).map(|frame| frame.fence) else {
                if let CaptureState::Submitted(submission) = &mut self.captures[index].state {
                    submission.completion =
                        CaptureCompletion::Failed("capture frame slot is missing");
                }
                continue;
            };
            // SAFETY: this is the fence of the frame slot that submitted the copy command.
            match unsafe { device.get_fence_status(fence) } {
                Ok(false) => continue,
                Err(_) => {
                    if let CaptureState::Submitted(submission) = &mut self.captures[index].state {
                        submission.completion =
                            CaptureCompletion::Failed("capture fence status failed");
                    }
                    continue;
                }
                Ok(true) => {}
            }

            let CaptureState::Submitted(submission) =
                std::mem::replace(&mut self.captures[index].state, CaptureState::Cancelled)
            else {
                unreachable!("capture state was checked immediately before the fence poll")
            };
            let submission = *submission;
            self.captures[index].state = match submission.completion {
                CaptureCompletion::Image => {
                    let result = submission
                        .buffer
                        .read_bytes(submission.layout.size)
                        .and_then(|bytes| {
                            capture_rgba8(
                                &bytes,
                                submission.width,
                                submission.height,
                                submission.layout.row_pitch,
                                submission.order,
                            )
                            .map(|mut rgba8| {
                                if submission.opaque {
                                    make_opaque(&mut rgba8);
                                }
                                rgba8
                            })
                            .map(|rgba8| CapturedImage {
                                width: submission.width,
                                height: submission.height,
                                rgba8,
                            })
                        });
                    CaptureState::Ready(result)
                }
                CaptureCompletion::Failed(message) => {
                    CaptureState::Ready(Err(RenderError::InvalidState(message)))
                }
                CaptureCompletion::Cancelled => CaptureState::Cancelled,
            };
        }
    }
}

pub(crate) struct CaptureRecord {
    pub(crate) ticket: CaptureTicket,
    /// `None` — захват окна (swapchain), иначе захват viewport.
    pub(crate) viewport: Option<ViewportId>,
    pub(crate) state: CaptureState,
}

pub(crate) enum CaptureState {
    Queued,
    Submitted(Box<CaptureSubmission>),
    Ready(Result<CapturedImage, RenderError>),
    Cancelled,
}

pub(crate) struct CaptureSubmission {
    pub(crate) ticket: CaptureTicket,
    pub(crate) frame_slot: usize,
    pub(crate) buffer: HostBuffer,
    pub(crate) layout: CaptureBufferLayout,
    pub(crate) width: u32,
    pub(crate) height: u32,
    pub(crate) order: CapturePixelOrder,
    /// Альфа swapchain не несёт смысла для изображения: захват окна делается непрозрачным.
    pub(crate) opaque: bool,
    pub(crate) completion: CaptureCompletion,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CaptureCompletion {
    Image,
    Failed(&'static str),
    Cancelled,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CaptureLifecycle {
    Queued,
    Submitted,
    Completed,
    Failed,
    Cancelled,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CapturePixelOrder {
    Rgba,
    Bgra,
}

/// Порядок байт 8-битного формата swapchain для захвата окна; другие форматы не поддерживаются.
pub(crate) fn window_capture_order(format: vk::Format) -> Option<CapturePixelOrder> {
    match format {
        vk::Format::B8G8R8A8_SRGB | vk::Format::B8G8R8A8_UNORM => Some(CapturePixelOrder::Bgra),
        vk::Format::R8G8B8A8_SRGB | vk::Format::R8G8B8A8_UNORM => Some(CapturePixelOrder::Rgba),
        _ => None,
    }
}

/// Копия swapchain-изображения в host-буфер перед present.
#[derive(Clone, Copy)]
pub(crate) struct SwapchainCopy {
    pub(crate) buffer: vk::Buffer,
    pub(crate) row_length: u32,
}

pub(crate) fn capture_pixel_order(format: vk::Format) -> CapturePixelOrder {
    if format == vk::Format::B8G8R8A8_SRGB {
        CapturePixelOrder::Bgra
    } else {
        CapturePixelOrder::Rgba
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct CaptureBufferLayout {
    pub(crate) row_pitch: usize,
    pub(crate) row_length: u32,
    pub(crate) size: usize,
}

pub(crate) fn capture_lifecycle(state: &CaptureState) -> CaptureLifecycle {
    match state {
        CaptureState::Queued => CaptureLifecycle::Queued,
        CaptureState::Submitted(_) => CaptureLifecycle::Submitted,
        CaptureState::Ready(Ok(_)) => CaptureLifecycle::Completed,
        CaptureState::Ready(Err(_)) => CaptureLifecycle::Failed,
        CaptureState::Cancelled => CaptureLifecycle::Cancelled,
    }
}

pub(crate) fn cancel_capture_lifecycle(state: CaptureLifecycle) -> CaptureLifecycle {
    match state {
        CaptureLifecycle::Submitted => CaptureLifecycle::Submitted,
        CaptureLifecycle::Cancelled => CaptureLifecycle::Cancelled,
        CaptureLifecycle::Queued | CaptureLifecycle::Completed | CaptureLifecycle::Failed => {
            CaptureLifecycle::Cancelled
        }
    }
}

pub(crate) fn capture_buffer_layout(
    width: u32,
    height: u32,
    row_pitch_alignment: u64,
) -> Result<CaptureBufferLayout, RenderError> {
    if width == 0 || height == 0 {
        return Err(RenderError::InvalidConfig("viewport capture size is zero"));
    }
    let texel_bytes = 4_u64;
    let alignment = row_pitch_alignment.max(1);
    let divisor = greatest_common_divisor(alignment, texel_bytes);
    let pitch_alignment = alignment
        .checked_div(divisor)
        .and_then(|value| value.checked_mul(texel_bytes))
        .ok_or(RenderError::InvalidConfig(
            "viewport capture alignment overflow",
        ))?;
    let row_bytes = u64::from(width)
        .checked_mul(texel_bytes)
        .ok_or(RenderError::InvalidConfig("viewport capture row overflow"))?;
    let row_pitch = align_up(row_bytes, pitch_alignment)
        .ok_or(RenderError::InvalidConfig("viewport capture row overflow"))?;
    let size = row_pitch
        .checked_mul(u64::from(height))
        .ok_or(RenderError::InvalidConfig(
            "viewport capture buffer overflow",
        ))?;
    let row_length = row_pitch
        .checked_div(texel_bytes)
        .and_then(|length| u32::try_from(length).ok())
        .ok_or(RenderError::InvalidConfig(
            "viewport capture row is too wide",
        ))?;
    Ok(CaptureBufferLayout {
        row_pitch: usize::try_from(row_pitch)
            .map_err(|_| RenderError::InvalidConfig("viewport capture row is too wide"))?,
        row_length,
        size: usize::try_from(size)
            .map_err(|_| RenderError::InvalidConfig("viewport capture buffer is too large"))?,
    })
}

pub(crate) fn align_up(value: u64, alignment: u64) -> Option<u64> {
    let remainder = value % alignment;
    if remainder == 0 {
        Some(value)
    } else {
        value.checked_add(alignment - remainder)
    }
}

pub(crate) fn greatest_common_divisor(mut left: u64, mut right: u64) -> u64 {
    while right != 0 {
        (left, right) = (right, left % right);
    }
    left
}

pub(crate) fn next_capture_ticket(next: &mut u64) -> CaptureTicket {
    let ticket = CaptureTicket(*next);
    *next = next.wrapping_add(1);
    ticket
}

pub(crate) fn capture_rgba8(
    source: &[u8],
    width: u32,
    height: u32,
    row_pitch: usize,
    order: CapturePixelOrder,
) -> Result<Vec<u8>, RenderError> {
    let tight_row = usize::try_from(width)
        .ok()
        .and_then(|width| width.checked_mul(4))
        .ok_or(RenderError::InvalidConfig("viewport capture row overflow"))?;
    let source_size = row_pitch
        .checked_mul(height as usize)
        .ok_or(RenderError::InvalidConfig(
            "viewport capture buffer overflow",
        ))?;
    if width == 0 || height == 0 || row_pitch < tight_row || source.len() < source_size {
        return Err(RenderError::InvalidConfig(
            "invalid viewport capture buffer layout",
        ));
    }
    let mut rgba = Vec::with_capacity(tight_row * height as usize);
    for row in source.chunks_exact(row_pitch).take(height as usize) {
        let row = &row[..tight_row];
        match order {
            CapturePixelOrder::Rgba => rgba.extend_from_slice(row),
            CapturePixelOrder::Bgra => {
                for pixel_index in 0..width as usize {
                    let pixel_start = pixel_index * 4;
                    rgba.extend_from_slice(&[
                        row[pixel_start + 2],
                        row[pixel_start + 1],
                        row[pixel_start],
                        row[pixel_start + 3],
                    ]);
                }
            }
        }
    }
    Ok(rgba)
}

/// Swapchain и кадровый слот, для которых готовятся захваты окна.
pub(crate) struct WindowCaptureTarget<'a> {
    pub(crate) instance: &'a ash::Instance,
    pub(crate) device: &'a ash::Device,
    pub(crate) physical_device: vk::PhysicalDevice,
    pub(crate) row_alignment: u64,
    pub(crate) transfer_src: bool,
    pub(crate) format: vk::Format,
    pub(crate) frame_slot: usize,
    pub(crate) extent: vk::Extent2D,
}

/// Выделяет host-буферы для захватов окна, ждущих кадра; неподдерживаемые запросы завершает.
///
/// Ошибка выделения завершает только захваты окна: кадр продолжает рисоваться.
pub(crate) fn prepare_window_captures(
    captures: &mut [CaptureRecord],
    target: WindowCaptureTarget<'_>,
) -> (Vec<SwapchainCopy>, Vec<CaptureSubmission>) {
    let tickets = queued_window_tickets(captures);
    if tickets.is_empty() {
        return (Vec::new(), Vec::new());
    }
    let prepared = window_capture_support(target.transfer_src, target.format).and_then(|order| {
        allocate_window_captures(&target, &tickets, order).map_err(|error| {
            log::warn!("не удалось подготовить захват окна: {error}");
            "не удалось выделить буфер захвата окна"
        })
    });
    prepared.unwrap_or_else(|message| {
        fail_captures(captures, &tickets, message);
        (Vec::new(), Vec::new())
    })
}

pub(crate) fn queued_window_tickets(captures: &[CaptureRecord]) -> Vec<CaptureTicket> {
    captures
        .iter()
        .filter(|capture| {
            capture.viewport.is_none()
                && capture_lifecycle(&capture.state) == CaptureLifecycle::Queued
        })
        .map(|capture| capture.ticket)
        .collect()
}

/// Порядок байт захвата окна или причина, по которой swapchain нельзя прочитать.
pub(crate) fn window_capture_support(
    transfer_src: bool,
    format: vk::Format,
) -> Result<CapturePixelOrder, &'static str> {
    if !transfer_src {
        return Err("surface не поддерживает TRANSFER_SRC для захвата окна");
    }
    window_capture_order(format).ok_or("формат swapchain не поддерживает захват окна")
}

pub(crate) fn fail_captures(
    captures: &mut [CaptureRecord],
    tickets: &[CaptureTicket],
    message: &str,
) {
    for capture in captures {
        if tickets.contains(&capture.ticket) {
            capture.state = CaptureState::Ready(Err(RenderError::Unsupported(message.to_owned())));
        }
    }
}

pub(crate) fn allocate_window_captures(
    target: &WindowCaptureTarget<'_>,
    tickets: &[CaptureTicket],
    order: CapturePixelOrder,
) -> Result<(Vec<SwapchainCopy>, Vec<CaptureSubmission>), RenderError> {
    let extent = target.extent;
    let layout = capture_buffer_layout(extent.width, extent.height, target.row_alignment)?;
    let mut copies = Vec::with_capacity(tickets.len());
    let mut submissions = Vec::with_capacity(tickets.len());
    for &ticket in tickets {
        let buffer = HostBuffer::new_empty(
            target.instance,
            target.device,
            target.physical_device,
            vk::BufferUsageFlags::TRANSFER_DST,
            layout.size,
        )?;
        copies.push(SwapchainCopy {
            buffer: buffer.buffer,
            row_length: layout.row_length,
        });
        submissions.push(CaptureSubmission {
            ticket,
            frame_slot: target.frame_slot,
            buffer,
            layout,
            width: extent.width,
            height: extent.height,
            order,
            opaque: true,
            completion: CaptureCompletion::Image,
        });
    }
    Ok((copies, submissions))
}

pub(crate) fn make_opaque(rgba8: &mut [u8]) {
    for pixel in rgba8.as_chunks_mut::<4>().0 {
        pixel[3] = u8::MAX;
    }
}

/// Identifier for an asynchronous viewport capture request.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct CaptureTicket(pub(crate) u64);

/// CPU readback of a viewport's post-composite sRGB image.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CapturedImage {
    pub width: u32,
    pub height: u32,
    pub rgba8: Vec<u8>,
}

/// Переводит swapchain-изображение после вывода в `PRESENT_SRC_KHR` через копии в host-буферы.
pub(crate) fn record_present_copies(
    device: &ash::Device,
    command_buffer: vk::CommandBuffer,
    image: vk::Image,
    extent: vk::Extent2D,
    copies: &[SwapchainCopy],
) {
    let to_transfer = vk::ImageMemoryBarrier2::default()
        .src_stage_mask(vk::PipelineStageFlags2::COLOR_ATTACHMENT_OUTPUT)
        .src_access_mask(vk::AccessFlags2::COLOR_ATTACHMENT_WRITE)
        .dst_stage_mask(vk::PipelineStageFlags2::TRANSFER)
        .dst_access_mask(vk::AccessFlags2::TRANSFER_READ)
        .old_layout(vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL)
        .new_layout(vk::ImageLayout::TRANSFER_SRC_OPTIMAL)
        .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
        .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
        .image(image)
        .subresource_range(color_subresource_range());
    pipeline_barrier(device, command_buffer, to_transfer);
    for copy in copies {
        let regions = [vk::BufferImageCopy::default()
            .buffer_offset(0)
            .buffer_row_length(copy.row_length)
            .buffer_image_height(0)
            .image_subresource(
                vk::ImageSubresourceLayers::default()
                    .aspect_mask(vk::ImageAspectFlags::COLOR)
                    .mip_level(0)
                    .base_array_layer(0)
                    .layer_count(1),
            )
            .image_offset(vk::Offset3D::default())
            .image_extent(vk::Extent3D {
                width: extent.width,
                height: extent.height,
                depth: 1,
            })];
        // SAFETY: the swapchain image is in TRANSFER_SRC_OPTIMAL and was created with TRANSFER_SRC;
        // the staging buffer fits the aligned extent.
        unsafe {
            device.cmd_copy_image_to_buffer(
                command_buffer,
                image,
                vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
                copy.buffer,
                &regions,
            );
        }
    }
    let host_read = vk::MemoryBarrier2::default()
        .src_stage_mask(vk::PipelineStageFlags2::TRANSFER)
        .src_access_mask(vk::AccessFlags2::TRANSFER_WRITE)
        .dst_stage_mask(vk::PipelineStageFlags2::HOST)
        .dst_access_mask(vk::AccessFlags2::HOST_READ);
    let to_present = vk::ImageMemoryBarrier2::default()
        .src_stage_mask(vk::PipelineStageFlags2::TRANSFER)
        .src_access_mask(vk::AccessFlags2::NONE)
        .dst_stage_mask(vk::PipelineStageFlags2::NONE)
        .dst_access_mask(vk::AccessFlags2::NONE)
        .old_layout(vk::ImageLayout::TRANSFER_SRC_OPTIMAL)
        .new_layout(vk::ImageLayout::PRESENT_SRC_KHR)
        .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
        .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
        .image(image)
        .subresource_range(color_subresource_range());
    let memory_barriers = [host_read];
    let image_barriers = [to_present];
    let dependency = vk::DependencyInfo::default()
        .memory_barriers(&memory_barriers)
        .image_memory_barriers(&image_barriers);
    // SAFETY: the copies above write host-visible buffers read only after the frame fence signals.
    unsafe { device.cmd_pipeline_barrier2(command_buffer, &dependency) };
}

pub(crate) fn record_viewport_capture(
    device: &ash::Device,
    command_buffer: vk::CommandBuffer,
    image: vk::Image,
    buffer: vk::Buffer,
    extent: vk::Extent2D,
    row_length: u32,
) {
    let to_transfer = vk::ImageMemoryBarrier2::default()
        .src_stage_mask(vk::PipelineStageFlags2::FRAGMENT_SHADER)
        .src_access_mask(vk::AccessFlags2::SHADER_SAMPLED_READ)
        .dst_stage_mask(vk::PipelineStageFlags2::TRANSFER)
        .dst_access_mask(vk::AccessFlags2::TRANSFER_READ)
        .old_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)
        .new_layout(vk::ImageLayout::TRANSFER_SRC_OPTIMAL)
        .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
        .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
        .image(image)
        .subresource_range(color_subresource_range());
    pipeline_barrier(device, command_buffer, to_transfer);

    let regions = [vk::BufferImageCopy::default()
        .buffer_offset(0)
        .buffer_row_length(row_length)
        .buffer_image_height(0)
        .image_subresource(
            vk::ImageSubresourceLayers::default()
                .aspect_mask(vk::ImageAspectFlags::COLOR)
                .mip_level(0)
                .base_array_layer(0)
                .layer_count(1),
        )
        .image_offset(vk::Offset3D::default())
        .image_extent(vk::Extent3D {
            width: extent.width,
            height: extent.height,
            depth: 1,
        })];
    // SAFETY: the output image is in TRANSFER_SRC_OPTIMAL and the staging buffer fits the aligned extent.
    unsafe {
        device.cmd_copy_image_to_buffer(
            command_buffer,
            image,
            vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
            buffer,
            &regions,
        );
    }

    let host_read = vk::BufferMemoryBarrier2::default()
        .src_stage_mask(vk::PipelineStageFlags2::TRANSFER)
        .src_access_mask(vk::AccessFlags2::TRANSFER_WRITE)
        .dst_stage_mask(vk::PipelineStageFlags2::HOST)
        .dst_access_mask(vk::AccessFlags2::HOST_READ)
        .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
        .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
        .buffer(buffer)
        .offset(0)
        .size(vk::WHOLE_SIZE);
    let host_barriers = [host_read];
    let host_dependency = vk::DependencyInfo::default().buffer_memory_barriers(&host_barriers);
    // SAFETY: the copy writes this host-visible buffer before the frame fence signals.
    unsafe { device.cmd_pipeline_barrier2(command_buffer, &host_dependency) };

    let to_shader = vk::ImageMemoryBarrier2::default()
        .src_stage_mask(vk::PipelineStageFlags2::TRANSFER)
        .src_access_mask(vk::AccessFlags2::TRANSFER_READ)
        .dst_stage_mask(vk::PipelineStageFlags2::FRAGMENT_SHADER)
        .dst_access_mask(vk::AccessFlags2::SHADER_SAMPLED_READ)
        .old_layout(vk::ImageLayout::TRANSFER_SRC_OPTIMAL)
        .new_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)
        .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
        .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
        .image(image)
        .subresource_range(color_subresource_range());
    pipeline_barrier(device, command_buffer, to_shader);
}

#[cfg(test)]
mod tests;
