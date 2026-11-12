//! Публичное API создания и удаления ресурсов: сетки, текстуры, материалы, шейдеры и LUT.

use crate::*;

impl Renderer {
    /// Записывает асинхронную загрузку треугольного меша в GPU. Каждые три индекса образуют треугольник.
    /// Ошибка отправки пачки может вернуться из следующего `create_mesh`, `destroy_mesh` или кадра.
    pub fn create_mesh(
        &mut self,
        vertices: &[Vertex],
        indices: &[u32],
    ) -> Result<MeshId, RenderError> {
        self.create_mesh_inner(vertices, indices, false)
    }

    /// Меш для skinning: вершины читает compute, поэтому буфер доступен как storage.
    pub(crate) fn create_mesh_with_skin(
        &mut self,
        vertices: &[Vertex],
        indices: &[u32],
    ) -> Result<MeshId, RenderError> {
        self.create_mesh_inner(vertices, indices, true)
    }

    pub(crate) fn create_mesh_inner(
        &mut self,
        vertices: &[Vertex],
        indices: &[u32],
        skinned: bool,
    ) -> Result<MeshId, RenderError> {
        validate_mesh(vertices.len(), indices)?;
        let instance = self.instance()?.clone();
        let device = self.device()?.clone();
        let bounds = LocalBounds::from_vertices(vertices);
        let vertex_data = vertex_bytes(vertices);
        let index_data = index_bytes(indices);
        let layout = mesh_upload_layout(
            0,
            vertex_data.len(),
            index_data.len(),
            self.upload_alignment,
        )
        .ok_or(RenderError::InvalidMesh(
            "размер меша переполняет диапазон буфера",
        ))?;
        let capacity = upload_batch_capacity(layout.end, self.upload_alignment).ok_or(
            RenderError::InvalidMesh("размер меша переполняет диапазон буфера"),
        )?;
        let must_flush = self.pending_upload.as_ref().is_some_and(|batch| {
            batch.oversized
                || upload_batch_flush_required(
                    batch.cursor,
                    vertex_data.len(),
                    index_data.len(),
                    self.upload_alignment,
                    batch.capacity,
                )
                .unwrap_or(true)
        });
        if must_flush {
            self.flush_uploads()?;
        }
        if self.pending_upload.is_none() {
            self.poll_uploads()?;
            self.pending_upload = Some(UploadBatch::new(
                &instance,
                &device,
                self.physical_device,
                self.queue_family_index,
                capacity,
                capacity > DEFAULT_UPLOAD_BATCH_CAPACITY,
                self.next_upload_serial,
            )?);
            self.next_upload_serial = self
                .next_upload_serial
                .checked_add(1)
                .ok_or(RenderError::InvalidState("upload serial исчерпан"))?;
        }
        let batch = self
            .pending_upload
            .as_mut()
            .ok_or(RenderError::InvalidState("upload batch отсутствует"))?;
        let layout = mesh_upload_layout(
            batch.cursor,
            vertex_data.len(),
            index_data.len(),
            self.upload_alignment,
        )
        .ok_or(RenderError::InvalidMesh(
            "размер меша переполняет диапазон буфера",
        ))?;
        let vertex_usage = if skinned {
            vk::BufferUsageFlags::VERTEX_BUFFER
                | vk::BufferUsageFlags::TRANSFER_DST
                | vk::BufferUsageFlags::STORAGE_BUFFER
        } else {
            vk::BufferUsageFlags::VERTEX_BUFFER | vk::BufferUsageFlags::TRANSFER_DST
        };
        let vertex_buffer = DeviceBuffer::new(
            &instance,
            &device,
            self.physical_device,
            vertex_usage,
            vertex_data.len(),
        )?;
        let index_buffer = DeviceBuffer::new(
            &instance,
            &device,
            self.physical_device,
            vk::BufferUsageFlags::INDEX_BUFFER | vk::BufferUsageFlags::TRANSFER_DST,
            index_data.len(),
        )?;
        batch.staging.write_at(layout.vertex_offset, vertex_data)?;
        batch.staging.write_at(layout.index_offset, index_data)?;
        batch.record_mesh_copies(
            &vertex_buffer,
            &index_buffer,
            layout,
            vertex_data.len(),
            index_data.len(),
            skinned,
        );
        batch.cursor = layout.end;
        let upload_serial = batch.serial;
        self.meshes.push(Some(MeshBuffers {
            vertex_buffer,
            index_buffer,
            index_count: indices.len() as u32,
            bounds,
            upload_serial,
            skin: None,
        }));
        Ok(MeshId(self.meshes.len() - 1))
    }

