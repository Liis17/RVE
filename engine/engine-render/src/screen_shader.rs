//! Пользовательские экранные шейдеры камеры: компиляция GLSL, проверка интерфейса и упаковка
//! push constants.
//!
//! Пользователь пишет только `vec3 rve_shade(vec2 uv)` и свои вспомогательные функции. Модуль
//! оборачивает исходник фиксированным prelude и собственным `main`, компилирует через naga и
//! проверяет IR: одна точка входа, ровно одна привязка texture и sampler из prelude, один блок
//! push constants и никаких других ресурсов или интерфейсов. Всё это выполняется на CPU, поэтому
//! сборка rve-build проверяет шейдеры тем же кодом, что и renderer, без GPU.
//!
//! Раскладка push constants (112 байт) совпадает с блоком `RveScreen` из prelude:
//! `params[4]` (16 чисел), `resolution` (размер content и обратный), `content` (масштаб
//! content/physical и обратный размер physical), `time` (время сцены по модулю 3600, номер кадра).

use std::fmt;

use ash::vk;
use naga::back::spv;
use naga::front::glsl;
use naga::valid::{Capabilities, ValidationFlags, Validator};
use naga::{AddressSpace, Binding, Module, ShaderStage, TypeInner};

use super::bloom::{draw_fullscreen, ensure_sampled};
use super::{
    DeviceImage, GpuMarks, GpuPass, LDR_FORMAT, OFFSCREEN_FORMAT, OwnedDescriptorPool, RenderError,
    color_subresource_range, create_post_pipeline, image_layout_barrier, mark_pass,
    offscreen_previous_use, pipeline_barrier, write_image_set,
};

/// Число пользовательских параметров шейдера (`rve_param(0..16)`).
pub const SCREEN_SHADER_PARAM_COUNT: usize = 16;
/// Максимум проходов экранных шейдеров на один вид.
pub const SCREEN_SHADER_MAX_PASSES: usize = 4;
/// Размер push constants экранного прохода в байтах.
pub const SCREEN_PUSH_CONSTANTS_SIZE: usize = 112;
/// Период времени сцены, передаваемого шейдеру, в секундах.
pub const SCREEN_SHADER_TIME_PERIOD: f32 = 3600.0;

const _: () = assert!(SCREEN_PUSH_CONSTANTS_SIZE <= 128);

/// Заголовок prelude: директивы, привязки, push constants, UV и `rve_sample`.
/// Строки заголовка и `main` добавляются вокруг исходника пользователя, поэтому номера строк
/// ошибок пересчитываются через длину заголовка.
const PRELUDE_HEAD: &str = "#version 450
layout(set = 0, binding = 0) uniform texture2D rve_source_image;
layout(set = 0, binding = 1) uniform sampler rve_source_sampler;
layout(push_constant) uniform RveScreen {
    vec4 params[4];
    vec4 resolution;
    vec4 content;
    vec4 time;
} rve;
layout(location = 0) in vec2 rve_in_uv;
layout(location = 0) out vec4 rve_out_color;

float rve_param(int index) {
    return rve.params[index >> 2][index & 3];
}

vec4 rve_sample(vec2 uv) {
    vec2 limit_min = 0.5 * rve.content.zw;
    vec2 limit_max = rve.content.xy - limit_min;
    vec2 texel_uv = clamp(clamp(uv, 0.0, 1.0) * rve.content.xy, limit_min, limit_max);
    return textureLod(sampler2D(rve_source_image, rve_source_sampler), texel_uv, 0.0);
}
";

/// Хвост для стадии `before_post`: alpha берётся из исходного буфера.
const TAIL_BEFORE_POST: &str = "
void main() {
    vec3 rve_color = rve_shade(rve_in_uv);
    rve_out_color = vec4(rve_color, rve_sample(rve_in_uv).a);
}
";

/// Хвост для стадии `after_post`: alpha — перцептивная яркость (контракт FXAA в output.frag).
const TAIL_AFTER_POST: &str = "
float rve_luma(vec3 color) {
    vec3 clamped = clamp(color, 0.0, 1.0);
    vec3 srgb = mix(
        1.055 * pow(clamped, vec3(1.0 / 2.4)) - 0.055,
        clamped * 12.92,
        step(clamped, vec3(0.0031308))
    );
    return dot(srgb, vec3(0.2126, 0.7152, 0.0722));
}

void main() {
    vec3 rve_color = rve_shade(rve_in_uv);
    rve_out_color = vec4(rve_color, rve_luma(rve_color));
}
";

/// Стадия экранного шейдера относительно цепочки постобработки.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ScreenShaderStage {
    /// HDR после temporal resolve, до motion blur, экспозиции, bloom и tonemap.
    BeforePost,
    /// LDR после tonemap и виньетки, до FXAA, film grain и UI.
    AfterPost,
}

impl ScreenShaderStage {
    /// Хвост GLSL для стадии; весь остальной текст одинаков для обеих стадий.
    const fn tail(self) -> &'static str {
        match self {
            Self::BeforePost => TAIL_BEFORE_POST,
            Self::AfterPost => TAIL_AFTER_POST,
        }
    }
}

/// Ошибка компиляции или проверки экранного шейдера.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ScreenShaderError {
    /// Ошибка разбора GLSL; `line` — номер строки в исходнике пользователя, если она в нём.
    Syntax { line: Option<u32>, message: String },
    /// Запрещённая конструкция: своя директива, `#version` и т. п.
    Forbidden { line: u32, reason: &'static str },
    /// Интерфейс шейдера не совпадает с prelude или содержит лишние ресурсы.
    Interface(&'static str),
    /// Ошибка валидации IR naga.
    Validation(String),
    /// Ошибка записи SPIR-V.
    Spirv(String),
}

impl fmt::Display for ScreenShaderError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Syntax {
                line: Some(line),
                message,
            } => write!(f, "строка {line}: {message}"),
            Self::Syntax {
                line: None,
                message,
            } => write!(f, "ошибка шейдера: {message}"),
            Self::Forbidden { line, reason } => write!(f, "строка {line}: {reason}"),
            Self::Interface(reason) => write!(f, "интерфейс шейдера: {reason}"),
            Self::Validation(message) => write!(f, "проверка шейдера: {message}"),
            Self::Spirv(message) => write!(f, "запись SPIR-V: {message}"),
        }
    }
}

