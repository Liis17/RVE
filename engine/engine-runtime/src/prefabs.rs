//! Тяжёлые префабы проекта: CPU-загрузка в потоке `rve-prefabs`, GPU-загрузка на render thread
//! бюджетными шагами и освобождение по токену `(путь, load_id)`.
//!
//! Владение GPU-ресурсами. Модель (меши с материалами по умолчанию и текстуры этих материалов)
//! подсчитывается ссылками токенов, поэтому два префаба с одним FBX делят меши. Материалы
//! переопределений и ссылки на текстуры принадлежат токену. Освобождение токена уменьшает
//! ссылки, а ресурс, на который больше никто не ссылается, уничтожается.

use crate::module_host::ModuleHost;
use crate::pipeline::ScriptControl;
use crate::scene::{
    MaterialTextureSource, ObjectModel, ParticleSource, ResolvedMaterial, load_prefab_template,
    model_geometry, prepare_template, resolve_material, resolve_material_asset,
};
use engine_assets::{Assets, Model, ModelMaterial, TextureData};
use engine_files::VirtualPath;
use engine_physics::ModelGeometry;
use engine_render::{
    MaterialId, MeshId, ParticleEmitterDesc, RenderError, Renderer, ShaderId, SurfaceShaderSource,
    TextureColorSpace, TextureId, Vertex,
};
use engine_resources::ResourceHandle;
use engine_scene::{MaterialAsset, ObjectHandle, PrefabTemplate, ShaderAsset};
use flume::{Receiver, Sender};
use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::fmt::Debug;
use std::hash::Hash;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;

/// Имя потока загрузчика тяжёлых префабов.
pub(crate) const PREFAB_LOADER_THREAD: &str = "rve-prefabs";
/// Шагов GPU-загрузки (мешей и материалов) за одну итерацию render loop. Пока есть работа,
/// цикл не блокируется, поэтому большой префаб продолжается на следующих итерациях.
pub(crate) const PREFAB_UPLOAD_BUDGET: usize = 8;

/// Идентификатор одной загрузки префаба. Новая загрузка того же пути получает новый `load_id`.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub(crate) struct PrefabToken {
    pub path: String,
    pub load_id: u64,
}

/// Заявка загрузчику: загрузить токен или остановиться.
pub(crate) enum LoaderRequest {
    Load(PrefabToken),
    Shutdown,
}

/// CPU-часть готового префаба, которую получает scripts thread.
#[derive(Debug)]
pub(crate) struct PrefabCpu {
    pub template: Arc<PrefabTemplate>,
    pub sources: BTreeMap<String, String>,
    pub geometry: Vec<ModelGeometry>,
}

/// Результат CPU-загрузки. Модели и текстуры нужны только render thread для GPU.
pub(crate) struct LoadedPrefab {
    pub token: PrefabToken,
    pub cpu: PrefabCpu,
    pub models: HashMap<String, Model>,
    pub textures: HashMap<String, TextureData>,
    pub material_assets: HashMap<String, Result<MaterialAsset, String>>,
    pub shader_assets: HashMap<String, Result<ShaderAsset, String>>,
    pub levels: Vec<Option<ObjectModel>>,
    /// Эмиттеры объектов шаблона; элемент `i` относится к объекту `i`.
    pub particles: Vec<Option<ParticleSource>>,
}

/// Запросы к render thread, кроме управления окном.
pub(crate) enum RenderResourceRequest {
    /// Загрузчик отдал CPU-ресурсы префаба.
    Upload(Box<LoadedPrefab>),
    /// Scripts thread освободил префаб; GPU-ресурсы уничтожаются, когда снимки с `seq ≥ last_seq`
    /// обработаны.
    Release { token: PrefabToken, last_seq: u64 },
    /// Эмиттеры частиц: создание для объекта или уничтожение по объектам (поддеревья удалены).
    Particle(ParticleRequest),
}

/// Заявка на эмиттер частиц объекта. Эмиттер живёт в render thread под хендлом объекта.
pub(crate) enum ParticleRequest {
    Create {
        object: ObjectHandle,
        desc: Box<ParticleEmitterDesc>,
    },
    Destroy(Vec<ObjectHandle>),
}

/// Готовый префаб: GPU-уровни шаблона по объектам и CPU-часть.
#[derive(Debug)]
pub(crate) struct PrefabReadyOf<M, Mt> {
    pub token: PrefabToken,
    /// Элемент `i` относится к объекту шаблона `i`; уровни объекта — список мешей с материалами.
    pub gpu: Vec<Vec<Vec<(M, Mt)>>>,
    /// Описания эмиттеров по объектам шаблона; их текстуры живут, пока токен не освобождён.
    pub particles: Vec<Option<ParticleEmitterDesc>>,
    pub cpu: PrefabCpu,
}

/// Ответ scripts thread на загрузку префаба.
#[derive(Debug)]
pub(crate) enum PrefabReplyOf<M, Mt> {
    Ready(Box<PrefabReadyOf<M, Mt>>),
    Failed { token: PrefabToken, error: String },
}

pub(crate) type PrefabReady = PrefabReadyOf<MeshId, MaterialId>;
pub(crate) type PrefabReply = PrefabReplyOf<MeshId, MaterialId>;

/// Причина неуспешной CPU-загрузки. `Interrupted` означает остановку, а не ошибку префаба.
#[derive(Debug)]
pub(crate) enum LoadError {
    Interrupted,
    Failed(String),
}

/// Текстуры слотов материала уже созданы в GPU.
pub(crate) struct MaterialTextures<T> {
    pub base_color: Option<T>,
    pub metallic_roughness: Option<T>,
    pub normal: Option<T>,
    pub emissive: Option<T>,
    pub occlusion: Option<T>,
}

/// GPU-операции, нужные загрузке префабов. В `Renderer` это прямые вызовы; тесты подставляют
/// учёт ресурсов.
pub(crate) trait GpuResources {
    type Mesh: Copy + Eq + Hash + Debug;
    type Texture: Copy + Eq + Hash + Debug;
    type Material: Copy + Eq + Hash + Debug;
    type Shader: Copy + Eq + Hash + Debug;

    fn create_mesh(
        &mut self,
        vertices: &[Vertex],
        indices: &[u32],
    ) -> Result<Self::Mesh, RenderError>;
    fn create_texture(
        &mut self,
        texture: &TextureData,
        color_space: TextureColorSpace,
    ) -> Result<Self::Texture, RenderError>;
    fn create_material(
        &mut self,
        resolved: &ResolvedMaterial,
        textures: MaterialTextures<Self::Texture>,
        shader: Option<Self::Shader>,
        user_textures: [Option<Self::Texture>; 8],
    ) -> Result<Self::Material, RenderError>;
    fn create_surface_shader(
        &mut self,
        source: &SurfaceShaderSource,
    ) -> Result<Self::Shader, RenderError>;
    fn destroy_surface_shader(&mut self, shader: Self::Shader) -> Result<(), RenderError>;
    fn destroy_mesh(&mut self, mesh: Self::Mesh) -> Result<(), RenderError>;
    fn destroy_texture(&mut self, texture: Self::Texture) -> Result<(), RenderError>;
    fn destroy_material(&mut self, material: Self::Material) -> Result<(), RenderError>;
}