    /// Отправляет его ожидающую загрузку при необходимости; буферы освобождаются после upload и кадров.
    pub fn destroy_mesh(&mut self, mesh: MeshId) -> Result<(), RenderError> {
        if self
            .skin_instances
            .iter()
            .flatten()
            .any(|instance| instance.mesh == mesh)
        {
            return Err(RenderError::InvalidState(
                "меш используется экземпляром skinning; сначала освободите его",
            ));
        }
        let upload_serial = self
            .meshes
            .get(mesh.0)
            .and_then(Option::as_ref)
            .map(|buffers| buffers.upload_serial)
            .ok_or(RenderError::InvalidMesh("MeshId не принадлежит renderer"))?;
        self.flush_upload_serial(upload_serial)?;
        let buffers = self
            .meshes
            .get_mut(mesh.0)
            .and_then(Option::take)
            .ok_or(RenderError::InvalidMesh("MeshId не принадлежит renderer"))?;
        self.retired
            .retire(RetiredResource::Mesh(Box::new(buffers)), upload_serial);
        Ok(())
    }

    /// Записывает sRGB RGBA8 загрузку со строками сверху вниз в upload batch.
    /// Ошибка отправки batch может вернуться из следующего resource API или кадра.
    pub fn create_texture(
        &mut self,
        width: u32,
        height: u32,
        rgba: &[u8],
    ) -> Result<TextureId, RenderError> {
        self.create_texture_with_color_space(width, height, rgba, TextureColorSpace::Srgb)
    }

    /// Записывает RGBA8-текстуру заданного цветового пространства в upload batch.
    /// Ошибка отправки batch может вернуться из следующего resource API или кадра.
    pub fn create_texture_with_color_space(
        &mut self,
        width: u32,
        height: u32,
        rgba: &[u8],
        color_space: TextureColorSpace,
    ) -> Result<TextureId, RenderError> {
        validate_texture(width, height, rgba.len(), self.max_texture_size)?;
        let texture = Arc::new(self.upload_texture(width, height, rgba, color_space)?);
        self.textures.push(Some(texture));
        Ok(TextureId(self.textures.len() - 1))
    }

    /// Uploads an equirectangular Radiance image or procedural sky and precomputes its IBL maps.
    pub fn create_environment(
        &mut self,
        source: &EnvironmentSource,
    ) -> Result<EnvironmentId, RenderError> {
        environment::validate_source(source)?;
        if let EnvironmentSource::Equirect { width, height, .. } = source
            && (*width > self.max_texture_size || *height > self.max_texture_size)
        {
            return Err(RenderError::InvalidEnvironment(
                "equirect dimensions exceed the selected GPU image limit".to_owned(),
            ));
        }
        let environment = self.create_gpu_environment(source)?;
        self.environments.push(Some(environment));
        Ok(EnvironmentId(self.environments.len() - 1))
    }