impl std::error::Error for ScreenShaderError {}

/// Компилирует пользовательский исходник экранного шейдера в SPIR-V для указанной стадии.
///
/// Исходник — GLSL 450 без `#version`, своего `main` и своих `in`/`out`, с функцией
/// `vec3 rve_shade(vec2 uv)`. Чистая функция без GPU: её используют renderer и rve-build.
pub fn compile_screen_shader(
    source: &str,
    stage: ScreenShaderStage,
) -> Result<Vec<u32>, ScreenShaderError> {
    reject_directives(source)?;
    let head_lines = line_count(PRELUDE_HEAD);
    let user_lines = line_count(source);
    let combined = format!("{PRELUDE_HEAD}{source}\n{}", stage.tail());
    let mut frontend = glsl::Frontend::default();
    let options = glsl::Options::from(ShaderStage::Fragment);
    let module = frontend.parse(&options, &combined).map_err(|errors| {
        let first = errors.errors.first();
        let message = first.map_or_else(
            || "неизвестная ошибка".to_owned(),
            |error| error.kind.to_string(),
        );
        let line = first
            .and_then(|error| error.location(&combined))
            .map(|location| location.line_number)
            .and_then(|line| user_line(line, head_lines, user_lines));
        ScreenShaderError::Syntax { line, message }
    })?;
    check_interface(&module)?;
    let info = Validator::new(ValidationFlags::all(), Capabilities::IMMEDIATES)
        .validate(&module)
        .map_err(|error| ScreenShaderError::Validation(error.to_string()))?;
    // Координаты не переворачиваются: fragment-шейдер следует соглашениям Vulkan, как в build.rs.
    let options = spv::Options {
        flags: spv::Options::default().flags - spv::WriterFlags::ADJUST_COORDINATE_SPACE,
        ..spv::Options::default()
    };
    spv::write_vec(&module, &info, &options, None)
        .map_err(|error| ScreenShaderError::Spirv(error.to_string()))
}

/// Номер строки исходника пользователя по номеру строки собранного текста; `None` для заголовка
/// и хвоста, которые пишет движок.
fn user_line(line: u32, head_lines: u32, user_lines: u32) -> Option<u32> {
    let relative = line.checked_sub(head_lines)?;
    (relative >= 1 && relative <= user_lines).then_some(relative)
}

fn line_count(text: &str) -> u32 {
    text.lines().count() as u32
}

/// Директивы препроцессора принадлежат движку: пользователь не задаёт версию и расширения.
fn reject_directives(source: &str) -> Result<(), ScreenShaderError> {
    for (index, line) in source.lines().enumerate() {
        if line.trim_start().starts_with('#') {
            return Err(ScreenShaderError::Forbidden {
                line: index as u32 + 1,
                reason: "директивы препроцессора (#version, #extension и т. п.) задаёт движок",
            });
        }
    }
    Ok(())
}

/// Проверяет интерфейс IR: одна fragment-точка входа `main` с одним входом `rve_in_uv` и одним
/// выходом `rve_out_color`, ресурсы только из prelude, функция `rve_shade(vec2) -> vec3`.
fn check_interface(module: &Module) -> Result<(), ScreenShaderError> {
    let [entry] = module.entry_points.as_slice() else {
        return Err(ScreenShaderError::Interface(
            "шейдер должен содержать ровно одну точку входа main",
        ));
    };
    if entry.name != "main" || entry.stage != ShaderStage::Fragment {
        return Err(ScreenShaderError::Interface(
            "точка входа должна быть fragment-шейдером main",
        ));
    }
    let [argument] = entry.function.arguments.as_slice() else {
        return Err(ScreenShaderError::Interface(
            "вход шейдера не совпадает с prelude: допустим только rve_in_uv",
        ));
    };
    if !matches!(
        argument.binding,
        Some(Binding::Location { location: 0, .. })
    ) || !is_vector(module, argument.ty, 2)
    {
        return Err(ScreenShaderError::Interface(
            "вход шейдера не совпадает с prelude: допустим только rve_in_uv",
        ));
    }
    let Some(result) = entry.function.result.as_ref() else {
        return Err(ScreenShaderError::Interface(
            "выход шейдера не совпадает с prelude: допустим только rve_out_color",
        ));
    };
    // naga 30 оборачивает выход fragment-шейдера в структуру с одним членом Location(0): vec4.
    let output_ok = match &module.types[result.ty].inner {
        TypeInner::Struct { members, .. } => matches!(
            members.as_slice(),
            [member] if matches!(member.binding, Some(Binding::Location { location: 0, .. }))
                && is_vector(module, member.ty, 4)
        ),
        _ => false,
    };
    if !output_ok {
        return Err(ScreenShaderError::Interface(
            "выход шейдера не совпадает с prelude: допустим только rve_out_color",
        ));
    }
    let mut textures = 0;
    let mut samplers = 0;
    let mut push_blocks = 0;
    // Private-глобалы naga 30 — это копии входа и выхода fragment-шейдера; любой другой отклоняется.
    let mut io_globals = 0;
    for (_, global) in module.global_variables.iter() {
        match global.space {
            AddressSpace::Private => match global.name.as_deref() {
                Some("rve_in_uv" | "rve_out_color") => io_globals += 1,
                _ => {
                    return Err(ScreenShaderError::Interface(
                        "шейдер объявляет лишнюю глобальную переменную или in/out",
                    ));
                }
            },
            AddressSpace::Immediate => push_blocks += 1,
            AddressSpace::Handle => {
                let binding = global
                    .binding
                    .as_ref()
                    .map(|binding| (binding.group, binding.binding));
                let inner = &module.types[global.ty].inner;
                match (binding, inner) {
                    (Some((0, 0)), TypeInner::Image { .. }) => textures += 1,
                    (Some((0, 1)), TypeInner::Sampler { .. }) => samplers += 1,
                    _ => {
                        return Err(ScreenShaderError::Interface(
                            "шейдер объявляет лишний texture, sampler или буфер",
                        ));
                    }
                }
            }
            _ => {
                return Err(ScreenShaderError::Interface(
                    "шейдер объявляет лишний буфер или ресурс; разрешён только prelude",
                ));
            }
        }
    }
    if textures != 1 || samplers != 1 || push_blocks != 1 || io_globals != 2 {
        return Err(ScreenShaderError::Interface(
            "ресурсы шейдера не совпадают с prelude: нужны texture, sampler и блок push constants",
        ));
    }
    let shade = module.functions.iter().find_map(|(_, function)| {
        (function.name.as_deref() == Some("rve_shade")).then_some(function)
    });
    let Some(shade) = shade else {
        return Err(ScreenShaderError::Interface(
            "нет функции vec3 rve_shade(vec2 uv)",
        ));
    };
    let [uv] = shade.arguments.as_slice() else {
        return Err(ScreenShaderError::Interface(
            "rve_shade должна принимать один аргумент vec2 uv",
        ));
    };
    let returns_color = shade
        .result
        .as_ref()
        .is_some_and(|result| is_vector(module, result.ty, 3));
    if !is_vector(module, uv.ty, 2) || !returns_color {
        return Err(ScreenShaderError::Interface(
            "нет функции vec3 rve_shade(vec2 uv)",
        ));
    }
    Ok(())
}

