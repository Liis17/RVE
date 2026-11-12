//! Ошибка renderer `RenderError`.

use crate::*;

/// Ошибка загрузки Vulkan или создания рендерера.
#[derive(Debug)]
pub enum RenderError {
    InvalidConfig(&'static str),
    Loader(String),
    Handle(HandleError),
    Vulkan(vk::Result),
    Unsupported(String),
    NoSuitableDevice(Vec<String>),
    InvalidState(&'static str),
    InvalidMesh(&'static str),
    InvalidTexture(String),
    InvalidMaterial(String),
    InvalidShader(String),
    InvalidEnvironment(String),
    ScreenShader(ScreenShaderError),
}

impl fmt::Display for RenderError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ScreenShader(error) => write!(f, "экранный шейдер не создан: {error}"),
            Self::InvalidConfig(message) => write!(f, "неверная конфигурация рендерера: {message}"),
            Self::Loader(message) => write!(f, "не удалось загрузить Vulkan: {message}"),
            Self::Handle(error) => write!(f, "не удалось получить window handle: {error}"),
            Self::Vulkan(error) => write!(f, "ошибка Vulkan: {error:?}"),
            Self::Unsupported(message) => {
                write!(f, "неподдерживаемая конфигурация Vulkan: {message}")
            }
            Self::NoSuitableDevice(reasons) => {
                write!(f, "не найден подходящий Vulkan GPU")?;
                for reason in reasons {
                    write!(f, "; {reason}")?;
                }
                Ok(())
            }
            Self::InvalidState(message) => write!(f, "некорректное состояние рендерера: {message}"),
            Self::InvalidMesh(message) => write!(f, "некорректный меш: {message}"),
            Self::InvalidTexture(message) => write!(f, "некорректная текстура: {message}"),
            Self::InvalidMaterial(message) => write!(f, "некорректный материал: {message}"),
            Self::InvalidShader(message) => write!(f, "некорректный surface shader: {message}"),
            Self::InvalidEnvironment(message) => write!(f, "некорректное окружение: {message}"),
        }
    }
}

impl Error for RenderError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Handle(error) => Some(error),
            _ => None,
        }
    }
}

impl From<vk::Result> for RenderError {
    fn from(error: vk::Result) -> Self {
        Self::Vulkan(error)
    }
}