    /// Retires an environment after submitted frames have stopped using its descriptor set.
    pub fn destroy_environment(&mut self, environment: EnvironmentId) -> Result<(), RenderError> {
        if environment.0 == 0 {
            return Err(RenderError::InvalidEnvironment(
                "built-in environment cannot be destroyed".to_owned(),
            ));
        }
        let resource = self
            .environments
            .get_mut(environment.0)
            .and_then(Option::take)
            .ok_or_else(|| {
                RenderError::InvalidEnvironment(
                    "EnvironmentId does not belong to renderer".to_owned(),
                )
            })?;
        self.retired
            .retire(RetiredResource::Environment(Box::new(resource)), 0);
        Ok(())
    }

    /// Создаёт неизменяемый материал, ссылающийся только на текстуры совместимого цветового пространства.
    pub fn create_material(&mut self, material: &Material) -> Result<MaterialId, RenderError> {
        let spaces = self
            .textures
            .iter()
            .map(|texture| texture.as_ref().map(|texture| texture.color_space))
            .collect::<Vec<_>>();
        validate_material(material, &spaces)?;
        validate_shader_material(material, &spaces, &self.surface_shaders)?;
        let gpu_material = self.upload_material(material)?;
        if let Some(shader) = material.shader {
            let shader = self.surface_shaders[shader.0]
                .as_mut()
                .expect("shader was validated before material upload");
            shader.references = shader.references.saturating_add(1);
        }
        self.materials.push(Some(gpu_material));
        Ok(MaterialId(self.materials.len() - 1))
    }

    /// Compiles a user surface and atomically creates its four scene pipelines.
    pub fn create_surface_shader(
        &mut self,
        source: &SurfaceShaderSource,
    ) -> Result<ShaderId, RenderError> {
        validate_surface_shader_source(source)?;
        let compiled = compile_surface_shader(source)
            .map_err(|error| RenderError::InvalidShader(error.to_string()))?;
        let pipelines = self.create_surface_pipelines(&compiled)?;
        self.surface_shaders.push(Some(GpuSurfaceShader {
            device: self.device()?.clone(),
            pipelines,
            properties: source.properties.clone(),
            references: 0,
        }));
        Ok(ShaderId(self.surface_shaders.len() - 1))
    }

    /// Retires an unused shader. Materials must be destroyed before their shader.
    pub fn destroy_surface_shader(&mut self, shader: ShaderId) -> Result<(), RenderError> {
        let live = self
            .surface_shaders
            .get(shader.0)
            .and_then(Option::as_ref)
            .ok_or_else(|| {
                RenderError::InvalidShader("ShaderId не принадлежит renderer".to_owned())
            })?;
        validate_shader_destroyable(live.references)?;
        let resource = self.surface_shaders[shader.0].take().ok_or_else(|| {
            RenderError::InvalidShader("ShaderId не принадлежит renderer".to_owned())
        })?;
        self.retired
            .retire(RetiredResource::SurfaceShader(Box::new(resource)), 0);
        Ok(())
    }

    /// Удаляет неизменяемый материал после завершения его кадровых обращений.
    pub fn destroy_material(&mut self, material: MaterialId) -> Result<(), RenderError> {
        if material.0 == DEFAULT_MATERIAL {
            return Err(RenderError::InvalidMaterial(
                "встроенный материал нельзя удалить".to_owned(),
            ));
        }
        let resource = self
            .materials
            .get_mut(material.0)
            .and_then(Option::take)
            .ok_or_else(|| {
                RenderError::InvalidMaterial("MaterialId не принадлежит renderer".to_owned())
            })?;
        if let Some(shader) = resource.shader
            && let Some(Some(shader)) = self.surface_shaders.get_mut(shader.0)
        {
            shader.references = shader.references.saturating_sub(1);
        }
        self.retired
            .retire(RetiredResource::Material(Box::new(resource)), 0);
        Ok(())
    }

