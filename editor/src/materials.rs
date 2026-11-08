//! Parsed material and surface-shader assets used by Inspector and viewport rendering.

use crate::asset_watch::AssetChanges;
use crate::assets_fs::{AssetEntry, AssetKind};
use crate::project::OpenProject;
use engine_render::{
    SurfaceLighting, SurfaceProperty, SurfacePropertyKind, SurfaceShaderSource,
    SurfaceTextureFallback, TextureColorSpace,
};
use engine_scene::{
    MaterialAsset, ShaderAsset, ShaderColorSpace, ShaderLighting, ShaderPropertyDefault,
    ShaderPropertyKind, TextureFallback,
};
use std::collections::HashMap;

#[derive(Clone, Debug)]
pub struct CachedMaterial {
    pub asset: Option<MaterialAsset>,
    /// In-memory edits are rendered immediately and persisted when the UI commits the edit.
    pub draft: Option<MaterialAsset>,
    pub revision: u64,
    pub error: Option<String>,
}

impl CachedMaterial {
    pub fn effective_asset(&self) -> Option<&MaterialAsset> {
        self.draft.as_ref().or(self.asset.as_ref())
    }
}

#[derive(Clone, Debug)]
pub struct CachedShader {
    pub asset: Option<ShaderAsset>,
    pub source: Option<String>,
    pub revision: u64,
    pub error: Option<String>,
}

/// Project asset cache. A failed external edit records its error while keeping the last valid
/// parsed version available to the renderer.
#[derive(Default)]
pub struct MaterialLibrary {
    materials: HashMap<String, CachedMaterial>,
    shaders: HashMap<String, CachedShader>,
    next_revision: u64,
}

impl MaterialLibrary {
    pub fn refresh_all(&mut self, project: &OpenProject, entries: &[AssetEntry]) {
        let mut materials = Vec::new();
        let mut shaders = Vec::new();
        collect(entries, &mut materials, &mut shaders);
        self.materials.retain(|path, _| materials.contains(path));
        self.shaders.retain(|path, _| shaders.contains(path));
        for path in materials {
            self.reload_material(project, &path);
        }
        for path in shaders {
            self.reload_shader(project, &path);
        }
    }

    pub fn refresh_changed(&mut self, project: &OpenProject, changes: &AssetChanges) {
        if changes.rescan {
            return;
        }
        for path in changes.material_paths() {
            self.reload_material(project, path.as_str());
        }
        for path in changes.shader_paths() {
            self.reload_shader(project, path.as_str());
        }
        for path in &changes.directory_paths {
            for asset in self.materials.keys().cloned().collect::<Vec<_>>() {
                if asset.starts_with(path.as_str()) {
                    self.reload_material(project, &asset);
                }
            }
            for asset in self.shaders.keys().cloned().collect::<Vec<_>>() {
                if asset.starts_with(path.as_str()) {
                    self.reload_shader(project, &asset);
                }
            }
        }
    }

    pub fn material(&self, path: &str) -> Option<&CachedMaterial> {
        self.materials.get(path)
    }

    pub fn shader(&self, path: &str) -> Option<&CachedShader> {
        self.shaders.get(path)
    }

    pub fn shader_paths(&self) -> impl Iterator<Item = &str> {
        self.shaders.keys().map(String::as_str)
    }

    pub fn shader_source(&self, path: &str) -> Option<SurfaceShaderSource> {
        let shader = self.shaders.get(path)?.asset.as_ref()?;
        Some(surface_source(shader))
    }

    /// Applies an editor draft to the live material cache without writing the asset yet.
    pub fn set_material_draft(&mut self, path: &str, draft: MaterialAsset) {
        let Some(entry) = self.materials.get_mut(path) else {
            return;
        };
        if entry.effective_asset() == Some(&draft) {
            return;
        }
        self.next_revision = self
            .next_revision
            .max(entry.revision)
            .wrapping_add(1)
            .max(1);
        entry.revision = self.next_revision;
        entry.draft = Some(draft);
    }