impl GpuResources for Renderer {
    type Mesh = MeshId;
    type Texture = TextureId;
    type Material = MaterialId;
    type Shader = ShaderId;

    fn create_mesh(&mut self, vertices: &[Vertex], indices: &[u32]) -> Result<MeshId, RenderError> {
        Renderer::create_mesh(self, vertices, indices)
    }

    fn create_texture(
        &mut self,
        texture: &TextureData,
        color_space: TextureColorSpace,
    ) -> Result<TextureId, RenderError> {
        Renderer::create_texture_with_color_space(
            self,
            texture.width,
            texture.height,
            &texture.rgba,
            color_space,
        )
    }

    fn create_material(
        &mut self,
        resolved: &ResolvedMaterial,
        textures: MaterialTextures<TextureId>,
        shader: Option<ShaderId>,
        user_textures: [Option<TextureId>; 8],
    ) -> Result<MaterialId, RenderError> {
        Renderer::create_material(
            self,
            &engine_render::Material {
                shader,
                user_values: resolved.user_values,
                user_textures,
                base_color: resolved.base_color,
                metallic: resolved.metallic,
                roughness: resolved.roughness,
                emissive: resolved.emissive,
                opacity: resolved.opacity,
                alpha_mode: resolved.alpha_mode,
                double_sided: resolved.double_sided,
                base_color_texture: textures.base_color,
                metallic_roughness_texture: textures.metallic_roughness,
                normal_texture: textures.normal,
                emissive_texture: textures.emissive,
                occlusion_texture: textures.occlusion,
                normal_scale: resolved.normal_scale,
                occlusion_strength: resolved.occlusion_strength,
            },
        )
    }

    fn create_surface_shader(
        &mut self,
        source: &SurfaceShaderSource,
    ) -> Result<ShaderId, RenderError> {
        Renderer::create_surface_shader(self, source)
    }

    fn destroy_surface_shader(&mut self, shader: ShaderId) -> Result<(), RenderError> {
        Renderer::destroy_surface_shader(self, shader)
    }

    fn destroy_mesh(&mut self, mesh: MeshId) -> Result<(), RenderError> {
        Renderer::destroy_mesh(self, mesh)
    }

    fn destroy_texture(&mut self, texture: TextureId) -> Result<(), RenderError> {
        Renderer::destroy_texture(self, texture)
    }

    fn destroy_material(&mut self, material: MaterialId) -> Result<(), RenderError> {
        Renderer::destroy_material(self, material)
    }
}

/// Ключ GPU-текстуры: текстура модели по индексу или текстура сцены по пути.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
enum TextureKey {
    Model {
        model: String,
        index: usize,
        space: TextureColorSpace,
    },
    Scene {
        path: String,
        space: TextureColorSpace,
    },
}

struct SharedModel<G: GpuResources> {
    /// Меши модели с материалами по умолчанию, в порядке мешей модели.
    meshes: Vec<(G::Mesh, G::Material)>,
    /// Текстуры, на которые ссылаются материалы по умолчанию; владеет модель.
    textures: Vec<TextureKey>,
    refs: usize,
}

struct SharedTexture<G: GpuResources> {
    id: G::Texture,
    refs: usize,
}

/// Ресурсы, которыми владеет один токен.
struct TokenGpu<G: GpuResources> {
    models: Vec<String>,
    materials: Vec<G::Material>,
    textures: Vec<TextureKey>,
    shaders: Vec<String>,
    error_material: Option<G::Material>,
}

impl<G: GpuResources> Default for TokenGpu<G> {
    fn default() -> Self {
        Self {
            models: Vec::new(),
            materials: Vec::new(),
            textures: Vec::new(),
            shaders: Vec::new(),
            error_material: None,
        }
    }
}

/// Шаг GPU-загрузки. Индексы относятся к CPU-данным задания.
enum UploadStep {
    /// Меш модели и его материал по умолчанию.
    Mesh { model: String, index: usize },
    /// Материал уровня объекта с переопределением; меш берётся из модели.
    Override {
        object: usize,
        level: usize,
        mesh: usize,
    },
}

/// Пары «меш и материал», которые рисуют один уровень объекта.
type Draws<G> = Vec<(<G as GpuResources>::Mesh, <G as GpuResources>::Material)>;

struct UploadJob<G: GpuResources> {
    prefab: LoadedPrefab,
    token: TokenGpu<G>,
    /// `None`, пока задание не начато: модели подключаются при старте, когда предыдущие задания
    /// уже завершены, поэтому частично загруженная модель никому не отдаётся.
    steps: Option<VecDeque<UploadStep>>,
    overrides: HashMap<(usize, usize), Draws<G>>,
}

/// GPU-учёт тяжёлых префабов: очередь заданий, модели, текстуры и токены.
pub(crate) struct PrefabGpu<G: GpuResources> {
    models: HashMap<String, SharedModel<G>>,
    textures: HashMap<TextureKey, SharedTexture<G>>,
    shaders: HashMap<String, Result<SharedShader<G::Shader>, String>>,
    tokens: HashMap<PrefabToken, TokenGpu<G>>,
    queue: VecDeque<UploadJob<G>>,
}

impl<G: GpuResources> Default for PrefabGpu<G> {
    fn default() -> Self {
        Self {
            models: HashMap::new(),
            textures: HashMap::new(),
            shaders: HashMap::new(),
            tokens: HashMap::new(),
            queue: VecDeque::new(),
        }
    }
}

struct SharedShader<S> {
    id: S,
    refs: usize,
}

impl<G: GpuResources> PrefabGpu<G> {
    /// Ставит префаб в очередь GPU-загрузки. Возвращает ответ сразу, если токен уже занят.
    pub fn submit(&mut self, prefab: LoadedPrefab) -> Option<PrefabReplyOf<G::Mesh, G::Material>> {
        let busy = self.tokens.contains_key(&prefab.token)
            || self
                .queue
                .iter()
                .any(|job| job.prefab.token == prefab.token);
        if busy {
            return Some(PrefabReplyOf::Failed {
                token: prefab.token.clone(),
                error: "загрузка префаба с таким идентификатором уже выполняется".to_owned(),
            });
        }
        self.queue.push_back(UploadJob {
            prefab,
            token: TokenGpu::default(),
            steps: None,
            overrides: HashMap::new(),
        });
        None
    }

    pub fn has_pending(&self) -> bool {
        !self.queue.is_empty()
    }

