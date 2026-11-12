//! Буферы в host- и device-памяти и выбор типа памяти.

use crate::*;

/// Host-visible буфер кадрового слота, растущий до следующей степени двойки.
#[derive(Default)]
pub(crate) struct GrowingBuffer {
    pub(crate) buffer: Option<HostBuffer>,
    pub(crate) capacity: usize,
}

impl GrowingBuffer {
    /// Копирует данные в начало буфера. Вызывается после ожидания fence слота,
    /// когда GPU уже не читает прежний буфер; пустые данные буфер не меняют.
    pub(crate) fn write(
        &mut self,
        instance: &ash::Instance,
        device: &ash::Device,
        physical_device: vk::PhysicalDevice,
        usage: vk::BufferUsageFlags,
        bytes: &[u8],
    ) -> Result<(), RenderError> {
        drop(self.write_keep_old(instance, device, physical_device, usage, bytes)?);
        Ok(())
    }

    /// Grows a descriptor-backed slot buffer without dropping its prior buffer until the
    /// descriptor has been redirected by the caller.
    pub(crate) fn write_keep_old(
        &mut self,
        instance: &ash::Instance,
        device: &ash::Device,
        physical_device: vk::PhysicalDevice,
        usage: vk::BufferUsageFlags,
        bytes: &[u8],
    ) -> Result<Option<HostBuffer>, RenderError> {
        if bytes.is_empty() {
            return Ok(None);
        }
        if self.buffer.is_none() || self.capacity < bytes.len() {
            let capacity = bytes.len().next_power_of_two();
            let mut data = vec![0; capacity];
            data[..bytes.len()].copy_from_slice(bytes);
            let new_buffer = HostBuffer::new(instance, device, physical_device, usage, &data)?;
            let previous = self.buffer.replace(new_buffer);
            self.capacity = capacity;
            return Ok(previous);
        }
        self.buffer
            .as_ref()
            .ok_or(RenderError::InvalidState("буфер кадра отсутствует"))?
            .write(bytes)?;
        Ok(None)
    }
}

/// Буфер в host-visible памяти; данные копируются один раз при создании.
pub(crate) struct HostBuffer {
    pub(crate) device: ash::Device,
    pub(crate) buffer: vk::Buffer,
    pub(crate) memory: vk::DeviceMemory,
    pub(crate) size: usize,
}

impl HostBuffer {
    pub(crate) fn new(
        instance: &ash::Instance,
        device: &ash::Device,
        physical_device: vk::PhysicalDevice,
        usage: vk::BufferUsageFlags,
        data: &[u8],
    ) -> Result<Self, RenderError> {
        let buffer = Self::new_empty(instance, device, physical_device, usage, data.len())?;
        buffer.write(data)?;
        Ok(buffer)
    }

    pub(crate) fn new_empty(
        instance: &ash::Instance,
        device: &ash::Device,
        physical_device: vk::PhysicalDevice,
        usage: vk::BufferUsageFlags,
        size: usize,
    ) -> Result<Self, RenderError> {
        let buffer_info = vk::BufferCreateInfo::default()
            .size(u64::try_from(size).map_err(|_| {
                RenderError::InvalidConfig("размер Vulkan buffer превышает DeviceSize")
            })?)
            .usage(usage)
            .sharing_mode(vk::SharingMode::EXCLUSIVE);
        // SAFETY: callers request a non-zero buffer size.
        let buffer = unsafe { device.create_buffer(&buffer_info, None) }?;
        let mut host_buffer = Self {
            device: device.clone(),
            buffer,
            memory: vk::DeviceMemory::null(),
            size,
        };
        // SAFETY: buffer was created by this device and remains alive.
        let requirements = unsafe { device.get_buffer_memory_requirements(buffer) };
        let memory_type = find_memory_type(
            instance,
            physical_device,
            requirements.memory_type_bits,
            vk::MemoryPropertyFlags::HOST_VISIBLE | vk::MemoryPropertyFlags::HOST_COHERENT,
        )
        .ok_or_else(|| {
            RenderError::Unsupported(
                "нет HOST_VISIBLE | HOST_COHERENT memory для буфера".to_owned(),
            )
        })?;
        let allocate_info = vk::MemoryAllocateInfo::default()
            .allocation_size(requirements.size)
            .memory_type_index(memory_type);
        // SAFETY: allocation requirements and memory type were reported for this buffer.
        host_buffer.memory = unsafe { device.allocate_memory(&allocate_info, None) }?;
        // SAFETY: memory allocation satisfies the buffer's reported requirements.
        unsafe { device.bind_buffer_memory(buffer, host_buffer.memory, 0) }?;
        Ok(host_buffer)
    }
}

impl HostBuffer {
    /// Перезаписывает начало буфера; GPU не должен читать буфер во время записи.
    pub(crate) fn write(&self, data: &[u8]) -> Result<(), RenderError> {
        self.write_at(0, data)
    }