    /// Marks an atomically written material as committed and clears its in-memory draft.
    pub fn mark_material_saved(&mut self, path: &str, asset: MaterialAsset) {
        let Some(entry) = self.materials.get_mut(path) else {
            return;
        };
        if entry.asset.as_ref() != Some(&asset) || entry.draft.is_some() || entry.error.is_some() {
            self.next_revision = self
                .next_revision
                .max(entry.revision)
                .wrapping_add(1)
                .max(1);
            entry.revision = self.next_revision;
        }
        entry.asset = Some(asset);
        entry.draft = None;
        entry.error = None;
    }

    fn reload_material(&mut self, project: &OpenProject, path: &str) {
        let source = project.read_text(path);
        self.next_revision = self.next_revision.wrapping_add(1).max(1);
        let revision = self.next_revision;
        let entry = self
            .materials
            .entry(path.to_owned())
            .or_insert(CachedMaterial {
                asset: None,
                draft: None,
                revision,
                error: None,
            });
        match source.and_then(|source| {
            MaterialAsset::parse(path, &source)
                .map(|asset| (source, asset))
                .map_err(|error| error.to_string())
        }) {
            Ok((_, asset)) => {
                let source_changed = entry.asset.as_ref() != Some(&asset) || entry.error.is_some();
                if source_changed {
                    entry.revision = revision;
                    entry.draft = None;
                } else if entry.draft.as_ref() == Some(&asset) {
                    entry.draft = None;
                }
                entry.asset = Some(asset);
                entry.error = None;
            }
            Err(error) => entry.error = Some(error),
        }
    }

    fn reload_shader(&mut self, project: &OpenProject, path: &str) {
        let source = project.read_text(path);
        self.next_revision = self.next_revision.wrapping_add(1).max(1);
        let revision = self.next_revision;
        let entry = self.shaders.entry(path.to_owned()).or_insert(CachedShader {
            asset: None,
            source: None,
            revision,
            error: None,
        });
        match source.and_then(|source| {
            ShaderAsset::parse(path, &source)
                .map(|asset| (source, asset))
                .map_err(|error| error.to_string())
        }) {
            Ok((source, asset)) => {
                if entry.source.as_deref() != Some(&source) || entry.error.is_some() {
                    entry.revision = revision;
                }
                entry.source = Some(source);
                entry.asset = Some(asset);
                entry.error = None;
            }
            Err(error) => entry.error = Some(error),
        }
    }
}

pub fn surface_source(shader: &ShaderAsset) -> SurfaceShaderSource {
    let schema = shader.layout();
    SurfaceShaderSource {
        lighting: match shader.lighting {
            ShaderLighting::Lit => SurfaceLighting::Lit,
            ShaderLighting::Unlit => SurfaceLighting::Unlit,
        },
        properties: schema
            .properties
            .into_iter()
            .map(|layout| {
                let property = layout.property;
                SurfaceProperty {
                    name: property.name,
                    kind: match property.kind {
                        ShaderPropertyKind::Float => SurfacePropertyKind::Float,
                        ShaderPropertyKind::Vec2 => SurfacePropertyKind::Vec2,
                        ShaderPropertyKind::Vec3 => SurfacePropertyKind::Vec3,
                        ShaderPropertyKind::Vec4 => SurfacePropertyKind::Vec4,
                        ShaderPropertyKind::Color => SurfacePropertyKind::Color,
                        ShaderPropertyKind::Texture => SurfacePropertyKind::Texture,
                    },
                    slot: layout.slot,
                    color_space: property.color_space.map(|space| match space {
                        ShaderColorSpace::Srgb => TextureColorSpace::Srgb,
                        ShaderColorSpace::Linear => TextureColorSpace::Linear,
                    }),
                    fallback: match property.fallback.unwrap_or_default() {
                        TextureFallback::White => SurfaceTextureFallback::White,
                        TextureFallback::Black => SurfaceTextureFallback::Black,
                        TextureFallback::Normal => SurfaceTextureFallback::Normal,
                    },
                }
            })
            .collect(),
        code: shader.code.clone(),
    }
}

pub fn shader_schema(shader: Option<&ShaderAsset>) -> engine_scene::ShaderSchema {
    shader.map_or_else(engine_scene::ShaderSchema::lit, ShaderAsset::layout)
}