/// Проверяет, что тип — вектор `f32` заданного размера (2, 3 или 4 компонента).
fn is_vector(module: &Module, ty: naga::Handle<naga::Type>, size: u8) -> bool {
    let TypeInner::Vector {
        size: vector_size,
        scalar,
    } = module.types[ty].inner
    else {
        return false;
    };
    let expected = match size {
        2 => naga::VectorSize::Bi,
        3 => naga::VectorSize::Tri,
        _ => naga::VectorSize::Quad,
    };
    vector_size == expected && scalar.kind == naga::ScalarKind::Float && scalar.width == 4
}

/// Собирает push constants экранного прохода: параметры, размеры content и physical, время.
///
/// `content` — заполненная область входа (то, что рисует предыдущий проход или composite),
/// `physical` — размер самого изображения. UV шейдера нормализованы к `content`; выборка
/// переводит их в координаты `physical`, поэтому масштаб не равен 1 при render scale и temporal
/// output extent.
pub(crate) fn screen_push_constants(
    params: &[f32; SCREEN_SHADER_PARAM_COUNT],
    content: (u32, u32),
    physical: (u32, u32),
    time: f32,
    frame: u32,
) -> [u8; SCREEN_PUSH_CONSTANTS_SIZE] {
    let content_w = content.0.max(1) as f32;
    let content_h = content.1.max(1) as f32;
    let physical_w = physical.0.max(1) as f32;
    let physical_h = physical.1.max(1) as f32;
    let mut values = [0.0_f32; SCREEN_PUSH_CONSTANTS_SIZE / 4];
    values[..SCREEN_SHADER_PARAM_COUNT].copy_from_slice(params);
    values[16..20].copy_from_slice(&[content_w, content_h, 1.0 / content_w, 1.0 / content_h]);
    values[20..24].copy_from_slice(&[
        content_w / physical_w,
        content_h / physical_h,
        1.0 / physical_w,
        1.0 / physical_h,
    ]);
    values[24..28].copy_from_slice(&[
        time.rem_euclid(SCREEN_SHADER_TIME_PERIOD),
        frame as f32,
        0.0,
        0.0,
    ]);
    let mut bytes = [0_u8; SCREEN_PUSH_CONSTANTS_SIZE];
    for (index, value) in values.iter().enumerate() {
        bytes[index * 4..index * 4 + 4].copy_from_slice(&value.to_ne_bytes());
    }
    bytes
}

/// Идентификатор экранного шейдера, созданного [`crate::Renderer::create_screen_shader`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct ScreenShaderId(pub(crate) usize);

/// Один проход экранного шейдера вида.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ScreenShaderPass {
    pub shader: ScreenShaderId,
    pub stage: ScreenShaderStage,
    /// Пользовательские параметры `rve_param(0..16)`; все значения должны быть конечными.
    pub params: [f32; SCREEN_SHADER_PARAM_COUNT],
}

/// Экранные шейдеры вида: до четырёх включённых проходов в порядке записи и время сцены.
/// Выключенный или ещё не загруженный шейдер — `None` в своём слоте.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct ScreenShaders {
    pub passes: [Option<ScreenShaderPass>; SCREEN_SHADER_MAX_PASSES],
    /// Время сцены в секундах; передаётся шейдеру по модулю 3600.
    pub time: f32,
}

impl ScreenShaders {
    /// Число включённых проходов заданной стадии.
    pub fn count(&self, stage: ScreenShaderStage) -> usize {
        self.passes
            .iter()
            .flatten()
            .filter(|pass| pass.stage == stage)
            .count()
    }

    /// Все проходы ссылаются на шейдеры, для которых `is_alive` возвращает `true`.
    pub(crate) fn all_known(&self, is_alive: impl Fn(ScreenShaderId) -> bool) -> bool {
        self.passes
            .iter()
            .flatten()
            .all(|pass| is_alive(pass.shader))
    }

    /// Проверяет конечность времени и параметров; существование шейдеров проверяет renderer.
    pub fn validate(&self) -> Result<(), &'static str> {
        if !self.time.is_finite() {
            return Err("время экранных шейдеров должно быть конечным");
        }
        let finite = self
            .passes
            .iter()
            .flatten()
            .all(|pass| pass.params.iter().all(|value| value.is_finite()));
        if !finite {
            return Err("параметры экранных шейдеров должны быть конечными");
        }
        Ok(())
    }
}

/// Проходы одной стадии, отобранные из [`ScreenShaders`] в порядке записи.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub(crate) struct ScreenChain {
    pub(crate) passes: [Option<ScreenShaderPass>; SCREEN_SHADER_MAX_PASSES],
    pub(crate) time: f32,
    pub(crate) frame: u32,
}

impl ScreenChain {
    /// Отбирает проходы стадии; пустая цепочка не требует ресурсов.
    pub(crate) fn collect(shaders: &ScreenShaders, stage: ScreenShaderStage, frame: u32) -> Self {
        let mut passes = [None; SCREEN_SHADER_MAX_PASSES];
        let mut count = 0;
        for pass in shaders.passes.iter().flatten() {
            if pass.stage == stage {
                passes[count] = Some(*pass);
                count += 1;
            }
        }
        Self {
            passes,
            time: shaders.time,
            frame,
        }
    }