    /// Отправляет её ожидающую загрузку при необходимости; image ждёт upload и кадров перед освобождением.
    pub fn destroy_texture(&mut self, texture: TextureId) -> Result<(), RenderError> {
        let unknown =
            || RenderError::InvalidTexture("TextureId не принадлежит renderer".to_owned());
        // Встроенная белая текстура не выдаётся наружу и нужна всем DrawCommand без текстуры.
        if texture.0 <= BLACK_TEXTURE {
            return Err(unknown());
        }
        let upload_serial = self
            .textures
            .get(texture.0)
            .and_then(Option::as_ref)
            .map(|texture| texture.upload_serial)
            .ok_or_else(unknown)?;
        self.flush_upload_serial(upload_serial)?;
        let resource = self
            .textures
            .get_mut(texture.0)
            .and_then(Option::take)
            .ok_or_else(unknown)?;
        self.retired
            .retire(RetiredResource::Texture(resource), upload_serial);
        Ok(())
    }

    /// Наибольшая сторона изображения, поддерживаемая GPU.
    pub fn max_texture_size(&self) -> u32 {
        self.max_texture_size
    }

    /// Записывает UI-текстуру RGBA8 в upload batch: предумноженный цвет в гамма-пространстве, строки сверху вниз.
    /// Ошибка отправки batch может вернуться из следующего resource API или кадра.
    pub fn create_ui_texture(
        &mut self,
        width: u32,
        height: u32,
        rgba: &[u8],
    ) -> Result<UiTextureId, RenderError> {
        validate_texture(width, height, rgba.len(), self.max_texture_size)?;
        let sampler = self
            .ui_sampler
            .ok_or(RenderError::InvalidState("UI sampler отсутствует"))?;
        let device = self.device()?.clone();
        let image_set_layout = self.texture_set_layout()?;
        let (image, upload_serial) =
            self.upload_image(UI_TEXTURE_FORMAT, 1, width, height, rgba)?;
        let (pool, set) = match allocate_image_set(&device, image_set_layout, image.view, sampler) {
            Ok(resources) => resources,
            Err(error) => {
                self.retired
                    .retire(RetiredResource::Image(Box::new(image)), upload_serial);
                return Err(error);
            }
        };
        self.ui_textures.push(Some(UiTexture {
            device,
            pool,
            set,
            image,
            extent: vk::Extent2D { width, height },
            upload_serial,
        }));
        Ok(UiTextureId(self.ui_textures.len() - 1))
    }

