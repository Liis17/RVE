//! GPU-ресурсы renderer: буферы, изображения, загрузка, дескрипторы, цели кадра и viewport.

use crate::*;

mod api;
mod buffers;
mod descriptors;
mod environments;
mod frame_targets;
mod image;
mod materials;
mod upload;
mod viewports;

pub(crate) use buffers::*;
pub(crate) use descriptors::*;
pub(crate) use environments::*;
pub(crate) use frame_targets::*;
pub(crate) use image::*;
pub(crate) use materials::*;
pub(crate) use upload::*;
pub(crate) use viewports::*;

pub(crate) struct UiTexture {
    pub(crate) device: ash::Device,
    pub(crate) pool: vk::DescriptorPool,
    pub(crate) set: vk::DescriptorSet,
    pub(crate) image: DeviceImage,
    pub(crate) extent: vk::Extent2D,
    pub(crate) upload_serial: u64,
}

impl Drop for UiTexture {
    fn drop(&mut self) {
        // SAFETY: retirement waits for frame use and the image's upload serial before freeing its descriptor pool.
        unsafe { self.device.destroy_descriptor_pool(self.pool, None) };
    }
}

/// Ресурс, ожидающий кадры и связанный upload serial; освобождается своим `Drop`.
pub(crate) enum RetiredResource {
    Mesh(#[allow(dead_code)] Box<MeshBuffers>),
    Texture(#[allow(dead_code)] Arc<Texture>),
    Image(#[allow(dead_code)] Box<DeviceImage>),
    Material(#[allow(dead_code)] Box<GpuMaterial>),
    SurfaceShader(#[allow(dead_code)] Box<GpuSurfaceShader>),
    UiTexture(#[allow(dead_code)] Box<UiTexture>),
    Viewport(#[allow(dead_code)] Box<Viewport>),
    TemporalHistory(#[allow(dead_code)] Box<TemporalHistory>),
    #[cfg(windows)]
    FsrView(#[allow(dead_code)] Box<FsrView>),
    #[cfg(all(windows, feature = "dlss"))]
    DlssView(#[allow(dead_code)] Box<DlssView>),
    Environment(#[allow(dead_code)] Box<GpuEnvironment>),
    SkinInstance(#[allow(dead_code)] Box<skin::SkinInstance>),
    ParticleEmitter(#[allow(dead_code)] Box<ParticleEmitter>),
    ScreenShader(#[allow(dead_code)] Box<ScreenShaderPipelines>),
}