    /// Выполняет до `budget` шагов и возвращает законченные задания. Незавершённое задание
    /// остаётся первым в очереди и продолжается при следующем вызове.
    pub fn advance(
        &mut self,
        gpu: &mut G,
        mut budget: usize,
    ) -> Vec<PrefabReplyOf<G::Mesh, G::Material>> {
        let mut replies = Vec::new();
        while let Some(mut job) = self.queue.pop_front() {
            match self.run_job(gpu, &mut job, &mut budget) {
                Ok(true) => replies.push(self.finish(job)),
                Ok(false) => {
                    self.queue.push_front(job);
                    break;
                }
                Err(error) => {
                    let token = job.prefab.token.clone();
                    self.release_token(gpu, job.token);
                    replies.push(PrefabReplyOf::Failed { token, error });
                }
            }
        }
        replies
    }

    /// Освобождает ресурсы токена. Неизвестный токен ничего не делает: повторное освобождение
    /// безопасно. Задание в очереди снимается и его частичные ресурсы освобождаются.
    pub fn release(&mut self, gpu: &mut G, token: &PrefabToken) {
        if let Some(index) = self.queue.iter().position(|job| &job.prefab.token == token)
            && let Some(job) = self.queue.remove(index)
        {
            self.release_token(gpu, job.token);
        }
        if let Some(token_gpu) = self.tokens.remove(token) {
            self.release_token(gpu, token_gpu);
        }
    }

    /// Возвращает `Ok(true)`, если задание выполнено, и `Ok(false)`, если кончился бюджет.
    fn run_job(
        &mut self,
        gpu: &mut G,
        job: &mut UploadJob<G>,
        budget: &mut usize,
    ) -> Result<bool, String> {
        if job.steps.is_none() {
            job.steps = Some(self.activate(job)?);
        }
        loop {
            if job.steps.as_ref().is_some_and(VecDeque::is_empty) {
                return Ok(true);
            }
            if *budget == 0 {
                return Ok(false);
            }
            *budget -= 1;
            let Some(step) = job.steps.as_mut().and_then(VecDeque::pop_front) else {
                return Ok(true);
            };
            self.run_step(gpu, job, step)
                .map_err(|error| error.to_string())?;
        }
    }

    /// Подключает модели задания и строит шаги. Общие модели уже готовы; новые получают пустую
    /// запись, которую заполнят шаги мешей.
    fn activate(&mut self, job: &mut UploadJob<G>) -> Result<VecDeque<UploadStep>, String> {
        let prefab = &job.prefab;
        let token = &mut job.token;
        let mut steps = VecDeque::new();
        let mut seen = HashSet::new();
        for object in prefab.levels.iter().flatten() {
            for (model_path, _) in &object.levels {
                if !seen.insert(model_path.as_str()) {
                    continue;
                }
                let Some(model) = prefab.models.get(model_path) else {
                    return Err(format!("CPU-модель {model_path:?} отсутствует"));
                };
                token.models.push(model_path.clone());
                if let Some(entry) = self.models.get_mut(model_path) {
                    entry.refs += 1;
                    continue;
                }
                self.models.insert(
                    model_path.clone(),
                    SharedModel {
                        meshes: Vec::new(),
                        textures: Vec::new(),
                        refs: 1,
                    },
                );
                steps.extend((0..model.meshes.len()).map(|index| UploadStep::Mesh {
                    model: model_path.clone(),
                    index,
                }));
            }
        }
        for (object_index, object) in prefab.levels.iter().enumerate() {
            let Some(object) = object else { continue };
            for (level_index, (model_path, replacement)) in object.levels.iter().enumerate() {
                if !needs_override(object, replacement) {
                    continue;
                }
                let count = prefab.models[model_path].meshes.len();
                steps.extend((0..count).filter_map(|mesh| {
                    needs_mesh_override(object, replacement, mesh).then_some(UploadStep::Override {
                        object: object_index,
                        level: level_index,
                        mesh,
                    })
                }));
            }
        }
        Ok(steps)
    }

    fn run_step(
        &mut self,
        gpu: &mut G,
        job: &mut UploadJob<G>,
        step: UploadStep,
    ) -> Result<(), RenderError> {
        match step {
            UploadStep::Mesh { model, index } => {
                let prefab = &job.prefab;
                let model_cpu = prefab.models.get(&model).ok_or(RenderError::InvalidState(
                    "CPU-модель отсутствует в таблице подготовленных ресурсов",
                ))?;
                let mesh = &model_cpu.meshes[index];
                let vertices = gpu_vertices(mesh);
                let mesh_id = gpu.create_mesh(&vertices, &mesh.indices)?;
                let resolved = resolve_material(&mesh.material, None, None);
                let sources = TextureSources {
                    model_path: &model,
                    model: model_cpu,
                    scene: &prefab.textures,
                };
                let mut keys = Vec::new();
                match self.build_material(gpu, &resolved, &sources, &mut job.token, &mut keys) {
                    Ok(material) => {
                        let entry =
                            self.models
                                .get_mut(&model)
                                .ok_or(RenderError::InvalidState(
                                    "запись модели отсутствует при загрузке мешей",
                                ))?;
                        entry.meshes.push((mesh_id, material));
                        entry.textures.extend(keys);
                        Ok(())
                    }
                    Err(error) => {
                        self.release_keys(gpu, &keys);
                        report(gpu.destroy_mesh(mesh_id));
                        Err(error)
                    }
                }
            }
            UploadStep::Override {
                object,
                level,
                mesh,
            } => {
                let prefab = &job.prefab;
                let object_model =
                    prefab.levels[object]
                        .as_ref()
                        .ok_or(RenderError::InvalidState(
                            "уровень объекта без меша в шаблоне префаба",
                        ))?;
                let (model_path, replacement) = &object_model.levels[level];
                let model_cpu = prefab
                    .models
                    .get(model_path)
                    .ok_or(RenderError::InvalidState(
                        "CPU-модель отсутствует в таблице подготовленных ресурсов",
                    ))?;
                let source = &model_cpu.meshes[mesh];
                let mesh_id = self
                    .models
                    .get(model_path)
                    .and_then(|entry| entry.meshes.get(mesh))
                    .map(|(mesh_id, _)| *mesh_id)
                    .ok_or(RenderError::InvalidState(
                        "меш модели не создан до материала переопределения",
                    ))?;
                let slot_path = object_model
                    .material_slots
                    .get(mesh)
                    .and_then(Option::as_ref);
                if slot_path.is_none() && object_model.material.is_none() && replacement.is_none() {
                    return Ok(());
                }
                let resolved = match slot_path {
                    Some(path) => resolve_prefab_material(
                        prefab,
                        path,
                        &source.material,
                        replacement.as_deref(),
                        object_model.material.as_ref(),
                    ),
                    None => Ok(resolve_material(
                        &source.material,
                        replacement.as_deref(),
                        object_model.material.as_ref(),
                    )),
                };
                let sources = TextureSources {
                    model_path,
                    model: model_cpu,
                    scene: &prefab.textures,
                };
                let mut keys = Vec::new();
                let built = resolved
                    .map_err(RenderError::InvalidMaterial)
                    .and_then(|resolved| {
                        self.build_material(gpu, &resolved, &sources, &mut job.token, &mut keys)
                    });
                let material = match built {
                    Ok(material) => material,
                    Err(error) if slot_path.is_some() => {
                        self.release_keys(gpu, &keys);
                        if let Some(path) = slot_path {
                            log::error!(
                                "[material] /{path}: {error}; используется материал ошибки"
                            );
                        }
                        self.error_material(gpu, &mut job.token)?
                    }
                    Err(error) => {
                        self.release_keys(gpu, &keys);
                        return Err(error);
                    }
                };
                if job.token.error_material != Some(material) {
                    job.token.materials.push(material);
                }
                job.token.textures.extend(keys);
                let draws = job.overrides.entry((object, level)).or_insert_with(|| {
                    self.models
                        .get(model_path)
                        .map(|entry| entry.meshes.clone())
                        .unwrap_or_default()
                });
                if let Some(draw) = draws.get_mut(mesh) {
                    *draw = (mesh_id, material);
                }
                Ok(())
            }
        }
    }