    pub(crate) fn len(&self) -> usize {
        self.passes.iter().flatten().count()
    }
}

/// Индекс ping-pong изображения с результатом цепочки из `count` проходов: первый проход пишет
/// изображение 0, второй — 1 и т. д. Пустая цепочка результата не даёт.
pub(crate) fn chain_output_index(count: usize) -> Option<usize> {
    (count > 0).then(|| (count - 1) % 2)
}

/// Пайплайны одного шейдера: вариант `before_post` в HDR и вариант `after_post` в LDR.
pub(crate) struct ScreenShaderPipelines {
    device: ash::Device,
    pub(crate) before: vk::Pipeline,
    pub(crate) after: vk::Pipeline,
}

impl ScreenShaderPipelines {
    pub(crate) fn new(
        device: &ash::Device,
        cache: vk::PipelineCache,
        layout: vk::PipelineLayout,
        before_spirv: &[u8],
        after_spirv: &[u8],
    ) -> Result<Self, RenderError> {
        let before =
            create_post_pipeline(device, cache, layout, OFFSCREEN_FORMAT, before_spirv, false)?;
        let after =
            match create_post_pipeline(device, cache, layout, LDR_FORMAT, after_spirv, false) {
                Ok(after) => after,
                Err(error) => {
                    // SAFETY: the pipeline was created above and nothing references it yet.
                    unsafe { device.destroy_pipeline(before, None) };
                    return Err(error);
                }
            };
        Ok(Self {
            device: device.clone(),
            before,
            after,
        })
    }

    /// Пайплайн для стадии; стадия выбирает формат цели, поэтому варианты не смешиваются.
    pub(crate) fn for_stage(&self, stage: ScreenShaderStage) -> vk::Pipeline {
        match stage {
            ScreenShaderStage::BeforePost => self.before,
            ScreenShaderStage::AfterPost => self.after,
        }
    }
}

impl Drop for ScreenShaderPipelines {
    fn drop(&mut self) {
        // SAFETY: owners retire this value only after frames that might use its pipelines finished.
        unsafe {
            self.device.destroy_pipeline(self.before, None);
            self.device.destroy_pipeline(self.after, None);
        }
    }
}

/// Набор дескрипторов и layout экранных проходов; общий для всех шейдеров renderer.
pub(crate) struct ScreenShaderLayout {
    device: ash::Device,
    pub(crate) set_layout: vk::DescriptorSetLayout,
    pub(crate) pipeline_layout: vk::PipelineLayout,
}

impl ScreenShaderLayout {
    /// Создаёт layout с binding 0 (sampled image), binding 1 (sampler) и push constants фрагмента.
    pub(crate) fn new(device: &ash::Device) -> Result<Self, RenderError> {
        let bindings = [
            vk::DescriptorSetLayoutBinding::default()
                .binding(0)
                .descriptor_type(vk::DescriptorType::SAMPLED_IMAGE)
                .descriptor_count(1)
                .stage_flags(vk::ShaderStageFlags::FRAGMENT),
            vk::DescriptorSetLayoutBinding::default()
                .binding(1)
                .descriptor_type(vk::DescriptorType::SAMPLER)
                .descriptor_count(1)
                .stage_flags(vk::ShaderStageFlags::FRAGMENT),
        ];
        let set_layout_info = vk::DescriptorSetLayoutCreateInfo::default().bindings(&bindings);
        // SAFETY: the bindings match the prelude declared in this module.
        let set_layout = unsafe { device.create_descriptor_set_layout(&set_layout_info, None) }?;
        let set_layouts = [set_layout];
        let push_constant_ranges = [vk::PushConstantRange::default()
            .stage_flags(vk::ShaderStageFlags::FRAGMENT)
            .offset(0)
            .size(SCREEN_PUSH_CONSTANTS_SIZE as u32)];
        let layout_info = vk::PipelineLayoutCreateInfo::default()
            .set_layouts(&set_layouts)
            .push_constant_ranges(&push_constant_ranges);
        // SAFETY: the set layout and the 112-byte fragment range match the prelude.
        match unsafe { device.create_pipeline_layout(&layout_info, None) } {
            Ok(pipeline_layout) => Ok(Self {
                device: device.clone(),
                set_layout,
                pipeline_layout,
            }),
            Err(error) => {
                // SAFETY: the set layout was created above and nothing references it.
                unsafe { device.destroy_descriptor_set_layout(set_layout, None) };
                Err(error.into())
            }
        }
    }
}

impl Drop for ScreenShaderLayout {
    fn drop(&mut self) {
        // SAFETY: the renderer drops this layout after all pipelines built on it are gone.
        unsafe {
            self.device
                .destroy_pipeline_layout(self.pipeline_layout, None);
            self.device
                .destroy_descriptor_set_layout(self.set_layout, None);
        }
    }
}

/// Два промежуточных изображения одной стадии и наборы дескрипторов кадрового слота.
/// Размер — физический размер входа стадии; каждый записанный проход имеет свой набор.
pub(crate) struct ScreenShaderScratch {
    pub(crate) extent: vk::Extent2D,
    images: [DeviceImage; 2],
    layouts: [vk::ImageLayout; 2],
    _pool: OwnedDescriptorPool,
    sets: [vk::DescriptorSet; SCREEN_SHADER_MAX_PASSES],
}

