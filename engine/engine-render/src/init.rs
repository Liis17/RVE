//! Создание `Renderer`: instance, устройство, очереди и начальные ресурсы.

use crate::*;

impl Renderer {
    /// Загружает Vulkan и создаёт renderer. Окно удерживается до уничтожения surface.
    pub fn new<W>(
        window: Arc<W>,
        width: u32,
        height: u32,
        config: RendererConfig,
    ) -> Result<Self, RenderError>
    where
        W: HasDisplayHandle + HasWindowHandle + Send + Sync + 'static,
    {
        let entry = engine_platform::load_vulkan()
            .map_err(|error| RenderError::Loader(error.to_string()))?;
        let window: Arc<dyn WindowHandleOwner> = window;
        let mut renderer = Self {
            entry,
            instance: None,
            surface_loader: None,
            surface: None,
            device: None,
            swapchain_loader: None,
            physical_device: vk::PhysicalDevice::null(),
            queue: vk::Queue::null(),
            queue_family_index: 0,
            upload_alignment: 16,
            capture_row_alignment: 4,
            next_upload_serial: 1,
            completed_upload_serial: 0,
            pending_upload: None,
            in_flight_uploads: Vec::new(),
            command_pool: None,
            pipeline_cache: None,
            pipeline_layout: None,
            pipeline: None,
            mask_pipeline: None,
            blend_pipeline: None,
            overdraw_pipeline: None,
            wireframe_pipeline: None,
            skybox_pipeline: None,
            shadow_pipeline: None,
            local_shadow_pipeline: None,
            cluster_pipeline: None,
            cluster_pipeline_layout: None,
            skin_set_layout: None,
            skin_pipeline_layout: None,
            skin_pipeline: None,
            skin_instances: Vec::new(),
            skin_call_serial: 0,
            post_pipeline_layout: None,
            bloom_pipelines: None,
            exposure_pipelines: None,
            filter_pipelines: None,
            ao_pipelines: None,
            occlusion_pipelines: None,
            opaque_equal_pipeline: None,
            prepass_pipeline: None,
            taa_set_layout: None,
            taa_pipeline_layout: None,
            taa_pipeline: None,
            tonemap_pipeline: None,
            output_pipeline: None,
            viewport_output_pipeline: None,
            screen_shaders: Vec::new(),
            screen_shader_layout: None,
            overlay_pipeline_layout: None,
            overlay_pipeline: None,
            ui_pipeline_layout: None,
            ui_pipeline: None,
            viewport_ui_pipeline: None,
            ui_sampler: None,
            timestamp_valid_bits: 0,
            timestamp_period: 0.0,
            pending_gpu_time: None,
            pending_frame_profile: None,
            texture_sampler_anisotropy: None,
            indirect_instancing: false,
            frame_set_layout: None,
            material_set_layout: None,
            environment_set_layout: None,
            compute_set_layout: None,
            texture_set_layout: None,
            post_set_layout: None,
            frame_descriptor_pool: None,
            post_descriptor_pool: None,
            frame_uniforms: Vec::new(),
            sampler: None,
            builtin_user_uniform: None,
            post_sampler: None,
            shadow_sampler: None,
            environment_sampler: None,
            equirect_sampler: None,
            compute_pipeline_layout: None,
            equirect_pipeline: None,
            procedural_sky_pipeline: None,
            irradiance_pipeline: None,
            prefilter_pipeline: None,
            brdf_image: None,
            environments: Vec::new(),
            shadow_image: None,
            shadow_layout: vk::ImageLayout::UNDEFINED,
            local_shadow_image: None,
            local_shadow_layout: vk::ImageLayout::UNDEFINED,
            max_texture_size: 0,
            meshes: Vec::new(),
            textures: Vec::new(),
            color_luts: Vec::new(),
            materials: Vec::new(),
            surface_shaders: Vec::new(),
            ui_textures: Vec::new(),
            viewports: Vec::new(),
            screen_temporal: TemporalView::default(),
            particles: ParticleSystem::default(),
            retired: RetireQueue::new(FRAMES_IN_FLIGHT),
            retired_swapchains: RetiredSwapchains::new(FRAMES_IN_FLIGHT),
            latency: None,
            latency_semaphore: vk::Semaphore::null(),
            latency_frame: 0,
            present_ids: PresentIdTracker::default(),
            pending_swapchain_drop: None,
            frames: Vec::new(),
            captures: Vec::new(),
            next_capture_ticket: 0,
            frame_slot: 0,
            submission_generation: 0,
            swapchain_generation: 0,
            #[cfg(windows)]
            fsr_api: None,
            #[cfg(windows)]
            fsr_load_error: None,
            #[cfg(all(windows, feature = "dlss"))]
            dlss_api: None,
            dlss_load_error: None,
            fsr_warning_emitted: false,
            dlss_warning_emitted: false,
            swapchain: None,
            surface_format: vk::SurfaceFormatKHR::default(),
            present_mode: config.present_mode,
            last_warned_present_mode: None,
            frame_serial: 0,
            output_extent: vk::Extent2D { width, height },
            swapchain_dirty: true,
            frame_slots_lost: false,
            config,
            capabilities: Capabilities {
                device_name: String::new(),
                vendor_id: 0,
                api_version: 0,
                discrete: false,
                tessellation_shader: false,
                wireframe: false,
                fsr: false,
                dlss: false,
                low_latency: false,
            },
            window,
        };
        renderer.initialize()?;
        Ok(renderer)
    }