pub fn material_to_toml_value(property: &engine_scene::ShaderProperty) -> Option<toml::Value> {
    match &property.default {
        ShaderPropertyDefault::Numeric(values) if values.len() == 1 => {
            Some(toml::Value::Float(f64::from(values[0])))
        }
        ShaderPropertyDefault::Numeric(values) => Some(toml::Value::Array(
            values
                .iter()
                .map(|value| toml::Value::Float(f64::from(*value)))
                .collect(),
        )),
        ShaderPropertyDefault::Texture(path) => path
            .as_ref()
            .map(|path| toml::Value::String(path.as_str().to_owned())),
    }
}

fn collect(entries: &[AssetEntry], materials: &mut Vec<String>, shaders: &mut Vec<String>) {
    for entry in entries {
        if entry.is_dir {
            collect(&entry.children, materials, shaders);
        } else {
            match AssetKind::of(&entry.path) {
                AssetKind::Material => materials.push(entry.path.clone()),
                AssetKind::Shader => shaders.push(entry.path.clone()),
                _ => {}
            }
        }
    }
}

/// Sorts material and shader paths for stable inspector choices.
pub fn sorted_paths(paths: impl Iterator<Item = String>) -> Vec<String> {
    let mut paths = paths.collect::<Vec<_>>();
    paths.sort();
    paths
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parsed_surface_source_uses_schema_slots_and_property_types() {
        let shader =
            ShaderAsset::parse("surface.rveshader", &ShaderAsset::template("Surface")).unwrap();
        let source = surface_source(&shader);
        assert_eq!(source.lighting, SurfaceLighting::Lit);
        assert_eq!(source.properties[0].name, "tint");
        assert_eq!(source.properties[0].slot, 0);
        assert_eq!(source.properties[0].kind, SurfacePropertyKind::Color);
        assert_eq!(source.properties[1].kind, SurfacePropertyKind::Texture);
    }

    #[test]
    fn material_draft_updates_revision_and_committing_clears_it() {
        let committed = MaterialAsset::default();
        let mut library = MaterialLibrary::default();
        library.materials.insert(
            "materials/test.rvemat".to_owned(),
            CachedMaterial {
                asset: Some(committed.clone()),
                draft: None,
                revision: 1,
                error: None,
            },
        );
        let mut draft = committed.clone();
        draft.double_sided = true;
        library.set_material_draft("materials/test.rvemat", draft.clone());
        let cached = library.material("materials/test.rvemat").unwrap();
        assert!(cached.revision > 1);
        assert_eq!(cached.effective_asset(), Some(&draft));
        assert_eq!(cached.asset.as_ref(), Some(&committed));

        library.mark_material_saved("materials/test.rvemat", draft.clone());
        let cached = library.material("materials/test.rvemat").unwrap();
        assert_eq!(cached.effective_asset(), Some(&draft));
        assert!(cached.draft.is_none());
        assert_eq!(cached.asset.as_ref(), Some(&draft));
    }

    #[test]
    fn failed_shader_reload_keeps_the_last_valid_asset() {
        static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let root = std::env::temp_dir().join(format!(
            "rve-editor-materials-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        let assets = root.join("Assets");
        std::fs::create_dir_all(assets.join("shaders")).unwrap();
        std::fs::write(
            root.join("project.rveproject"),
            "id = \"fixture\"\nname = \"Fixture\"\nversion = \"1\"\n\n[[scenes]]\npath = \"/main.rvescene\"\n",
        )
        .unwrap();
        let shader_path = assets.join("shaders/surface.rveshader");
        std::fs::write(&shader_path, ShaderAsset::template("Surface")).unwrap();
        let project = OpenProject::open(&root).unwrap();
        let mut library = MaterialLibrary::default();
        library.reload_shader(&project, "shaders/surface.rveshader");
        let cached = library.shader("shaders/surface.rveshader").unwrap();
        let valid_asset = cached.asset.clone().unwrap();
        let revision = cached.revision;

        std::fs::write(&shader_path, "[shader]\nlighting = \"invalid\"\n---\n").unwrap();
        library.reload_shader(&project, "shaders/surface.rveshader");
        let cached = library.shader("shaders/surface.rveshader").unwrap();
        assert_eq!(cached.asset.as_ref(), Some(&valid_asset));
        assert_eq!(cached.revision, revision);
        assert!(cached.error.is_some());
        drop(project);
        std::fs::remove_dir_all(root).unwrap();
    }
}