    /// Создаёт материал, получая текстуры слотов через учёт ссылок. Ключи полученных текстур
    /// попадают в `keys`, даже если создание материала не удалось.
    fn build_material(
        &mut self,
        gpu: &mut G,
        resolved: &ResolvedMaterial,
        sources: &TextureSources<'_>,
        token: &mut TokenGpu<G>,
        keys: &mut Vec<TextureKey>,
    ) -> Result<G::Material, RenderError> {
        let textures = MaterialTextures {
            base_color: self.acquire_slot(
                gpu,
                sources,
                resolved.base_color_texture.as_ref(),
                TextureColorSpace::Srgb,
                keys,
            )?,
            metallic_roughness: self.acquire_slot(
                gpu,
                sources,
                resolved.metallic_roughness_texture.as_ref(),
                TextureColorSpace::Linear,
                keys,
            )?,
            normal: self.acquire_slot(
                gpu,
                sources,
                resolved.normal_texture.as_ref(),
                TextureColorSpace::Linear,
                keys,
            )?,
            emissive: self.acquire_slot(
                gpu,
                sources,
                resolved.emissive_texture.as_ref(),
                TextureColorSpace::Srgb,
                keys,
            )?,
            occlusion: self.acquire_slot(
                gpu,
                sources,
                resolved.occlusion_texture.as_ref(),
                TextureColorSpace::Linear,
                keys,
            )?,
        };
        let mut user_textures = [None; 8];
        for (slot, source) in resolved.user_texture_sources.iter().enumerate() {
            let Some(source) = source else { continue };
            user_textures[slot] = self.acquire_slot(
                gpu,
                sources,
                Some(&MaterialTextureSource::Scene(source.path.clone())),
                source.color_space,
                keys,
            )?;
        }
        let shader = match (&resolved.shader_path, &resolved.shader_source) {
            (Some(path), Some(source)) => Some(self.acquire_shader(gpu, path, source, token)?),
            _ => None,
        };
        match gpu.create_material(resolved, textures, shader, user_textures) {
            Ok(material) => Ok(material),
            Err(error) => {
                if let Some(path) = &resolved.shader_path {
                    if token.shaders.last().is_some_and(|last| last == path) {
                        token.shaders.pop();
                    }
                    self.release_shader(gpu, path);
                }
                Err(error)
            }
        }
    }

    fn acquire_shader(
        &mut self,
        gpu: &mut G,
        path: &str,
        source: &SurfaceShaderSource,
        token: &mut TokenGpu<G>,
    ) -> Result<G::Shader, RenderError> {
        if !self.shaders.contains_key(path) {
            let shader = gpu
                .create_surface_shader(source)
                .map(|id| SharedShader { id, refs: 0 })
                .map_err(|error| error.to_string());
            self.shaders.insert(path.to_owned(), shader);
        }
        let entry = self
            .shaders
            .get_mut(path)
            .ok_or(RenderError::InvalidState("shader cache insertion failed"))?;
        let shader = entry
            .as_mut()
            .map_err(|error| RenderError::InvalidShader(format!("/{}: {}", path, error)))?;
        shader.refs += 1;
        token.shaders.push(path.to_owned());
        Ok(shader.id)
    }

    fn release_shader(&mut self, gpu: &mut G, path: &str) {
        let Some(Ok(entry)) = self.shaders.get_mut(path) else {
            return;
        };
        entry.refs = entry.refs.saturating_sub(1);
        if entry.refs != 0 {
            return;
        }
        if let Some(Ok(entry)) = self.shaders.remove(path) {
            report(gpu.destroy_surface_shader(entry.id));
        }
    }

    fn error_material(
        &mut self,
        gpu: &mut G,
        token: &mut TokenGpu<G>,
    ) -> Result<G::Material, RenderError> {
        if let Some(material) = token.error_material {
            return Ok(material);
        }
        let mut resolved = resolve_material(&ModelMaterial::default(), None, None);
        resolved.base_color = [1.0, 0.0, 1.0];
        let material = gpu.create_material(
            &resolved,
            MaterialTextures {
                base_color: None,
                metallic_roughness: None,
                normal: None,
                emissive: None,
                occlusion: None,
            },
            None,
            [None; 8],
        )?;
        token.error_material = Some(material);
        Ok(material)
    }

    fn acquire_slot(
        &mut self,
        gpu: &mut G,
        sources: &TextureSources<'_>,
        source: Option<&MaterialTextureSource>,
        color_space: TextureColorSpace,
        keys: &mut Vec<TextureKey>,
    ) -> Result<Option<G::Texture>, RenderError> {
        let Some(source) = source else {
            return Ok(None);
        };
        let (key, data) = match source {
            MaterialTextureSource::Model(index) => {
                let data = sources.model.textures.get(*index).ok_or_else(|| {
                    RenderError::InvalidTexture(format!(
                        "модель {:?} ссылается на отсутствующую текстуру {index}",
                        sources.model_path
                    ))
                })?;
                (
                    TextureKey::Model {
                        model: sources.model_path.to_owned(),
                        index: *index,
                        space: color_space,
                    },
                    data,
                )
            }
            MaterialTextureSource::Scene(path) => {
                let data = sources.scene.get(path).ok_or(RenderError::InvalidState(
                    "CPU-текстура отсутствует в таблице GPU-ресурсов",
                ))?;
                (
                    TextureKey::Scene {
                        path: path.clone(),
                        space: color_space,
                    },
                    data,
                )
            }
        };
        if let Some(entry) = self.textures.get_mut(&key) {
            entry.refs += 1;
            let id = entry.id;
            keys.push(key);
            return Ok(Some(id));
        }
        let id = gpu.create_texture(data, color_space)?;
        self.textures
            .insert(key.clone(), SharedTexture { id, refs: 1 });
        keys.push(key);
        Ok(Some(id))
    }