impl ScreenShaderScratch {
    pub(crate) fn new(
        instance: &ash::Instance,
        device: &ash::Device,
        physical_device: vk::PhysicalDevice,
        extent: vk::Extent2D,
        format: vk::Format,
        set_layout: vk::DescriptorSetLayout,
    ) -> Result<Self, RenderError> {
        let image = |_| {
            DeviceImage::new(
                instance,
                device,
                physical_device,
                extent,
                format,
                vk::ImageUsageFlags::COLOR_ATTACHMENT | vk::ImageUsageFlags::SAMPLED,
                color_subresource_range(),
            )
        };
        let images = [image(0)?, image(1)?];
        let pool_sizes = [
            vk::DescriptorPoolSize::default()
                .ty(vk::DescriptorType::SAMPLED_IMAGE)
                .descriptor_count(SCREEN_SHADER_MAX_PASSES as u32),
            vk::DescriptorPoolSize::default()
                .ty(vk::DescriptorType::SAMPLER)
                .descriptor_count(SCREEN_SHADER_MAX_PASSES as u32),
        ];
        let pool_info = vk::DescriptorPoolCreateInfo::default()
            .max_sets(SCREEN_SHADER_MAX_PASSES as u32)
            .pool_sizes(&pool_sizes);
        let pool = OwnedDescriptorPool::new(device, &pool_info)?;
        let layouts = [set_layout; SCREEN_SHADER_MAX_PASSES];
        let allocate_info = vk::DescriptorSetAllocateInfo::default()
            .descriptor_pool(pool.handle)
            .set_layouts(&layouts);
        // SAFETY: the pool was sized for exactly SCREEN_SHADER_MAX_PASSES sets of this layout.
        let allocated = unsafe { device.allocate_descriptor_sets(&allocate_info) }?;
        let mut sets = [vk::DescriptorSet::null(); SCREEN_SHADER_MAX_PASSES];
        sets.copy_from_slice(&allocated);
        Ok(Self {
            extent,
            images,
            layouts: [vk::ImageLayout::UNDEFINED; 2],
            _pool: pool,
            sets,
        })
    }

    /// Изображение результата ping-pong по индексу из [`record_screen_chain`].
    pub(crate) fn image(&self, index: usize) -> &DeviceImage {
        &self.images[index]
    }
}

/// Пайплайн и параметры одного прохода для записи.
#[derive(Clone, Copy)]
pub(crate) struct ScreenPassDraw {
    pub(crate) pipeline: vk::Pipeline,
    pub(crate) params: [f32; SCREEN_SHADER_PARAM_COUNT],
}

/// Запись одной стадии: scratch кадрового слота, sampler, layout и проходы с пайплайнами.
pub(crate) struct ScreenChainRecord<'a> {
    pub(crate) scratch: &'a mut ScreenShaderScratch,
    pub(crate) sampler: vk::Sampler,
    pub(crate) pipeline_layout: vk::PipelineLayout,
    pub(crate) passes: [Option<ScreenPassDraw>; SCREEN_SHADER_MAX_PASSES],
    pub(crate) time: f32,
    pub(crate) frame: u32,
}

impl ScreenChainRecord<'_> {
    /// Число проходов, которые будут записаны.
    pub(crate) fn len(&self) -> usize {
        self.passes.iter().flatten().count()
    }
}

/// Записывает проходы стадии ping-pong; возвращает индекс изображения с результатом.
///
/// Первый проход читает `source` (уже в `SHADER_READ_ONLY_OPTIMAL`), каждый следующий — результат
/// предыдущего. Viewport и render area — `content`; `physical` — размер scratch-изображений.
/// Каждый проход получает собственный набор дескрипторов, поэтому его не перезаписывает следующий.
#[allow(clippy::too_many_arguments)]
pub(crate) fn record_screen_chain(
    device: &ash::Device,
    command_buffer: vk::CommandBuffer,
    source: &DeviceImage,
    source_layout: &mut vk::ImageLayout,
    content: vk::Extent2D,
    record: &mut ScreenChainRecord<'_>,
    timestamps: Option<GpuMarks>,
    pass: GpuPass,
) -> Option<usize> {
    if record.len() == 0 {
        return None;
    }
    ensure_sampled(device, command_buffer, source.image, source_layout);
    let physical = record.scratch.extent;
    let mut input = source.view;
    let mut result = None;
    let mut index = 0;
    let passes = record.passes;
    for draw in passes.iter().flatten() {
        let target = index % 2;
        let set = record.scratch.sets[index];
        write_image_set(device, set, input, record.sampler);
        let push = screen_push_constants(
            &draw.params,
            (content.width, content.height),
            (physical.width, physical.height),
            record.time,
            record.frame,
        );
        transition_to_attachment(device, command_buffer, record.scratch, target);
        draw_fullscreen(
            device,
            command_buffer,
            record.scratch.images[target].view,
            content,
            false,
            draw.pipeline,
            record.pipeline_layout,
            set,
            &push,
        );
        transition_to_sampled(device, command_buffer, record.scratch, target);
        input = record.scratch.images[target].view;
        result = Some(target);
        index += 1;
    }
    mark_pass(device, command_buffer, timestamps, pass);
    debug_assert_eq!(result, chain_output_index(index));
    result
}

fn transition_to_attachment(
    device: &ash::Device,
    command_buffer: vk::CommandBuffer,
    scratch: &mut ScreenShaderScratch,
    index: usize,
) {
    let previous = scratch.layouts[index];
    let (stage, access) = offscreen_previous_use(previous);
    pipeline_barrier(
        device,
        command_buffer,
        image_layout_barrier(
            scratch.images[index].image,
            previous,
            vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL,
            stage,
            access,
            vk::PipelineStageFlags2::COLOR_ATTACHMENT_OUTPUT,
            vk::AccessFlags2::COLOR_ATTACHMENT_WRITE,
            color_subresource_range(),
        ),
    );
    scratch.layouts[index] = vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL;
}

fn transition_to_sampled(
    device: &ash::Device,
    command_buffer: vk::CommandBuffer,
    scratch: &mut ScreenShaderScratch,
    index: usize,
) {
    pipeline_barrier(
        device,
        command_buffer,
        image_layout_barrier(
            scratch.images[index].image,
            vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL,
            vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL,
            vk::PipelineStageFlags2::COLOR_ATTACHMENT_OUTPUT,
            vk::AccessFlags2::COLOR_ATTACHMENT_WRITE,
            vk::PipelineStageFlags2::FRAGMENT_SHADER,
            vk::AccessFlags2::SHADER_SAMPLED_READ,
            color_subresource_range(),
        ),
    );
    scratch.layouts[index] = vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL;
}

/// Ресурсы renderer, нужные для записи экранных проходов: пайплайны по id и общий layout.
#[derive(Clone, Copy)]
pub(crate) struct ScreenContext<'a> {
    pub(crate) shaders: &'a [Option<ScreenShaderPipelines>],
    pub(crate) layout: vk::PipelineLayout,
}