    /// Перезаписывает область UI-текстуры и ждёт завершения копирования.
    ///
    /// Барьер в той же очереди упорядочивает запись относительно уже отправленных кадров.
    pub fn update_ui_texture(
        &mut self,
        texture: UiTextureId,
        region: [u32; 4],
        rgba: &[u8],
    ) -> Result<(), RenderError> {
        let target = self.ui_texture(texture)?;
        validate_texture_region(region, target.extent, rgba.len())?;
        let upload_serial = target.upload_serial;
        let image = target.image.image;
        self.flush_upload_serial(upload_serial)?;
        let device = self.device()?;
        let staging = HostBuffer::new(
            self.instance()?,
            device,
            self.physical_device,
            vk::BufferUsageFlags::TRANSFER_SRC,
            rgba,
        )?;
        let [x, y, width, height] = region;
        self.submit_once(|command_buffer| {
            // Прежний layout сохраняет остальную часть изображения: это важно для атласа шрифтов.
            let to_transfer = vk::ImageMemoryBarrier2::default()
                .src_stage_mask(vk::PipelineStageFlags2::FRAGMENT_SHADER)
                .src_access_mask(vk::AccessFlags2::SHADER_SAMPLED_READ)
                .dst_stage_mask(vk::PipelineStageFlags2::COPY)
                .dst_access_mask(vk::AccessFlags2::TRANSFER_WRITE)
                .old_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)
                .new_layout(vk::ImageLayout::TRANSFER_DST_OPTIMAL)
                .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .image(image)
                .subresource_range(color_subresource_range());
            pipeline_barrier(device, command_buffer, to_transfer);
            let copy = [vk::BufferImageCopy::default()
                .image_subresource(color_subresource_layers())
                .image_offset(vk::Offset3D {
                    x: x as i32,
                    y: y as i32,
                    z: 0,
                })
                .image_extent(vk::Extent3D {
                    width,
                    height,
                    depth: 1,
                })];
            // SAFETY: the region was validated against the image and staging holds its texels.
            unsafe {
                device.cmd_copy_buffer_to_image(
                    command_buffer,
                    staging.buffer,
                    image,
                    vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                    &copy,
                )
            };
            let to_shader = vk::ImageMemoryBarrier2::default()
                .src_stage_mask(vk::PipelineStageFlags2::COPY)
                .src_access_mask(vk::AccessFlags2::TRANSFER_WRITE)
                .dst_stage_mask(vk::PipelineStageFlags2::FRAGMENT_SHADER)
                .dst_access_mask(vk::AccessFlags2::SHADER_SAMPLED_READ)
                .old_layout(vk::ImageLayout::TRANSFER_DST_OPTIMAL)
                .new_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)
                .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .image(image)
                .subresource_range(color_subresource_range());
            pipeline_barrier(device, command_buffer, to_shader);
        })?;
        self.poll_uploads()?;
        Ok(())
    }

    /// Отправляет её ожидающую загрузку при необходимости; image ждёт upload и кадров перед освобождением.
    pub fn free_ui_texture(&mut self, texture: UiTextureId) -> Result<(), RenderError> {
        let upload_serial = self.ui_texture(texture)?.upload_serial;
        self.flush_upload_serial(upload_serial)?;
        if let Some(resource) = self.ui_textures[texture.0].take() {
            self.retired.retire(
                RetiredResource::UiTexture(Box::new(resource)),
                upload_serial,
            );
        }
        Ok(())
    }

    /// Компилирует пользовательский экранный шейдер (`vec3 rve_shade(vec2 uv)`) и создаёт его пайплайны
    /// для стадий `before_post` и `after_post`. Ошибка компиляции не меняет состояние renderer.
    pub fn create_screen_shader(&mut self, source: &str) -> Result<ScreenShaderId, RenderError> {
        let before = compile_screen_shader(source, ScreenShaderStage::BeforePost)
            .map_err(RenderError::ScreenShader)?;
        let after = compile_screen_shader(source, ScreenShaderStage::AfterPost)
            .map_err(RenderError::ScreenShader)?;
        let device = self.device()?.clone();
        let cache = self.pipeline_cache()?;
        if self.screen_shader_layout.is_none() {
            self.screen_shader_layout = Some(ScreenShaderLayout::new(&device)?);
        }
        let layout = self
            .screen_shader_layout
            .as_ref()
            .map_or(vk::PipelineLayout::null(), |layout| layout.pipeline_layout);
        let pipelines = ScreenShaderPipelines::new(
            &device,
            cache,
            layout,
            &screen_shader::spirv_bytes(&before),
            &screen_shader::spirv_bytes(&after),
        )?;
        self.screen_shaders.push(Some(pipelines));
        Ok(ScreenShaderId(self.screen_shaders.len() - 1))
    }

    /// Удаляет экранный шейдер; его пайплайны освобождаются после завершения кадров, которые могли их использовать.
    pub fn destroy_screen_shader(&mut self, shader: ScreenShaderId) -> Result<(), RenderError> {
        let resource = self
            .screen_shaders
            .get_mut(shader.0)
            .and_then(Option::take)
            .ok_or(RenderError::InvalidConfig("неизвестный экранный шейдер"))?;
        self.retired
            .retire(RetiredResource::ScreenShader(Box::new(resource)), 0);
        Ok(())
    }

    /// Создаёт 3D LUT цветокоррекции стороны `size` из `size³` узлов RGB; красный меняется быстрее
    /// всего, затем зелёный и синий (порядок `.cube`). Вход LUT — sRGB-кодированный цвет в `[0, 1]`.
    /// Загрузка синхронная; ошибка проверки не меняет состояние renderer.
    pub fn create_color_lut(
        &mut self,
        size: u32,
        texels: &[[f32; 3]],
    ) -> Result<ColorLutId, RenderError> {
        validate_color_lut(size, texels)?;
        let lut = self.upload_color_lut(size, &color_lut_bytes(texels))?;
        self.color_luts.push(Some(lut));
        Ok(ColorLutId(self.color_luts.len() - 1))
    }

    /// Удаляет LUT; изображение освобождается после кадров, которые могли его читать.
    /// Ссылка на удалённую LUT в `PostProcess` даёт `InvalidConfig`.
    pub fn destroy_color_lut(&mut self, lut: ColorLutId) -> Result<(), RenderError> {
        let unknown =
            || RenderError::InvalidTexture("ColorLutId не принадлежит renderer".to_owned());
        // Встроенная identity LUT не выдаётся наружу.
        if lut.0 == 0 {
            return Err(unknown());
        }
        let image = self
            .color_luts
            .get_mut(lut.0)
            .and_then(Option::take)
            .ok_or_else(unknown)?;
        self.retired
            .retire(RetiredResource::Image(Box::new(image)), 0);
        Ok(())
    }

    /// Проверяет параметры экранных шейдеров вида и то, что каждый проход ссылается на живой шейдер.
    pub(crate) fn validate_screen_shaders(
        &self,
        shaders: &ScreenShaders,
    ) -> Result<(), RenderError> {
        shaders.validate().map_err(RenderError::InvalidConfig)?;
        let known = shaders.all_known(|shader| {
            self.screen_shaders
                .get(shader.0)
                .is_some_and(Option::is_some)
        });
        if known {
            Ok(())
        } else {
            Err(RenderError::InvalidConfig(
                "экранный шейдер не создан или уничтожен",
            ))
        }
    }

    /// Встроенная identity LUT (`ColorLutId` 0): composite всегда читает binding 4.
    pub(crate) fn create_identity_color_lut(&mut self) -> Result<(), RenderError> {
        let lut = self.upload_color_lut(
            IDENTITY_LUT_SIZE,
            &color_lut_bytes(&identity_color_lut(IDENTITY_LUT_SIZE)),
        )?;
        self.color_luts.push(Some(lut));
        Ok(())
    }

    /// Загружает 3D LUT RGBA16F стороны `size` одной синхронной отправкой.
    pub(crate) fn upload_color_lut(
        &self,
        size: u32,
        bytes: &[u8],
    ) -> Result<DeviceImage, RenderError> {
        let device = self.device()?;
        let image = DeviceImage::new_3d(
            self.instance()?,
            device,
            self.physical_device,
            size,
            COLOR_LUT_FORMAT,
            vk::ImageUsageFlags::SAMPLED | vk::ImageUsageFlags::TRANSFER_DST,
        )?;
        let staging = HostBuffer::new(
            self.instance()?,
            device,
            self.physical_device,
            vk::BufferUsageFlags::TRANSFER_SRC,
            bytes,
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
                    width: size,
                    height: size,
                    depth: size,
                })];
            // SAFETY: staging holds size³ RGBA16F texels and the image is transfer-dst.
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
                    vk::PipelineStageFlags2::FRAGMENT_SHADER,
                    vk::AccessFlags2::SHADER_SAMPLED_READ,
                    color_subresource_range(),
                ),
            );
        })?;
        Ok(image)
    }

    /// Проверяет, что LUT цветокоррекции вида создана и не уничтожена.
    pub(crate) fn validate_color_lut(&self, post: &PostProcess) -> Result<(), RenderError> {
        match post.color_grading.lut {
            Some(lut) if lut.0 == 0 || color_lut_view(&self.color_luts, Some(lut)).is_none() => {
                Err(RenderError::InvalidConfig(
                    "LUT цветокоррекции не создана или уничтожена",
                ))
            }
            _ => Ok(()),
        }
    }
}