    /// Собирает ответ: уровни объектов берут меши модели или материалы переопределений.
    fn finish(&mut self, job: UploadJob<G>) -> PrefabReplyOf<G::Mesh, G::Material> {
        let UploadJob {
            prefab,
            token,
            mut overrides,
            ..
        } = job;
        let mut gpu_objects = Vec::with_capacity(prefab.levels.len());
        for (object_index, object) in prefab.levels.iter().enumerate() {
            let mut levels = Vec::new();
            if let Some(object) = object {
                for (level_index, (model_path, replacement)) in object.levels.iter().enumerate() {
                    if needs_override(object, replacement) {
                        levels.push(
                            overrides
                                .remove(&(object_index, level_index))
                                .unwrap_or_else(|| {
                                    self.models
                                        .get(model_path)
                                        .map(|entry| entry.meshes.clone())
                                        .unwrap_or_default()
                                }),
                        );
                    } else {
                        levels.push(
                            self.models
                                .get(model_path)
                                .map(|entry| entry.meshes.clone())
                                .unwrap_or_default(),
                        );
                    }
                }
            }
            gpu_objects.push(levels);
        }
        let token_key = prefab.token.clone();
        self.tokens.insert(token_key.clone(), token);
        PrefabReplyOf::Ready(Box::new(PrefabReadyOf {
            token: token_key,
            gpu: gpu_objects,
            particles: Vec::new(),
            cpu: prefab.cpu,
        }))
    }

    fn release_token(&mut self, gpu: &mut G, token: TokenGpu<G>) {
        for material in token.materials {
            report(gpu.destroy_material(material));
        }
        if let Some(material) = token.error_material {
            report(gpu.destroy_material(material));
        }
        for key in token.textures {
            self.release_texture(gpu, &key);
        }
        for path in token.models {
            self.release_model(gpu, &path);
        }
        for path in token.shaders {
            self.release_shader(gpu, &path);
        }
    }

    fn release_keys(&mut self, gpu: &mut G, keys: &[TextureKey]) {
        for key in keys {
            self.release_texture(gpu, key);
        }
    }

    fn release_model(&mut self, gpu: &mut G, path: &str) {
        let Some(entry) = self.models.get_mut(path) else {
            return;
        };
        entry.refs = entry.refs.saturating_sub(1);
        if entry.refs > 0 {
            return;
        }
        let Some(entry) = self.models.remove(path) else {
            return;
        };
        for (mesh, material) in entry.meshes {
            report(gpu.destroy_mesh(mesh));
            report(gpu.destroy_material(material));
        }
        for key in entry.textures {
            self.release_texture(gpu, &key);
        }
    }

    fn release_texture(&mut self, gpu: &mut G, key: &TextureKey) {
        let Some(entry) = self.textures.get_mut(key) else {
            return;
        };
        entry.refs = entry.refs.saturating_sub(1);
        if entry.refs > 0 {
            return;
        }
        if let Some(entry) = self.textures.remove(key) {
            report(gpu.destroy_texture(entry.id));
        }
    }
}

/// Материал переопределения нужен, если у объекта задана замена материала или текстуры уровня.
fn needs_override(object: &ObjectModel, replacement: &Option<String>) -> bool {
    object.material.is_some()
        || replacement.is_some()
        || object.material_slots.iter().any(Option::is_some)
}

fn needs_mesh_override(object: &ObjectModel, replacement: &Option<String>, mesh: usize) -> bool {
    object.material.is_some()
        || replacement.is_some()
        || object.material_slots.get(mesh).is_some_and(Option::is_some)
}

fn resolve_prefab_material(
    prefab: &LoadedPrefab,
    path: &str,
    model: &ModelMaterial,
    replacement_texture: Option<&str>,
    override_material: Option<&engine_scene::MaterialOverride>,
) -> Result<ResolvedMaterial, String> {
    let material = prefab
        .material_assets
        .get(path)
        .ok_or_else(|| format!("материал /{path} не был подготовлен"))?
        .as_ref()
        .map_err(Clone::clone)?;
    let shader = material
        .shader
        .as_ref()
        .map(|shader_path| {
            prefab
                .shader_assets
                .get(shader_path.as_str())
                .ok_or_else(|| format!("шейдер /{} не был подготовлен", shader_path))?
                .as_ref()
                .map_err(Clone::clone)
        })
        .transpose()?;
    resolve_material_asset(
        model,
        path,
        material,
        shader,
        replacement_texture,
        override_material,
    )
}

/// Пути данных текстур для одного материала: текстуры модели и текстуры сцены.
struct TextureSources<'a> {
    model_path: &'a str,
    model: &'a Model,
    scene: &'a HashMap<String, TextureData>,
}

fn gpu_vertices(mesh: &engine_assets::ModelMesh) -> Vec<Vertex> {
    mesh.vertices
        .iter()
        .map(|vertex| Vertex {
            position: vertex.position,
            normal: vertex.normal,
            uv: vertex.uv,
            color: [1.0; 3],
        })
        .collect()
}

fn report(result: Result<(), RenderError>) {
    if let Err(error) = result {
        log::warn!("освобождение GPU-ресурса префаба: {error}");
    }
}

/// Геометрия физики только для моделей мешевых коллайдеров шаблона: остальные модели нужны
/// лишь для отрисовки. Порядок — по пути модели.
fn prefab_collider_geometry(
    template: &PrefabTemplate,
    models: &HashMap<String, Model>,
) -> Result<Vec<ModelGeometry>, String> {
    template
        .mesh_collider_models()
        .into_iter()
        .map(|path| {
            let model = models
                .get(path)
                .ok_or_else(|| format!("модель коллайдера {path} не подготовлена"))?;
            Ok(model_geometry(path, model))
        })
        .collect()
}

/// Загружает префаб на CPU. Прерывание между объектами, моделями и исходниками даёт
/// [`LoadError::Interrupted`]; ошибки префаба — [`LoadError::Failed`] с текстом.
pub(crate) fn load_prefab(
    resources: &ResourceHandle,
    token: PrefabToken,
    stop: &AtomicBool,
) -> Result<LoadedPrefab, LoadError> {
    let interrupted = || stop.load(Ordering::Acquire);
    let classify = |error: String| {
        if interrupted() {
            LoadError::Interrupted
        } else {
            LoadError::Failed(error)
        }
    };
    let path = VirtualPath::parse(&token.path)
        .map_err(|_| LoadError::Failed(format!("некорректный путь префаба {:?}", token.path)))?;
    let template = load_prefab_template(resources, &path).map_err(classify)?;
    template
        .check_spawnable()
        .map_err(|error| LoadError::Failed(error.to_string()))?;
    let assets = Assets::with_resources(resources.clone());
    let prepared = prepare_template(resources, &assets, &token.path, &template, &interrupted)
        .map_err(classify)?;
    if interrupted() {
        return Err(LoadError::Interrupted);
    }
    let geometry =
        prefab_collider_geometry(&template, &prepared.models).map_err(LoadError::Failed)?;
    Ok(LoadedPrefab {
        token,
        cpu: PrefabCpu {
            template: Arc::new(template),
            sources: prepared.sources,
            geometry,
        },
        models: prepared.models,
        textures: prepared.textures,
        material_assets: prepared.material_assets,
        shader_assets: prepared.shader_assets,
        levels: prepared.levels,
        particles: prepared.particles,
    })
}