/// Строит запись стадии из цепочки вида; `None`, если цепочка пуста или scratch не подготовлен.
/// Шейдер, уничтоженный после выбора вида, пропускается, остальные проходы сохраняют порядок.
pub(crate) fn chain_record<'a>(
    scratch: Option<&'a mut ScreenShaderScratch>,
    chain: &ScreenChain,
    context: ScreenContext<'_>,
    sampler: vk::Sampler,
) -> Option<ScreenChainRecord<'a>> {
    if chain.len() == 0 {
        return None;
    }
    let scratch = scratch?;
    let passes = screen_pass_draws(chain, |shader, stage| {
        context
            .shaders
            .get(shader.0)
            .and_then(Option::as_ref)
            .map(|pipelines| pipelines.for_stage(stage))
    });
    let record = ScreenChainRecord {
        scratch,
        sampler,
        pipeline_layout: context.layout,
        passes,
        time: chain.time,
        frame: chain.frame,
    };
    (record.len() > 0).then_some(record)
}

/// Проходы цепочки в её порядке. Шейдер без пайплайна оставляет пустую позицию, но не сдвигает
/// остальные: запись берёт непустые позиции по порядку, а ping-pong считает только их.
fn screen_pass_draws(
    chain: &ScreenChain,
    pipeline: impl Fn(ScreenShaderId, ScreenShaderStage) -> Option<vk::Pipeline>,
) -> [Option<ScreenPassDraw>; SCREEN_SHADER_MAX_PASSES] {
    let mut passes = [None; SCREEN_SHADER_MAX_PASSES];
    for (index, pass) in chain.passes.iter().flatten().enumerate() {
        passes[index] = pipeline(pass.shader, pass.stage).map(|pipeline| ScreenPassDraw {
            pipeline,
            params: pass.params,
        });
    }
    passes
}

/// Действие над scratch стадии в кадре.
#[derive(Debug, PartialEq, Eq)]
enum ScratchPlan {
    Keep,
    Create,
    Release,
}

/// Пустая цепочка освобождает scratch, непустая создаёт его при отсутствии или смене размера.
fn screen_scratch_plan(
    chain_len: usize,
    current: Option<vk::Extent2D>,
    extent: vk::Extent2D,
) -> ScratchPlan {
    match (chain_len, current) {
        (0, None) => ScratchPlan::Keep,
        (0, Some(_)) => ScratchPlan::Release,
        (_, Some(current)) if current == extent => ScratchPlan::Keep,
        (_, _) => ScratchPlan::Create,
    }
}

/// Создаёт, пересоздаёт или освобождает scratch стадии по длине цепочки.
/// Освобождение сразу безопасно: scratch принадлежит кадровому слоту, чей fence уже дождан,
/// и пустая цепочка его не записывает. Так же заменяется scratch при смене размера.
#[allow(clippy::too_many_arguments)]
pub(crate) fn prepare_screen_scratch(
    slot: &mut Option<ScreenShaderScratch>,
    chain_len: usize,
    extent: vk::Extent2D,
    format: vk::Format,
    instance: &ash::Instance,
    device: &ash::Device,
    physical_device: vk::PhysicalDevice,
    set_layout: Option<vk::DescriptorSetLayout>,
) -> Result<(), RenderError> {
    match screen_scratch_plan(
        chain_len,
        slot.as_ref().map(|scratch| scratch.extent),
        extent,
    ) {
        ScratchPlan::Keep => Ok(()),
        ScratchPlan::Release => {
            *slot = None;
            Ok(())
        }
        ScratchPlan::Create => {
            let set_layout = set_layout.ok_or(RenderError::InvalidState(
                "layout экранных шейдеров отсутствует",
            ))?;
            *slot = Some(ScreenShaderScratch::new(
                instance,
                device,
                physical_device,
                extent,
                format,
                set_layout,
            )?);
            Ok(())
        }
    }
}