    #[cfg(windows)]
    pub(crate) fn load_fsr_runtime(&mut self) {
        let Some(path) = self.config.upscalers.fsr_library.as_deref() else {
            self.fsr_load_error = Some("FSR loader DLL is not configured".to_owned());
            return;
        };
        let effect_path = path
            .parent()
            .unwrap_or_else(|| std::path::Path::new("."))
            .join("amd_fidelityfx_upscaler_vk.dll");
        if !effect_path.is_file() {
            self.fsr_load_error = Some(format!(
                "FSR upscaler effect DLL is missing: {}",
                effect_path.display()
            ));
            return;
        }
        let Some(instance) = self.instance.as_ref() else {
            self.fsr_load_error = Some("Vulkan instance is unavailable".to_owned());
            return;
        };
        let exposure_usage = vk::ImageUsageFlags::TRANSFER_DST | vk::ImageUsageFlags::SAMPLED;
        // SAFETY: the selected physical device belongs to the live Vulkan instance.
        let exposure_features = unsafe {
            instance
                .get_physical_device_format_properties(self.physical_device, vk::Format::R32_SFLOAT)
        }
        .optimal_tiling_features;
        if !exposure_features
            .contains(vk::FormatFeatureFlags::TRANSFER_DST | vk::FormatFeatureFlags::SAMPLED_IMAGE)
            || unsafe {
                instance.get_physical_device_image_format_properties(
                    self.physical_device,
                    vk::Format::R32_SFLOAT,
                    vk::ImageType::TYPE_2D,
                    vk::ImageTiling::OPTIMAL,
                    exposure_usage,
                    vk::ImageCreateFlags::empty(),
                )
            }
            .is_err()
        {
            self.fsr_load_error = Some(
                "R32_SFLOAT exposure image does not support sampled and transfer-destination usage"
                    .to_owned(),
            );
            return;
        }
        match fsr::FfxApi::load(path) {
            Ok(api) => {
                self.fsr_api = Some(api);
                self.capabilities.fsr = true;
            }
            Err(error) => self.fsr_load_error = Some(error),
        }
    }