/// Запускает поток `rve-prefabs`. Возвращает канал заявок и канал сигнала о завершении потока.
pub(crate) fn start_prefab_loader(
    modules: &mut ModuleHost,
    resources: ResourceHandle,
    script_tx: Sender<ScriptControl>,
    render_tx: Sender<RenderResourceRequest>,
) -> std::io::Result<(Sender<LoaderRequest>, mpsc::Receiver<()>)> {
    let (requests_tx, requests_rx) = flume::unbounded();
    let (stopped_tx, stopped_rx) = mpsc::sync_channel(1);
    let stop = Arc::new(AtomicBool::new(false));
    let stop_flag = Arc::clone(&stop);
    let stop_requests = requests_tx.clone();
    modules.spawn(
        PREFAB_LOADER_THREAD,
        move || {
            stop_flag.store(true, Ordering::Release);
            let _ = stop_requests.send(LoaderRequest::Shutdown);
        },
        move || {
            run_loader(&requests_rx, &resources, &stop, &script_tx, &render_tx);
            let _ = stopped_tx.try_send(());
        },
    )?;
    Ok((requests_tx, stopped_rx))
}

fn run_loader(
    requests: &Receiver<LoaderRequest>,
    resources: &ResourceHandle,
    stop: &AtomicBool,
    script_tx: &Sender<ScriptControl>,
    render_tx: &Sender<RenderResourceRequest>,
) {
    while let Ok(request) = requests.recv() {
        let LoaderRequest::Load(token) = request else {
            break;
        };
        if stop.load(Ordering::Acquire) {
            break;
        }
        match load_prefab(resources, token.clone(), stop) {
            Ok(prefab) => {
                if render_tx
                    .send(RenderResourceRequest::Upload(Box::new(prefab)))
                    .is_err()
                {
                    break;
                }
            }
            Err(LoadError::Interrupted) => break,
            Err(LoadError::Failed(error)) => {
                let reply = PrefabReply::Failed { token, error };
                if script_tx.send(ScriptControl::Prefab(reply)).is_err() {
                    break;
                }
            }
        }
    }
}

/// Сторона scripts thread: какие токены текущие и какой снимок отправлен последним.
pub(crate) struct PrefabLoads {
    loader: Sender<LoaderRequest>,
    render: Sender<RenderResourceRequest>,
    next_load_id: u64,
    slots: HashMap<String, HeavySlot>,
    last_sent_seq: u64,
}

/// Состояние тяжёлого префаба на стороне скриптов.
pub(crate) struct HeavySlot {
    pub token: PrefabToken,
    /// Пути моделей геометрии коллайдеров; известны после готовности.
    pub geometry: Option<Vec<String>>,
}

impl PrefabLoads {
    pub fn new(loader: Sender<LoaderRequest>, render: Sender<RenderResourceRequest>) -> Self {
        Self {
            loader,
            render,
            next_load_id: 0,
            slots: HashMap::new(),
            last_sent_seq: 0,
        }
    }

    /// Номер последнего снимка, реально отправленного render thread.
    pub fn set_last_sent_seq(&mut self, seq: u64) {
        self.last_sent_seq = seq;
    }

    /// Отправляет заявку эмиттера render thread; закрытый канал означает остановку рендера.
    pub fn send_particles(&self, request: ParticleRequest) {
        let _ = self.render.send(RenderResourceRequest::Particle(request));
    }

    /// Запускает загрузку и возвращает ошибку, если загрузчик уже остановлен.
    pub fn start_load(&mut self, path: &str) -> Result<(), String> {
        self.next_load_id += 1;
        let token = PrefabToken {
            path: path.to_owned(),
            load_id: self.next_load_id,
        };
        self.loader
            .send(LoaderRequest::Load(token.clone()))
            .map_err(|_| "поток загрузки префабов остановлен".to_owned())?;
        self.slots.insert(
            path.to_owned(),
            HeavySlot {
                token,
                geometry: None,
            },
        );
        Ok(())
    }

    pub fn is_current(&self, token: &PrefabToken) -> bool {
        self.slots
            .get(&token.path)
            .is_some_and(|slot| &slot.token == token)
    }

    pub fn mark_ready(&mut self, token: &PrefabToken, geometry: Vec<String>) {
        if let Some(slot) = self.slots.get_mut(&token.path) {
            slot.geometry = Some(geometry);
        }
    }

    /// Снимает текущий слот пути, если он есть.
    pub fn forget(&mut self, path: &str) -> Option<HeavySlot> {
        self.slots.remove(path)
    }