/// Байты SPIR-V в порядке хоста, как их ожидает `create_post_pipeline`.
pub(crate) fn spirv_bytes(words: &[u32]) -> Vec<u8> {
    words.iter().flat_map(|word| word.to_ne_bytes()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Все `.glsl` примеров компилируются для обеих стадий: пример не должен расходиться с renderer.
    #[test]
    fn example_screen_shaders_compile_for_both_stages() {
        fn collect(directory: &std::path::Path, found: &mut Vec<std::path::PathBuf>) {
            for entry in std::fs::read_dir(directory).expect("каталог примеров") {
                let path = entry.expect("элемент каталога").path();
                if path.is_dir() {
                    collect(&path, found);
                } else if path
                    .extension()
                    .is_some_and(|extension| extension == "glsl")
                {
                    found.push(path);
                }
            }
        }
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples");
        let mut files = Vec::new();
        collect(&root, &mut files);
        assert!(
            files.len() >= 3,
            "в примерах должны быть экранные шейдеры: {files:?}"
        );
        for file in files {
            let source = std::fs::read_to_string(&file).expect("исходник примера");
            for stage in stages() {
                if let Err(error) = compile_screen_shader(&source, stage) {
                    panic!("{}: {error}", file.display());
                }
            }
        }
    }

    #[test]
    fn chain_result_alternates_between_ping_pong_images() {
        assert_eq!(chain_output_index(0), None);
        assert_eq!(chain_output_index(1), Some(0));
        assert_eq!(chain_output_index(2), Some(1));
        assert_eq!(chain_output_index(3), Some(0));
        assert_eq!(chain_output_index(4), Some(1));
    }

    #[test]
    fn unknown_shader_reference_is_detected() {
        let pass = |id| {
            Some(ScreenShaderPass {
                shader: ScreenShaderId(id),
                stage: ScreenShaderStage::AfterPost,
                params: [0.0; SCREEN_SHADER_PARAM_COUNT],
            })
        };
        let shaders = ScreenShaders {
            passes: [pass(1), None, pass(3), None],
            time: 0.0,
        };
        assert!(shaders.all_known(|id| id.0 != 2));
        assert!(!shaders.all_known(|id| id.0 != 3));
    }

    #[test]
    fn non_finite_parameters_and_time_are_rejected() {
        let mut shaders = ScreenShaders {
            passes: [
                Some(ScreenShaderPass {
                    shader: ScreenShaderId(0),
                    stage: ScreenShaderStage::BeforePost,
                    params: [0.0; SCREEN_SHADER_PARAM_COUNT],
                }),
                None,
                None,
                None,
            ],
            time: 1.0,
        };
        assert!(shaders.validate().is_ok());
        shaders.passes[0].as_mut().unwrap().params[7] = f32::NAN;
        assert!(shaders.validate().is_err());
        shaders.passes[0].as_mut().unwrap().params[7] = 0.0;
        shaders.time = f32::INFINITY;
        assert!(shaders.validate().is_err());
    }

    #[test]
    fn stage_counts_split_enabled_passes() {
        let pass = |stage| {
            Some(ScreenShaderPass {
                shader: ScreenShaderId(0),
                stage,
                params: [0.0; SCREEN_SHADER_PARAM_COUNT],
            })
        };
        let shaders = ScreenShaders {
            passes: [
                pass(ScreenShaderStage::BeforePost),
                pass(ScreenShaderStage::AfterPost),
                None,
                pass(ScreenShaderStage::BeforePost),
            ],
            time: 0.0,
        };
        assert_eq!(shaders.count(ScreenShaderStage::BeforePost), 2);
        assert_eq!(shaders.count(ScreenShaderStage::AfterPost), 1);
    }

    const IDENTITY: &str = "vec3 rve_shade(vec2 uv) {\n    return rve_sample(uv).rgb;\n}\n";

    fn stages() -> [ScreenShaderStage; 2] {
        [ScreenShaderStage::BeforePost, ScreenShaderStage::AfterPost]
    }

    #[test]
    fn identity_shader_compiles_for_both_stages() {
        for stage in stages() {
            let words = compile_screen_shader(IDENTITY, stage).expect("identity шейдер");
            assert_eq!(words[0], 0x0723_0203, "SPIR-V magic");
        }
    }

    #[test]
    fn shader_uses_every_prelude_resource_and_push_block() {
        let source = "
float tint_scale() { return rve_param(0) + rve.resolution.x * 0.0 + rve.content.x + rve.time.y; }
vec3 rve_shade(vec2 uv) {
    vec4 sample_value = rve_sample(uv);
    return sample_value.rgb * tint_scale() + vec3(rve.params[3].w, rve.resolution.zw, 0.0);
}
";
        for stage in stages() {
            compile_screen_shader(source, stage).expect("шейдер с полным prelude");
        }
    }

    #[test]
    fn syntax_error_reports_user_line_number() {
        let source = "vec3 rve_shade(vec2 uv) {\n    return unknown_helper(uv);\n}\n";
        let error = compile_screen_shader(source, ScreenShaderStage::AfterPost)
            .expect_err("неизвестная функция");
        match error {
            ScreenShaderError::Syntax { line, .. } => assert_eq!(line, Some(2)),
            other => panic!("ожидалась синтаксическая ошибка, получено {other:?}"),
        }
    }

    #[test]
    fn own_version_directive_is_rejected_with_line() {
        let source = "#version 450\nvec3 rve_shade(vec2 uv) { return vec3(uv, 0.0); }\n";
        let error =
            compile_screen_shader(source, ScreenShaderStage::BeforePost).expect_err("своя версия");
        assert_eq!(
            error,
            ScreenShaderError::Forbidden {
                line: 1,
                reason: "директивы препроцессора (#version, #extension и т. п.) задаёт движок",
            }
        );
    }

    #[test]
    fn own_main_is_rejected() {
        let source = "vec3 rve_shade(vec2 uv) { return vec3(uv, 0.0); }\nvoid main() {}\n";
        for stage in stages() {
            assert!(compile_screen_shader(source, stage).is_err(), "свой main");
        }
    }

    #[test]
    fn missing_rve_shade_is_rejected() {
        let source = "vec3 shade_other(vec2 uv) { return vec3(uv, 0.0); }\n";
        assert!(matches!(
            compile_screen_shader(source, ScreenShaderStage::AfterPost),
            Err(ScreenShaderError::Syntax { .. } | ScreenShaderError::Interface(_))
        ));
    }

    #[test]
    fn rve_shade_with_wrong_signature_is_rejected() {
        let source = "vec4 rve_shade(vec2 uv) { return vec4(uv, 0.0, 1.0); }\n";
        assert_eq!(
            compile_screen_shader(source, ScreenShaderStage::AfterPost),
            Err(ScreenShaderError::Interface(
                "нет функции vec3 rve_shade(vec2 uv)"
            ))
        );
    }

    #[test]
    fn extra_texture_sampler_and_buffer_are_rejected() {
        let extra = [
            "layout(set = 0, binding = 2) uniform texture2D extra_image;",
            "layout(set = 0, binding = 2) uniform sampler extra_sampler;",
            "layout(std430, set = 0, binding = 3) readonly buffer Data { float value[]; } data;",
            "layout(set = 0, binding = 4) uniform sampler2D combined;",
        ];
        for declaration in extra {
            let source =
                format!("{declaration}\nvec3 rve_shade(vec2 uv) {{ return vec3(uv, 0.0); }}\n");
            assert!(
                compile_screen_shader(&source, ScreenShaderStage::BeforePost).is_err(),
                "лишний ресурс: {declaration}"
            );
        }
    }

    #[test]
    fn extra_inputs_and_outputs_are_rejected() {
        let extra_input = "layout(location = 1) in vec2 extra_uv;\nvec3 rve_shade(vec2 uv) { return vec3(extra_uv, 0.0); }\n";
        let extra_output = "layout(location = 1) out vec4 extra_color;\nvec3 rve_shade(vec2 uv) { extra_color = vec4(1.0); return vec3(uv, 0.0); }\n";
        for source in [extra_input, extra_output] {
            assert!(
                compile_screen_shader(source, ScreenShaderStage::AfterPost).is_err(),
                "лишний интерфейс: {source}"
            );
        }
    }

    /// Блок push constants prelude, как его видит naga: член → смещение в байтах и размер блока.
    fn push_block_layout(stage: ScreenShaderStage) -> (Vec<(String, u32)>, u32) {
        let source = format!("{PRELUDE_HEAD}{IDENTITY}\n{}", stage.tail());
        let mut frontend = glsl::Frontend::default();
        let module = frontend
            .parse(&glsl::Options::from(ShaderStage::Fragment), &source)
            .expect("prelude с identity шейдером");
        let block = module
            .global_variables
            .iter()
            .find_map(|(_, global)| (global.space == AddressSpace::Immediate).then_some(global))
            .expect("блок push constants");
        let TypeInner::Struct { members, span } = &module.types[block.ty].inner else {
            panic!("блок push constants должен быть struct");
        };
        let offsets = members
            .iter()
            .map(|member| (member.name.clone().unwrap_or_default(), member.offset))
            .collect();
        (offsets, *span)
    }

    #[test]
    fn push_block_offsets_from_naga_match_the_packed_layout() {
        for stage in stages() {
            let (offsets, span) = push_block_layout(stage);
            assert_eq!(
                offsets,
                [
                    ("params".to_owned(), 0),
                    ("resolution".to_owned(), 64),
                    ("content".to_owned(), 80),
                    ("time".to_owned(), 96),
                ],
                "смещения членов блока"
            );
            assert_eq!(span, 112);
            assert_eq!(span as usize, SCREEN_PUSH_CONSTANTS_SIZE);
        }
    }

    #[test]
    fn packed_values_land_at_the_offsets_naga_reports() {
        let (offsets, _) = push_block_layout(ScreenShaderStage::AfterPost);
        let float_index = |name: &str| {
            let (_, offset) = offsets
                .iter()
                .find(|(member, _)| member == name)
                .expect("член блока");
            (*offset / 4) as usize
        };
        let params: [f32; SCREEN_SHADER_PARAM_COUNT] =
            std::array::from_fn(|index| index as f32 + 1.0);
        let bytes = screen_push_constants(&params, (640, 360), (1280, 720), 5.0, 9);
        let floats = read_floats(&bytes);
        let params_at = float_index("params");
        assert_eq!(floats[params_at..params_at + 16], params);
        let resolution_at = float_index("resolution");
        assert_eq!(
            floats[resolution_at..resolution_at + 4],
            [640.0, 360.0, 1.0 / 640.0, 1.0 / 360.0]
        );
        let content_at = float_index("content");
        assert_eq!(
            floats[content_at..content_at + 4],
            [0.5, 0.5, 1.0 / 1280.0, 1.0 / 720.0]
        );
        let time_at = float_index("time");
        assert_eq!(floats[time_at..time_at + 2], [5.0, 9.0]);
    }

    #[test]
    fn destroyed_or_missing_shader_keeps_order_of_remaining_passes() {
        use ash::vk::Handle as _;
        let pass = |id: usize| ScreenShaderPass {
            shader: ScreenShaderId(id),
            stage: ScreenShaderStage::AfterPost,
            params: [id as f32; SCREEN_SHADER_PARAM_COUNT],
        };
        let chain = ScreenChain {
            passes: [Some(pass(0)), Some(pass(1)), Some(pass(2)), None],
            time: 0.0,
            frame: 0,
        };
        // Шейдер 1 уничтожен: его позиция пустая, соседние проходы остаются на своих местах.
        let draws = screen_pass_draws(&chain, |shader, _| {
            (shader.0 != 1).then(|| vk::Pipeline::from_raw(100 + shader.0 as u64))
        });
        let drawn: Vec<(u64, f32)> = draws
            .iter()
            .flatten()
            .map(|draw| (draw.pipeline.as_raw(), draw.params[0]))
            .collect();
        assert_eq!(drawn, [(100, 0.0), (102, 2.0)]);
        assert!(draws[1].is_none(), "позиция уничтоженного шейдера пуста");
    }

    #[test]
    fn scratch_is_released_by_empty_chain_and_recreated_on_resize() {
        let small = vk::Extent2D {
            width: 64,
            height: 32,
        };
        let large = vk::Extent2D {
            width: 128,
            height: 32,
        };
        assert_eq!(
            screen_scratch_plan(0, Some(small), small),
            ScratchPlan::Release
        );
        assert_eq!(screen_scratch_plan(0, None, small), ScratchPlan::Keep);
        assert_eq!(
            screen_scratch_plan(2, Some(small), small),
            ScratchPlan::Keep
        );
        assert_eq!(
            screen_scratch_plan(2, Some(small), large),
            ScratchPlan::Create
        );
        assert_eq!(screen_scratch_plan(1, None, small), ScratchPlan::Create);
    }

    #[test]
    fn push_constants_identity_scale_when_content_equals_physical() {
        let bytes = screen_push_constants(&[0.0; 16], (1280, 720), (1280, 720), 1.0, 7);
        let floats = read_floats(&bytes);
        assert_eq!(floats[16..18], [1280.0, 720.0]);
        assert_eq!(floats[20..22], [1.0, 1.0]);
        assert_eq!(floats[24..26], [1.0, 7.0]);
    }

    #[test]
    fn push_constants_scale_content_inside_physical_for_render_scale() {
        let bytes = screen_push_constants(&[0.0; 16], (640, 360), (1280, 720), 0.0, 0);
        let floats = read_floats(&bytes);
        assert_eq!(floats[20..22], [0.5, 0.5]);
        assert_eq!(floats[22..24], [1.0 / 1280.0, 1.0 / 720.0]);
        assert_eq!(floats[18..20], [1.0 / 640.0, 1.0 / 360.0]);
    }

    #[test]
    fn push_constants_wrap_time_and_pack_parameters_in_order() {
        let mut params = [0.0_f32; 16];
        params[15] = 2.5;
        let bytes = screen_push_constants(&params, (2, 2), (2, 2), 3601.5, 0);
        let floats = read_floats(&bytes);
        assert_eq!(floats[15], 2.5);
        assert!((floats[24] - 1.5).abs() < 1e-3);
    }

    #[test]
    fn user_line_maps_only_user_source_lines() {
        assert_eq!(user_line(10, 9, 3), Some(1));
        assert_eq!(user_line(12, 9, 3), Some(3));
        assert_eq!(user_line(9, 9, 3), None, "строка заголовка");
        assert_eq!(user_line(13, 9, 3), None, "строка хвоста");
    }

    fn read_floats(bytes: &[u8; SCREEN_PUSH_CONSTANTS_SIZE]) -> [f32; 28] {
        let mut values = [0.0_f32; 28];
        for (index, value) in values.iter_mut().enumerate() {
            *value = f32::from_ne_bytes(bytes[index * 4..index * 4 + 4].try_into().unwrap());
        }
        values
    }
}