    pub(crate) fn write_at(&self, offset: usize, data: &[u8]) -> Result<(), RenderError> {
        if offset
            .checked_add(data.len())
            .is_none_or(|end| end > self.size)
        {
            return Err(RenderError::InvalidState(
                "запись выходит за размер host buffer",
            ));
        }
        if data.is_empty() {
            return Ok(());
        }
        // SAFETY: callers write within the coherent buffer while it is not in use by the GPU.
        unsafe {
            let mapped = self.device.map_memory(
                self.memory,
                0,
                self.size as vk::DeviceSize,
                vk::MemoryMapFlags::empty(),
            )?;
            std::ptr::copy_nonoverlapping(
                data.as_ptr(),
                mapped.cast::<u8>().add(offset),
                data.len(),
            );
            self.device.unmap_memory(self.memory);
        }
        Ok(())
    }

    pub(crate) fn read_bytes(&self, length: usize) -> Result<Vec<u8>, RenderError> {
        if length > self.size {
            return Err(RenderError::InvalidState(
                "чтение выходит за размер host buffer",
            ));
        }
        let mut bytes = vec![0; length];
        if length == 0 {
            return Ok(bytes);
        }
        // SAFETY: callers read only after the submission fence and the buffer is host-coherent.
        unsafe {
            let mapped = self.device.map_memory(
                self.memory,
                0,
                length as vk::DeviceSize,
                vk::MemoryMapFlags::empty(),
            )?;
            std::ptr::copy_nonoverlapping(mapped.cast::<u8>(), bytes.as_mut_ptr(), length);
            self.device.unmap_memory(self.memory);
        }
        Ok(bytes)
    }
}

/// Буфер в DEVICE_LOCAL памяти для vertex и index input.
pub(crate) struct DeviceBuffer {
    pub(crate) device: ash::Device,
    pub(crate) buffer: vk::Buffer,
    pub(crate) memory: vk::DeviceMemory,
}

impl DeviceBuffer {
    pub(crate) fn new(
        instance: &ash::Instance,
        device: &ash::Device,
        physical_device: vk::PhysicalDevice,
        usage: vk::BufferUsageFlags,
        size: usize,
    ) -> Result<Self, RenderError> {
        let buffer_info = vk::BufferCreateInfo::default()
            .size(u64::try_from(size).map_err(|_| {
                RenderError::InvalidConfig("размер Vulkan buffer превышает DeviceSize")
            })?)
            .usage(usage)
            .sharing_mode(vk::SharingMode::EXCLUSIVE);
        // SAFETY: callers provide non-empty mesh data and a valid usage.
        let buffer = unsafe { device.create_buffer(&buffer_info, None) }?;
        let mut device_buffer = Self {
            device: device.clone(),
            buffer,
            memory: vk::DeviceMemory::null(),
        };
        // SAFETY: buffer belongs to this device and is alive.
        let requirements = unsafe { device.get_buffer_memory_requirements(buffer) };
        let memory_type = find_memory_type(
            instance,
            physical_device,
            requirements.memory_type_bits,
            vk::MemoryPropertyFlags::DEVICE_LOCAL,
        )
        .ok_or_else(|| {
            RenderError::Unsupported("нет DEVICE_LOCAL memory для mesh buffer".to_owned())
        })?;
        let allocate_info = vk::MemoryAllocateInfo::default()
            .allocation_size(requirements.size)
            .memory_type_index(memory_type);
        // SAFETY: allocation size and memory type were returned for this buffer.
        device_buffer.memory = unsafe { device.allocate_memory(&allocate_info, None) }?;
        // SAFETY: allocation satisfies this buffer's memory requirements.
        unsafe { device.bind_buffer_memory(buffer, device_buffer.memory, 0) }?;
        Ok(device_buffer)
    }
}

impl Drop for DeviceBuffer {
    fn drop(&mut self) {
        // SAFETY: owning renderer waits for frame and upload serials before dropping mesh buffers.
        unsafe { self.device.destroy_buffer(self.buffer, None) };
        if self.memory != vk::DeviceMemory::null() {
            // SAFETY: the buffer bound to this allocation was destroyed first.
            unsafe { self.device.free_memory(self.memory, None) };
        }
    }
}

impl Drop for HostBuffer {
    fn drop(&mut self) {
        // SAFETY: owners drop this buffer only after uploads, frame submissions, or device idle complete.
        unsafe { self.device.destroy_buffer(self.buffer, None) };
        if self.memory != vk::DeviceMemory::null() {
            // SAFETY: the buffer bound to this memory was destroyed first.
            unsafe { self.device.free_memory(self.memory, None) };
        }
    }
}

pub(crate) fn find_memory_type(
    instance: &ash::Instance,
    physical_device: vk::PhysicalDevice,
    type_bits: u32,
    flags: vk::MemoryPropertyFlags,
) -> Option<u32> {
    // SAFETY: memory properties are queried from the live physical device.
    let properties = unsafe { instance.get_physical_device_memory_properties(physical_device) };
    (0..properties.memory_type_count).find(|index| {
        type_bits & (1 << index) != 0
            && properties.memory_types[*index as usize]
                .property_flags
                .contains(flags)
    })
}