    pub(crate) fn initialize(&mut self) -> Result<(), RenderError> {
        #[cfg(all(windows, feature = "dlss"))]
        {
            if let Some(path) = self.config.upscalers.dlss_library.as_deref() {
                match dlss::NgxApi::new(
                    self.config.upscalers.dlss_project_id.as_deref(),
                    self.config.upscalers.dlss_application_id,
                    self.config.upscalers.dlss_engine_version.as_deref(),
                    path,
                ) {
                    Ok(api) => self.dlss_api = Some(api),
                    Err(error) => self.dlss_load_error = Some(error),
                }
            } else {
                self.dlss_load_error = Some("DLSS runtime DLL is not configured".to_owned());
            }
        }
        #[cfg(not(all(windows, feature = "dlss")))]
        {
            self.dlss_load_error = Some(if self.config.upscalers.dlss_library.is_some() {
                "DLSS support was not compiled into this target".to_owned()
            } else if cfg!(windows) {
                "DLSS runtime DLL is not configured".to_owned()
            } else {
                "DLSS is only available on Windows".to_owned()
            });
        }

        let display_handle = self.window.display_handle().map_err(RenderError::Handle)?;
        let required = engine_platform::enumerate_required_extensions(display_handle.as_raw())
            .map_err(RenderError::Vulkan)?;
        let mut requested_instance_extensions = required
            .iter()
            .map(|pointer| {
                // SAFETY: the platform adapter returns static, null-terminated extension names.
                unsafe { CStr::from_ptr(*pointer) }.to_owned()
            })
            .collect::<Vec<_>>();
        requested_instance_extensions.extend(
            engine_platform::INSTANCE_EXTENSIONS
                .iter()
                .map(|name| CString::from(*name)),
        );
        requested_instance_extensions.extend(
            self.config
                .extra_instance_extensions
                .iter()
                .map(|name| CString::from(*name)),
        );
        let available_instance_extensions =
            // SAFETY: querying loader extension properties does not require an instance.
            unsafe { self.entry.enumerate_instance_extension_properties(None) }?;
        #[cfg(all(windows, feature = "dlss"))]
        let mut dlss_instance_ready = false;
        #[cfg(all(windows, feature = "dlss"))]
        if let Some(api) = self.dlss_api.as_ref() {
            match api.instance_extensions() {
                Ok(extensions) => {
                    let missing = missing_extensions(
                        &extensions,
                        &available_instance_extensions,
                        "DLSS instance",
                    );
                    if missing.is_empty() {
                        requested_instance_extensions.extend(extensions);
                        dlss_instance_ready = true;
                    } else {
                        self.dlss_load_error = Some(missing.join(", "));
                    }
                }
                Err(error) => self.dlss_load_error = Some(error),
            }
        }
        let requested_instance_extensions = deduplicate_names(requested_instance_extensions);
        check_extensions(
            &requested_instance_extensions,
            &available_instance_extensions,
            "instance",
        )?;

        let mut enabled_layers = Vec::new();
        if cfg!(debug_assertions) {
            let validation_layer = c"VK_LAYER_KHRONOS_validation";
            let layers =
                // SAFETY: querying the loader's available instance layers is read-only.
                unsafe { self.entry.enumerate_instance_layer_properties() }?;
            let validation_available = layers.iter().any(|layer| {
                // SAFETY: Vulkan layer property names are fixed-size null-terminated strings.
                (unsafe { CStr::from_ptr(layer.layer_name.as_ptr()) }) == validation_layer
            });
            if validation_available {
                enabled_layers.push(validation_layer);
            } else {
                log::warn!(
                    "VK_LAYER_KHRONOS_validation отсутствует; продолжаю без validation layer"
                );
            }
        }

        let instance_extension_pointers = requested_instance_extensions
            .iter()
            .map(|name| name.as_ptr())
            .collect::<Vec<_>>();
        let layer_pointers = enabled_layers
            .iter()
            .map(|name| name.as_ptr())
            .collect::<Vec<_>>();
        let application_info = vk::ApplicationInfo::default()
            .application_name(c"RVE")
            .application_version(vk::make_api_version(0, 0, 1, 0))
            .engine_name(c"RVE")
            .engine_version(vk::make_api_version(0, 0, 1, 0))
            .api_version(vk::make_api_version(0, 1, 3, 0));
        let instance_info = vk::InstanceCreateInfo::default()
            .flags(engine_platform::INSTANCE_FLAGS)
            .application_info(&application_info)
            .enabled_extension_names(&instance_extension_pointers)
            .enabled_layer_names(&layer_pointers);
        // SAFETY: all extension/layer names and create-info values remain alive for this call.
        let instance = unsafe { self.entry.create_instance(&instance_info, None) }?;
        self.instance = Some(instance);

        let display_handle = self.window.display_handle().map_err(RenderError::Handle)?;
        let window_handle = self.window.window_handle().map_err(RenderError::Handle)?;
        // SAFETY: handles refer to the live Arc-owned window and the instance enabled required surface extensions.
        let surface = unsafe {
            engine_platform::create_surface(
                &self.entry,
                self.instance()?,
                display_handle.as_raw(),
                window_handle.as_raw(),
                None,
            )
        }?;
        self.surface_loader = Some(ash::khr::surface::Instance::new(
            &self.entry,
            self.instance()?,
        ));
        self.surface = Some(surface);

        let base_device_extensions =
            required_device_extensions(&self.config.extra_device_extensions);
        let choice = choose_device(
            self.instance()?,
            self.surface_loader()?,
            self.surface()?,
            &base_device_extensions,
        )?;
        self.physical_device = choice.physical_device;
        self.queue_family_index = choice.queue_family_index;
        self.surface_format = choice.surface_format;
        self.timestamp_valid_bits = choice.timestamp_valid_bits;
        self.timestamp_period = choice.timestamp_period;
        self.texture_sampler_anisotropy = choice.texture_sampler_anisotropy;
        self.indirect_instancing = choice.indirect_instancing;
        self.capabilities = Capabilities {
            device_name: choice.device_name,
            vendor_id: choice.vendor_id,
            api_version: choice.api_version,
            discrete: choice.discrete,
            tessellation_shader: choice.tessellation_shader,
            wireframe: choice.wireframe,
            fsr: false,
            dlss: false,
            low_latency: false,
        };

        #[allow(unused_mut)]
        let mut device_extensions = base_device_extensions;
        #[cfg(all(windows, feature = "dlss"))]
        let mut dlss_device_ready = false;
        #[cfg(all(windows, feature = "dlss"))]
        if dlss_instance_ready && let Some(api) = self.dlss_api.as_ref() {
            match api.check_support(self.instance()?.handle(), self.physical_device) {
                Ok(()) => match api
                    .device_extensions(self.instance()?.handle(), self.physical_device)
                {
                    Ok(extensions) => {
                        // SAFETY: the selected physical device belongs to the live instance.
                        let available = unsafe {
                            self.instance()?
                                .enumerate_device_extension_properties(self.physical_device)
                        }?;
                        let missing = missing_extensions(&extensions, &available, "DLSS device");
                        if missing.is_empty() {
                            device_extensions.extend(extensions);
                            dlss_device_ready = true;
                        } else {
                            self.dlss_load_error = Some(missing.join(", "));
                        }
                    }
                    Err(error) => self.dlss_load_error = Some(error),
                },
                Err(error) => self.dlss_load_error = Some(error),
            }
        }
        let low_latency_ready = self.config.low_latency != LowLatencyMode::Off
            && match low_latency_support(self.instance()?, self.physical_device) {
                Ok(()) => true,
                Err(reason) => {
                    eprintln!("NVIDIA Reflex отключён: {reason}; рендеринг продолжается без него");
                    false
                }
            };
        if low_latency_ready {
            device_extensions.extend(
                low_latency_device_extensions()
                    .into_iter()
                    .map(CString::from),
            );
        }
        let device_extensions = deduplicate_names(device_extensions);

        let queue_priority = [1.0_f32];
        let queue_info = [vk::DeviceQueueCreateInfo::default()
            .queue_family_index(self.queue_family_index)
            .queue_priorities(&queue_priority)];
        let extension_pointers = device_extensions
            .iter()
            .map(|name| name.as_ptr())
            .collect::<Vec<_>>();
        let features = vk::PhysicalDeviceFeatures::default()
            .tessellation_shader(self.capabilities.tessellation_shader)
            .fill_mode_non_solid(self.capabilities.wireframe)
            .shader_storage_image_extended_formats(true)
            .sampler_anisotropy(self.texture_sampler_anisotropy.is_some())
            .multi_draw_indirect(self.indirect_instancing)
            .draw_indirect_first_instance(self.indirect_instancing);
        let mut features_13 = vk::PhysicalDeviceVulkan13Features::default()
            .dynamic_rendering(true)
            .synchronization2(true);
        let mut extended_dynamic_state =
            vk::PhysicalDeviceExtendedDynamicStateFeaturesEXT::default()
                .extended_dynamic_state(true);
        let mut present_id_features =
            vk::PhysicalDevicePresentIdFeaturesKHR::default().present_id(true);
        let mut timeline_features =
            vk::PhysicalDeviceVulkan12Features::default().timeline_semaphore(true);
        let mut device_info = vk::DeviceCreateInfo::default()
            .queue_create_infos(&queue_info)
            .enabled_extension_names(&extension_pointers)
            .enabled_features(&features)
            .push_next(&mut features_13)
            .push_next(&mut extended_dynamic_state);
        if low_latency_ready {
            device_info = device_info
                .push_next(&mut present_id_features)
                .push_next(&mut timeline_features);
        }
        // SAFETY: the selected physical device reported every requested feature and extension.
        let device = unsafe {
            self.instance()?
                .create_device(self.physical_device, &device_info, None)
        }?;
        self.device = Some(device);
        if low_latency_ready {
            self.init_low_latency()?;
        }
        #[cfg(all(windows, feature = "dlss"))]
        if dlss_device_ready {
            let instance_handle = self.instance()?.handle();
            let device_handle = self.device()?.handle();
            if let Some(api) = self.dlss_api.as_mut() {
                match api.initialize(instance_handle, self.physical_device, device_handle) {
                    Ok(()) => {
                        self.capabilities.dlss = true;
                        self.dlss_load_error = None;
                    }
                    Err(error) => self.dlss_load_error = Some(error),
                }
            }
        }
        self.queue =
            // SAFETY: queue family and index were checked during physical-device selection.
            unsafe { self.device()?.get_device_queue(self.queue_family_index, 0) };
        #[cfg(windows)]
        self.load_fsr_runtime();
        self.swapchain_loader = Some(ash::khr::swapchain::Device::new(
            self.instance()?,
            self.device()?,
        ));

        let command_pool_info = vk::CommandPoolCreateInfo::default()
            .queue_family_index(self.queue_family_index)
            .flags(vk::CommandPoolCreateFlags::RESET_COMMAND_BUFFER);
        // SAFETY: device and queue-family index are valid for this renderer.
        self.command_pool =
            Some(unsafe { self.device()?.create_command_pool(&command_pool_info, None) }?);
        // SAFETY: physical_device was selected from this live instance.
        let properties = unsafe {
            self.instance()?
                .get_physical_device_properties(self.physical_device)
        };
        let cache_data = compatible_pipeline_cache(
            &self.config.pipeline_cache,
            PipelineCacheIdentity {
                vendor_id: properties.vendor_id,
                device_id: properties.device_id,
                pipeline_cache_uuid: properties.pipeline_cache_uuid,
            },
        );
        let cache_info = vk::PipelineCacheCreateInfo::default().initial_data(cache_data);
        // SAFETY: supplied cache data passed the Vulkan version-one device identity checks.
        self.pipeline_cache =
            Some(unsafe { self.device()?.create_pipeline_cache(&cache_info, None) }?);
        self.config.pipeline_cache = Vec::new();
        let limits = properties.limits;
        self.max_texture_size = limits.max_image_dimension2_d;
        self.capture_row_alignment = limits.optimal_buffer_copy_row_pitch_alignment.max(1);
        self.upload_alignment = usize::try_from(limits.optimal_buffer_copy_offset_alignment)
            .map_err(|_| {
                RenderError::Unsupported("слишком большое выравнивание buffer copy".to_owned())
            })?
            .max(1);
        self.create_frame_resources()?;
        self.create_shadow_resources()?;
        self.create_descriptor_resources()?;
        self.create_cluster_pipeline()?;
        self.create_skin_pipeline()?;
        self.create_particle_pipelines()?;
        self.create_environment_resources()?;
        self.create_default_environment()?;
        self.create_identity_color_lut()?;
        self.create_mesh_pipeline()?;
        self.create_skybox_pipeline()?;
        self.create_shadow_pipeline()?;
        self.create_local_shadow_pipeline()?;
        self.create_post_pipelines()?;
        self.create_overlay_pipeline()?;
        self.create_ui_pipeline()?;
        let white_srgb = Arc::new(self.upload_texture(1, 1, &[255; 4], TextureColorSpace::Srgb)?);
        self.textures.push(Some(white_srgb));
        let white_linear =
            Arc::new(self.upload_texture(1, 1, &[255; 4], TextureColorSpace::Linear)?);
        self.textures.push(Some(white_linear));
        let flat_normal = Arc::new(self.upload_texture(
            1,
            1,
            &[128, 128, 255, 255],
            TextureColorSpace::Linear,
        )?);
        self.textures.push(Some(flat_normal));
        let black =
            Arc::new(self.upload_texture(1, 1, &[0, 0, 0, 255], TextureColorSpace::Linear)?);
        self.textures.push(Some(black));
        self.create_material(&Material::default())?;

        if self.output_extent.width != 0 && self.output_extent.height != 0 {
            self.recreate_swapchain()?;
        }
        Ok(())
    }

