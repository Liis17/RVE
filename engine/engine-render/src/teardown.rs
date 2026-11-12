//! Освобождение Vulkan-ресурсов `Renderer` в порядке зависимостей.

use crate::*;

impl Drop for Renderer {
    fn drop(&mut self) {
        // Reflex выключается до ожидания простоя: sleep/marker из других потоков перестают доходить до Vulkan.
        if let Some(latency) = self.latency.take() {
            latency.deactivate();
        }
        if let Some(device) = self.device.as_ref() {
            // SAFETY: teardown waits for renderer submissions before destroying owned Vulkan resources.
            let _ = unsafe { device.device_wait_idle() };
            if self.latency_semaphore != vk::Semaphore::null() {
                // SAFETY: device idle and Reflex deactivated: no sleep, marker or submit references the semaphore.
                unsafe { device.destroy_semaphore(self.latency_semaphore, None) };
                self.latency_semaphore = vk::Semaphore::null();
            }
        }
        self.pending_upload.take();
        self.in_flight_uploads.clear();
        // SAFETY: device_wait_idle above completed every command that can use capture staging buffers.
        self.captures.clear();
        self.swapchain.take();
        self.pending_swapchain_drop.take();
        self.retired_swapchains.clear();
        self.frames.clear();
        self.retired.clear();
        self.environments.clear();
        self.viewports.clear();
        // Пайплайны экранных шейдеров и их layout освобождаем до device; retired уже очищен выше.
        self.screen_shaders.clear();
        self.screen_shader_layout.take();
        self.screen_temporal.history.take();
        self.screen_temporal.exposure_resources.take();
        #[cfg(windows)]
        self.screen_temporal.fsr.take();
        #[cfg(all(windows, feature = "dlss"))]
        self.screen_temporal.dlss.take();
        #[cfg(all(windows, feature = "dlss"))]
        self.dlss_api.take();
        self.ui_textures.clear();
        // Экземпляры skinning владеют пулами наборов и буферами; освобождаем их до layout и device.
        self.skin_instances.clear();
        // Эмиттеры частиц держат буферы, наборы и текстуры; освобождаем их до мешей и device.
        if let Some(device) = self.device.as_ref() {
            self.particles.destroy(device);
        }
        self.meshes.clear();
        self.materials.clear();
        self.builtin_user_uniform.take();
        self.surface_shaders.clear();
        self.textures.clear();
        self.color_luts.clear();
        if let Some(device) = self.device.as_ref() {
            if let Some(pool) = self.frame_descriptor_pool.take() {
                // SAFETY: the device is idle; destroying the pool frees its frame sets.
                unsafe { device.destroy_descriptor_pool(pool, None) };
            }
            self.frame_uniforms.clear();
            if let Some(pool) = self.post_descriptor_pool.take() {
                // SAFETY: frame targets are gone; destroying the pool frees their post sets.
                unsafe { device.destroy_descriptor_pool(pool, None) };
            }
            if let Some(sampler) = self.sampler.take() {
                // SAFETY: every material descriptor referencing the sampler was destroyed above.
                unsafe { device.destroy_sampler(sampler, None) };
            }
            if let Some(sampler) = self.ui_sampler.take() {
                // SAFETY: every UI texture and viewport descriptor referencing the sampler is gone.
                unsafe { device.destroy_sampler(sampler, None) };
            }
            if let Some(sampler) = self.post_sampler.take() {
                // SAFETY: every post descriptor pool referencing this sampler was destroyed above.
                unsafe { device.destroy_sampler(sampler, None) };
            }
            if let Some(sampler) = self.shadow_sampler.take() {
                // SAFETY: the frame descriptor pool referencing the sampler was destroyed above.
                unsafe { device.destroy_sampler(sampler, None) };
            }
            if let Some(sampler) = self.environment_sampler.take() {
                // SAFETY: all environment descriptor pools were destroyed above.
                unsafe { device.destroy_sampler(sampler, None) };
            }
            if let Some(sampler) = self.equirect_sampler.take() {
                // SAFETY: environment baking is synchronous and all compute sets were freed.
                unsafe { device.destroy_sampler(sampler, None) };
            }
            if let Some(image) = self.brdf_image.take() {
                // SAFETY: environment descriptor sets were destroyed above.
                drop(image);
            }
            if let Some(image) = self.shadow_image.take() {
                // SAFETY: the frame descriptor pool is gone and the device is idle.
                drop(image);
            }
            if let Some(image) = self.local_shadow_image.take() {
                // SAFETY: the frame descriptor pool is gone and the device is idle.
                drop(image);
            }
            if let Some(pipeline) = self.shadow_pipeline.take() {
                // SAFETY: the device is idle, so no command buffer uses this pipeline.
                unsafe { device.destroy_pipeline(pipeline, None) };
            }
            if let Some(pipeline) = self.local_shadow_pipeline.take() {
                // SAFETY: the device is idle, so no command buffer uses this pipeline.
                unsafe { device.destroy_pipeline(pipeline, None) };
            }
            for pipeline in [
                self.pipeline.take(),
                self.mask_pipeline.take(),
                self.blend_pipeline.take(),
                self.overdraw_pipeline.take(),
                self.wireframe_pipeline.take(),
                self.skybox_pipeline.take(),
            ]
            .into_iter()
            .flatten()
            {
                // SAFETY: the device is idle, so no command buffer uses this pipeline.
                unsafe { device.destroy_pipeline(pipeline, None) };
            }
            for pipeline in [
                self.tonemap_pipeline.take(),
                self.output_pipeline.take(),
                self.viewport_output_pipeline.take(),
                self.taa_pipeline.take(),
            ]
            .into_iter()
            .flatten()
            {
                // SAFETY: the device is idle, so no command buffer uses these pipelines.
                unsafe { device.destroy_pipeline(pipeline, None) };
            }
            for pipeline in [
                self.equirect_pipeline.take(),
                self.procedural_sky_pipeline.take(),
                self.irradiance_pipeline.take(),
                self.prefilter_pipeline.take(),
                self.cluster_pipeline.take(),
            ]
            .into_iter()
            .flatten()
            {
                // SAFETY: synchronous environment bakes and all frame submissions have completed.
                unsafe { device.destroy_pipeline(pipeline, None) };
            }
            if let Some(layout) = self.pipeline_layout.take() {
                // SAFETY: the only pipeline created with this layout was destroyed above.
                unsafe { device.destroy_pipeline_layout(layout, None) };
            }
            if let Some(layout) = self.post_pipeline_layout.take() {
                // SAFETY: all pipelines created with this layout were destroyed above.
                unsafe { device.destroy_pipeline_layout(layout, None) };
            }
            if let Some(pipelines) = self.bloom_pipelines.take() {
                pipelines.destroy(device);
            }
            if let Some(pipelines) = self.exposure_pipelines.take() {
                pipelines.destroy(device);
            }
            if let Some(pipelines) = self.filter_pipelines.take() {
                pipelines.destroy(device);
            }
            if let Some(pipelines) = self.ao_pipelines.take() {
                pipelines.destroy(device);
            }
            if let Some(pipelines) = self.occlusion_pipelines.take() {
                pipelines.destroy(device);
            }
            for pipeline in [
                self.opaque_equal_pipeline.take(),
                self.prepass_pipeline.take(),
            ]
            .into_iter()
            .flatten()
            {
                // SAFETY: the device is idle, so no command buffer uses the mesh variant.
                unsafe { device.destroy_pipeline(pipeline, None) };
            }
            if let Some(layout) = self.taa_pipeline_layout.take() {
                // SAFETY: the TAA pipeline created with this layout was destroyed above.
                unsafe { device.destroy_pipeline_layout(layout, None) };
            }
            if let Some(layout) = self.compute_pipeline_layout.take() {
                // SAFETY: every compute pipeline using this layout was destroyed above.
                unsafe { device.destroy_pipeline_layout(layout, None) };
            }
            if let Some(layout) = self.cluster_pipeline_layout.take() {
                // SAFETY: the clustered compute pipeline was destroyed above.
                unsafe { device.destroy_pipeline_layout(layout, None) };
            }
            if let Some(pipeline) = self.skin_pipeline.take() {
                // SAFETY: device is idle and no submission uses the skinning pipeline.
                unsafe { device.destroy_pipeline(pipeline, None) };
            }
            if let Some(layout) = self.skin_pipeline_layout.take() {
                // SAFETY: the skinning pipeline using this layout was destroyed above.
                unsafe { device.destroy_pipeline_layout(layout, None) };
            }
            if let Some(layout) = self.skin_set_layout.take() {
                // SAFETY: descriptor sets using this layout are freed with their pools above.
                unsafe { device.destroy_descriptor_set_layout(layout, None) };
            }
            if let Some(pipeline) = self.overlay_pipeline.take() {
                // SAFETY: the device is idle, so no command buffer uses this pipeline.
                unsafe { device.destroy_pipeline(pipeline, None) };
            }
            if let Some(layout) = self.overlay_pipeline_layout.take() {
                // SAFETY: the only pipeline created with this layout was destroyed above.
                unsafe { device.destroy_pipeline_layout(layout, None) };
            }
            for pipeline in [self.ui_pipeline.take(), self.viewport_ui_pipeline.take()]
                .into_iter()
                .flatten()
            {
                // SAFETY: the device is idle, so no command buffer uses these pipelines.
                unsafe { device.destroy_pipeline(pipeline, None) };
            }
            if let Some(layout) = self.ui_pipeline_layout.take() {
                // SAFETY: the only pipeline created with this layout was destroyed above.
                unsafe { device.destroy_pipeline_layout(layout, None) };
            }
            for layout in [
                self.frame_set_layout.take(),
                self.material_set_layout.take(),
                self.environment_set_layout.take(),
                self.compute_set_layout.take(),
                self.texture_set_layout.take(),
                self.post_set_layout.take(),
                self.taa_set_layout.take(),
            ]
            .into_iter()
            .flatten()
            {
                // SAFETY: the pipeline layout and all descriptor pools using it are gone.
                unsafe { device.destroy_descriptor_set_layout(layout, None) };
            }
            if let Some(cache) = self.pipeline_cache.take() {
                // SAFETY: every pipeline created from the cache was destroyed above.
                unsafe { device.destroy_pipeline_cache(cache, None) };
            }
        }
        if let (Some(device), Some(command_pool)) = (self.device.as_ref(), self.command_pool.take())
        {
            // SAFETY: all command buffers and per-frame objects are no longer in use.
            unsafe { device.destroy_command_pool(command_pool, None) };
        }
        if let Some(device) = self.device.take() {
            // SAFETY: swapchain, frame resources and command pool were destroyed first.
            unsafe { device.destroy_device(None) };
        }
        if let (Some(loader), Some(surface)) = (self.surface_loader.as_ref(), self.surface.take()) {
            // SAFETY: the logical device and swapchain are gone; the instance still owns this surface.
            unsafe { loader.destroy_surface(surface, None) };
        }
        self.surface_loader.take();
        if let Some(instance) = self.instance.take() {
            // SAFETY: surface and logical device children have already been destroyed.
            unsafe { instance.destroy_instance(None) };
        }
        // Entry and the Arc-owned window remain fields until after the destructor body completes.
    }
}
