//! Asset categories shared by project tools and script parameter schemas.

/// Asset kind inferred from a virtual path's file extension.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum AssetKind {
    Scene,
    Prefab,
    CameraSettings,
    PostProcess,
    PhysicsMaterial,
    Material,
    Shader,
    Model,
    Texture,
    Hdr,
    Audio,
    Script,
    Other,
}

impl AssetKind {
    pub fn of(path: &str) -> Self {
        let extension = path
            .rsplit('/')
            .next()
            .and_then(|name| name.rsplit_once('.'))
            .map(|(_, extension)| extension.to_ascii_lowercase());
        match extension.as_deref() {
            Some("rvescene") => Self::Scene,
            Some("rveprefab") => Self::Prefab,
            Some("camerasettings") => Self::CameraSettings,
            Some("postprocess") => Self::PostProcess,
            Some("rvephysmat") => Self::PhysicsMaterial,
            Some("rvemat") => Self::Material,
            Some("rveshader") => Self::Shader,
            Some("fbx") => Self::Model,
            Some("png") => Self::Texture,
            Some("hdr") => Self::Hdr,
            Some("wav" | "ogg") => Self::Audio,
            Some("lua") => Self::Script,
            _ => Self::Other,
        }
    }

    pub fn parse(name: &str) -> Option<Self> {
        Some(match name {
            "scene" => Self::Scene,
            "prefab" => Self::Prefab,
            "camera_settings" => Self::CameraSettings,
            "post_process" => Self::PostProcess,
            "physics_material" => Self::PhysicsMaterial,
            "material" => Self::Material,
            "shader" => Self::Shader,
            "model" => Self::Model,
            "texture" => Self::Texture,
            "hdr" => Self::Hdr,
            "audio" => Self::Audio,
            "script" => Self::Script,
            "other" => Self::Other,
            _ => return None,
        })
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::Scene => "scene",
            Self::Prefab => "prefab",
            Self::CameraSettings => "camera_settings",
            Self::PostProcess => "post_process",
            Self::PhysicsMaterial => "physics_material",
            Self::Material => "material",
            Self::Shader => "shader",
            Self::Model => "model",
            Self::Texture => "texture",
            Self::Hdr => "hdr",
            Self::Audio => "audio",
            Self::Script => "script",
            Self::Other => "other",
        }
    }
}