    pub(crate) fn create_frame_resources(&mut self) -> Result<(), RenderError> {
        let allocate_info = vk::CommandBufferAllocateInfo::default()
            .command_pool(self.command_pool()?)
            .level(vk::CommandBufferLevel::PRIMARY)
            .command_buffer_count(FRAMES_IN_FLIGHT as u32);
        // SAFETY: command pool was created for the selected graphics queue family.
        let command_buffers = unsafe { self.device()?.allocate_command_buffers(&allocate_info) }?;
        let timestamps = self.timestamp_valid_bits != 0;
        for command_buffer in command_buffers {
            self.frames.push(FrameResources::new(
                self.device()?,
                command_buffer,
                timestamps,
            )?);
        }
        Ok(())
    }

    /// Создаёт directional и local depth-массивы и sampler сравнения для всех слотов кадров.
    pub(crate) fn create_shadow_resources(&mut self) -> Result<(), RenderError> {
        let image = DeviceImage::new_array(
            self.instance()?,
            self.device()?,
            self.physical_device,
            vk::Extent2D {
                width: SHADOW_MAP_RESOLUTION,
                height: SHADOW_MAP_RESOLUTION,
            },
            DEPTH_FORMAT,
            vk::ImageUsageFlags::DEPTH_STENCIL_ATTACHMENT | vk::ImageUsageFlags::SAMPLED,
            shadow_subresource_range(),
            SHADOW_CASCADE_COUNT as u32,
        )?;
        self.shadow_image = Some(image);
        let local_image = DeviceImage::new_array(
            self.instance()?,
            self.device()?,
            self.physical_device,
            vk::Extent2D {
                width: LOCAL_SHADOW_MAP_RESOLUTION,
                height: LOCAL_SHADOW_MAP_RESOLUTION,
            },
            DEPTH_FORMAT,
            vk::ImageUsageFlags::DEPTH_STENCIL_ATTACHMENT | vk::ImageUsageFlags::SAMPLED,
            local_shadow_subresource_range(),
            MAX_LOCAL_SHADOW_LAYERS as u32,
        )?;
        self.local_shadow_image = Some(local_image);
        let sampler_info = vk::SamplerCreateInfo::default()
            .mag_filter(vk::Filter::LINEAR)
            .min_filter(vk::Filter::LINEAR)
            .mipmap_mode(vk::SamplerMipmapMode::NEAREST)
            .address_mode_u(vk::SamplerAddressMode::CLAMP_TO_EDGE)
            .address_mode_v(vk::SamplerAddressMode::CLAMP_TO_EDGE)
            .address_mode_w(vk::SamplerAddressMode::CLAMP_TO_EDGE)
            .compare_enable(true)
            .compare_op(vk::CompareOp::LESS_OR_EQUAL)
            .max_lod(0.0);
        // SAFETY: linear depth comparison is required during physical-device selection.
        self.shadow_sampler = Some(unsafe { self.device()?.create_sampler(&sampler_info, None) }?);
        Ok(())
    }

    /// Создаёт timeline semaphore Reflex и общее состояние после создания device.
    pub(crate) fn init_low_latency(&mut self) -> Result<(), RenderError> {
        let device = self.device()?.clone();
        let mut type_info = vk::SemaphoreTypeCreateInfo::default()
            .semaphore_type(vk::SemaphoreType::TIMELINE)
            .initial_value(0);
        let create_info = vk::SemaphoreCreateInfo::default().push_next(&mut type_info);
        // SAFETY: the device enabled timelineSemaphore; the create info lives for this call.
        let semaphore = unsafe { device.create_semaphore(&create_info, None) }?;
        self.latency_semaphore = semaphore;
        let backend = VulkanLatency::new(self.instance()?, &device, semaphore);
        self.latency = Some(Arc::new(Shared::new(
            self.config.low_latency,
            Arc::new(backend),
        )));
        self.capabilities.low_latency = true;
        Ok(())
    }
}