    /// Просит render thread освободить токен после уже отправленных снимков.
    pub fn release(&self, token: PrefabToken) {
        let _ = self.render.send(RenderResourceRequest::Release {
            token,
            last_seq: self.last_sent_seq,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use engine_assets::{ModelAlphaMode, ModelMaterial, ModelMesh, ModelVertex};

    /// Учёт GPU без Vulkan: id выдаются по порядку, неживые освобождения фиксируются.
    #[derive(Default)]
    struct FakeGpu {
        next: u32,
        live: HashSet<u32>,
        created: usize,
        fail_material_at: Option<usize>,
        materials_created: usize,
    }

    impl FakeGpu {
        fn id(&mut self) -> u32 {
            self.next += 1;
            self.live.insert(self.next);
            self.next
        }
    }

    impl GpuResources for FakeGpu {
        type Mesh = u32;
        type Texture = u32;
        type Material = u32;
        type Shader = u32;

        fn create_mesh(&mut self, _: &[Vertex], _: &[u32]) -> Result<u32, RenderError> {
            self.created += 1;
            Ok(self.id())
        }

        fn create_texture(
            &mut self,
            _: &TextureData,
            _: TextureColorSpace,
        ) -> Result<u32, RenderError> {
            self.created += 1;
            Ok(self.id())
        }

        fn create_material(
            &mut self,
            _: &ResolvedMaterial,
            _: MaterialTextures<u32>,
            _: Option<u32>,
            _: [Option<u32>; 8],
        ) -> Result<u32, RenderError> {
            self.materials_created += 1;
            if self.fail_material_at == Some(self.materials_created) {
                return Err(RenderError::InvalidState("сбой материала"));
            }
            self.created += 1;
            Ok(self.id())
        }

        fn create_surface_shader(&mut self, _: &SurfaceShaderSource) -> Result<u32, RenderError> {
            self.created += 1;
            Ok(self.id())
        }

        fn destroy_surface_shader(&mut self, shader: u32) -> Result<(), RenderError> {
            assert!(
                self.live.remove(&shader),
                "двойное освобождение шейдера {shader}"
            );
            Ok(())
        }

        fn destroy_mesh(&mut self, mesh: u32) -> Result<(), RenderError> {
            assert!(self.live.remove(&mesh), "двойное освобождение меша {mesh}");
            Ok(())
        }

        fn destroy_texture(&mut self, texture: u32) -> Result<(), RenderError> {
            assert!(
                self.live.remove(&texture),
                "двойное освобождение текстуры {texture}"
            );
            Ok(())
        }

        fn destroy_material(&mut self, material: u32) -> Result<(), RenderError> {
            assert!(
                self.live.remove(&material),
                "двойное освобождение материала {material}"
            );
            Ok(())
        }
    }

    fn token(path: &str, load_id: u64) -> PrefabToken {
        PrefabToken {
            path: path.to_owned(),
            load_id,
        }
    }

    /// Модель с `meshes` мешами; у каждого меша материал со ссылкой на текстуру модели.
    fn model(meshes: usize) -> Model {
        Model {
            meshes: (0..meshes)
                .map(|_| ModelMesh {
                    vertices: vec![
                        ModelVertex {
                            position: [0.0; 3],
                            normal: [0.0, 1.0, 0.0],
                            uv: [0.0; 2],
                        };
                        3
                    ],
                    indices: vec![0, 1, 2],
                    skin: None,
                    material: ModelMaterial {
                        base_color: [1.0; 3],
                        metallic: 0.0,
                        roughness: 1.0,
                        emissive: [0.0; 3],
                        opacity: 1.0,
                        alpha_mode: ModelAlphaMode::Opaque,
                        double_sided: false,
                        base_color_texture: Some(0),
                        metallic_roughness_texture: None,
                        normal_texture: None,
                        emissive_texture: None,
                        occlusion_texture: None,
                        normal_scale: 1.0,
                        occlusion_strength: 1.0,
                    },
                })
                .collect(),
            textures: vec![TextureData {
                width: 1,
                height: 1,
                rgba: vec![255; 4],
            }],
            skeleton: None,
            animations: Vec::new(),
        }
    }

    /// Префаб с одним объектом, который ссылается на `models` моделей по путям.
    fn prefab(load: PrefabToken, models: &[(&str, Model)], overridden: bool) -> LoadedPrefab {
        let template_path = VirtualPath::parse(&load.path).unwrap();
        let template = PrefabTemplate::load(&template_path, &mut |_| {
            Ok("[[objects]]\nid = \"root\"\n".to_owned())
        })
        .unwrap();
        let levels = vec![Some(ObjectModel {
            levels: models
                .iter()
                .map(|(path, _)| ((*path).to_owned(), overridden.then(|| "tex.png".to_owned())))
                .collect(),
            material: None,
            material_slots: Vec::new(),
            animated: false,
        })];
        LoadedPrefab {
            token: load,
            cpu: PrefabCpu {
                template: Arc::new(template),
                sources: BTreeMap::new(),
                geometry: Vec::new(),
            },
            models: models
                .iter()
                .map(|(path, model)| ((*path).to_owned(), model.clone()))
                .collect(),
            textures: HashMap::from([(
                "tex.png".to_owned(),
                TextureData {
                    width: 1,
                    height: 1,
                    rgba: vec![255; 4],
                },
            )]),
            material_assets: HashMap::new(),
            shader_assets: HashMap::new(),
            levels,
            particles: Vec::new(),
        }
    }

    fn run_to_completion(
        gpu_state: &mut PrefabGpu<FakeGpu>,
        gpu: &mut FakeGpu,
    ) -> Vec<PrefabReplyOf<u32, u32>> {
        let mut replies = Vec::new();
        for _ in 0..100 {
            if !gpu_state.has_pending() {
                break;
            }
            replies.extend(gpu_state.advance(gpu, PREFAB_UPLOAD_BUDGET));
        }
        replies
    }

    fn ready_count(replies: &[PrefabReplyOf<u32, u32>]) -> usize {
        replies
            .iter()
            .filter(|reply| matches!(reply, PrefabReplyOf::Ready(_)))
            .count()
    }

    #[test]
    fn two_prefabs_share_a_model_until_both_are_released() {
        let mut gpu = FakeGpu::default();
        let mut state = PrefabGpu::default();
        let first = token("prefabs/a.rveprefab", 1);
        let second = token("prefabs/b.rveprefab", 2);
        state.submit(prefab(
            first.clone(),
            &[("models/house.fbx", model(2))],
            false,
        ));
        let replies = run_to_completion(&mut state, &mut gpu);
        assert_eq!(ready_count(&replies), 1);
        let meshes_after_first = gpu.created;
        state.submit(prefab(
            second.clone(),
            &[("models/house.fbx", model(2))],
            false,
        ));
        assert_eq!(ready_count(&run_to_completion(&mut state, &mut gpu)), 1);
        assert_eq!(
            gpu.created, meshes_after_first,
            "общая модель не должна грузиться повторно"
        );

        // Модель держит 2 меша, 2 материала и текстуру: после освобождения первого всё живо.
        state.release(&mut gpu, &first);
        assert_eq!(
            gpu.live.len(),
            5,
            "ресурсы общей модели остаются у второго префаба"
        );
        state.release(&mut gpu, &second);
        assert!(
            gpu.live.is_empty(),
            "после освобождения обоих ресурсов не осталось"
        );
    }

    #[test]
    fn budget_splits_one_upload_over_several_iterations() {
        let mut gpu = FakeGpu::default();
        let mut state = PrefabGpu::default();
        state.submit(prefab(
            token("prefabs/a.rveprefab", 1),
            &[("m.fbx", model(3))],
            false,
        ));
        // Три меша — три шага; бюджет 1 даёт по одному шагу за вызов, ответ приходит с последним.
        assert!(state.advance(&mut gpu, 1).is_empty());
        assert!(state.advance(&mut gpu, 1).is_empty());
        assert!(state.has_pending(), "задание продолжается между итерациями");
        assert_eq!(
            state.advance(&mut gpu, 1).len(),
            1,
            "последний шаг завершает задание"
        );
        assert!(!state.has_pending());
    }

    #[test]
    fn failed_upload_rolls_back_every_resource_it_created() {
        let mut gpu = FakeGpu {
            fail_material_at: Some(2),
            ..FakeGpu::default()
        };
        let mut state = PrefabGpu::default();
        state.submit(prefab(
            token("prefabs/a.rveprefab", 1),
            &[("m.fbx", model(2))],
            false,
        ));
        let replies = run_to_completion(&mut state, &mut gpu);
        assert!(matches!(
            replies.as_slice(),
            [PrefabReplyOf::Failed { error, .. }] if error.contains("сбой материала")
        ));
        assert!(
            gpu.live.is_empty(),
            "частично созданные ресурсы должны быть освобождены"
        );
    }

    #[test]
    fn corrupt_material_slot_uses_a_reusable_magenta_fallback_without_failing_prefab() {
        let mut gpu = FakeGpu::default();
        let mut state = PrefabGpu::default();
        let load = token("prefabs/broken-material.rveprefab", 1);
        let mut loaded = prefab(load.clone(), &[("m.fbx", model(1))], false);
        loaded.levels[0].as_mut().unwrap().material_slots =
            vec![Some("materials/broken.rvemat".to_owned())];
        loaded.material_assets.insert(
            "materials/broken.rvemat".to_owned(),
            Err("разбор материала завершился ошибкой".to_owned()),
        );
        state.submit(loaded);

        let replies = run_to_completion(&mut state, &mut gpu);
        let [PrefabReplyOf::Ready(ready)] = replies.as_slice() else {
            panic!("повреждённый материал не должен прерывать загрузку префаба");
        };
        assert_eq!(ready.gpu[0][0].len(), 1);
        assert_eq!(
            gpu.materials_created, 2,
            "FBX материал и fallback создаются один раз"
        );
        state.release(&mut gpu, &load);
        assert!(gpu.live.is_empty(), "fallback и ресурсы модели освобождены");
    }

    #[test]
    fn stale_release_of_an_old_load_does_not_touch_a_fresh_upload() {
        let mut gpu = FakeGpu::default();
        let mut state = PrefabGpu::default();
        let old = token("prefabs/a.rveprefab", 1);
        let fresh = token("prefabs/a.rveprefab", 2);
        state.submit(prefab(old.clone(), &[("m.fbx", model(1))], false));
        run_to_completion(&mut state, &mut gpu);
        state.submit(prefab(fresh.clone(), &[("m.fbx", model(1))], false));
        run_to_completion(&mut state, &mut gpu);
        let live_before = gpu.live.clone();

        // Оба токена держат одну модель. Освобождение старого токена не трогает общие ресурсы
        // и не может снять ресурсы новой загрузки; повторное освобождение безопасно.
        state.release(&mut gpu, &old);
        assert_eq!(
            gpu.live, live_before,
            "свежая загрузка продолжает владеть ресурсами"
        );
        state.release(&mut gpu, &old);
        assert_eq!(gpu.live, live_before);
        state.release(&mut gpu, &fresh);
        assert!(gpu.live.is_empty());
    }

    #[test]
    fn releasing_a_queued_upload_frees_its_partial_resources() {
        let mut gpu = FakeGpu::default();
        let mut state = PrefabGpu::default();
        let pending = token("prefabs/a.rveprefab", 1);
        state.submit(prefab(pending.clone(), &[("m.fbx", model(2))], false));
        // Один из двух мешей уже создан, задание ещё в очереди.
        assert!(state.advance(&mut gpu, 1).is_empty());
        assert!(!gpu.live.is_empty());
        state.release(&mut gpu, &pending);
        assert!(gpu.live.is_empty());
        assert!(!state.has_pending());
    }

    #[test]
    fn override_levels_reuse_model_meshes_and_own_their_materials() {
        let mut gpu = FakeGpu::default();
        let mut state = PrefabGpu::default();
        let load = token("prefabs/a.rveprefab", 1);
        state.submit(prefab(load.clone(), &[("m.fbx", model(2))], true));
        let replies = run_to_completion(&mut state, &mut gpu);
        let [PrefabReplyOf::Ready(ready)] = replies.as_slice() else {
            panic!("ожидался один готовый префаб");
        };
        let [levels] = ready.gpu.as_slice() else {
            panic!("один объект");
        };
        assert_eq!(levels.len(), 1, "один уровень");
        assert_eq!(levels[0].len(), 2, "по мешу на каждый меш модели");
        // Два меша и два материала по умолчанию модели, текстура модели, два материала
        // переопределения и текстура замены: при освобождении токена всё уходит.
        assert_eq!(gpu.live.len(), 8);
        state.release(&mut gpu, &load);
        assert!(gpu.live.is_empty());
    }

    fn template_from(text: &str) -> PrefabTemplate {
        PrefabTemplate::load(
            &VirtualPath::parse("/prefabs/test.rveprefab").unwrap(),
            &mut |_| Ok(text.to_owned()),
        )
        .unwrap()
    }

    #[test]
    fn mesh_without_a_collider_gets_no_physics_geometry() {
        let template = template_from(
            "[[assets]]\npath = \"/models/visual.fbx\"\n[[objects]]\nid = \"root\"\n\
             [[objects]]\nid = \"visual\"\nparent = \"root\"\n[objects.mesh_renderer]\n\
             model = \"/models/visual.fbx\"\n",
        );
        let models = HashMap::from([("models/visual.fbx".to_owned(), model(1))]);
        assert!(
            prefab_collider_geometry(&template, &models)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn only_mesh_collider_models_reach_the_physics_geometry() {
        let template = template_from(
            "[[assets]]\npath = \"/models/visual.fbx\"\n[[assets]]\npath = \"/models/hull.fbx\"\n\
             [[assets]]\npath = \"/models/crate.fbx\"\n[[objects]]\nid = \"root\"\n\
             [[objects]]\nid = \"visual\"\nparent = \"root\"\n[objects.mesh_renderer]\n\
             model = \"/models/visual.fbx\"\n[[objects]]\nid = \"hull\"\nparent = \"root\"\n\
             [objects.mesh_renderer]\nmodel = \"/models/hull.fbx\"\n[objects.collider]\n\
             shape = \"convex_mesh\"\n[[objects]]\nid = \"crate\"\nparent = \"root\"\n\
             [objects.mesh_renderer]\nmodel = \"/models/crate.fbx\"\n[objects.collider]\n\
             shape = \"box\"\nhalf_extents = [1.0, 1.0, 1.0]\n",
        );
        let models = HashMap::from([
            ("models/visual.fbx".to_owned(), model(1)),
            ("models/hull.fbx".to_owned(), model(2)),
            ("models/crate.fbx".to_owned(), model(1)),
        ]);
        let geometry = prefab_collider_geometry(&template, &models).unwrap();
        let paths = geometry
            .iter()
            .map(|model| model.path.as_str())
            .collect::<Vec<_>>();
        assert_eq!(paths, ["models/hull.fbx"]);
        assert_eq!(geometry[0].meshes.len(), 2);

        // Модель коллайдера, которой нет среди подготовленных, — ошибка, а не пустая геометрия.
        assert!(prefab_collider_geometry(&template, &HashMap::new()).is_err());
    }
}
