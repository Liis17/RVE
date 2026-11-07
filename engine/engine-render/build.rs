//! Компилирует GLSL-шейдеры renderer в SPIR-V через naga.

use naga::back::spv;
use naga::front::glsl;
use naga::valid::{Capabilities, ValidationFlags, Validator};
use std::env;
use std::fs;
use std::path::PathBuf;

const CLUSTER_GRID_X: u32 = 16;
const CLUSTER_GRID_Y: u32 = 9;
const CLUSTER_GRID_Z: u32 = 24;
const MAX_CLUSTER_LIGHTS: usize = 128;
const MAX_RENDERED_LIGHTS: usize = 1024;
const MAX_LOCAL_SHADOW_LAYERS: usize = 32;
const LOCAL_SHADOW_MAP_RESOLUTION: u32 = 1024;
const CLUSTER_COMPUTE_LOCAL_SIZE: u32 = 64;
const SKIN_COMPUTE_LOCAL_SIZE: u32 = 64;

type ShaderBuild = (
    &'static str,
    naga::ShaderStage,
    &'static [(&'static str, &'static str)],
    &'static str,
);

const SHADERS: &[ShaderBuild] = &[
    (
        "equirect_to_cube.comp",
        naga::ShaderStage::Compute,
        &[],
        "equirect_to_cube.comp.spv",
    ),
    ("sky.comp", naga::ShaderStage::Compute, &[], "sky.comp.spv"),
    (
        "irradiance.comp",
        naga::ShaderStage::Compute,
        &[],
        "irradiance.comp.spv",
    ),
    (
        "prefilter.comp",
        naga::ShaderStage::Compute,
        &[],
        "prefilter.comp.spv",
    ),
    (
        "cluster.comp",
        naga::ShaderStage::Compute,
        &[],
        "cluster.comp.spv",
    ),
    (
        "skin.comp",
        naga::ShaderStage::Compute,
        &[],
        "skin.comp.spv",
    ),
    ("mesh.vert", naga::ShaderStage::Vertex, &[], "mesh.vert.spv"),
    (
        "mesh.frag",
        naga::ShaderStage::Fragment,
        &[],
        "mesh.frag.opaque.spv",
    ),
    (
        "mesh.frag",
        naga::ShaderStage::Fragment,
        &[("ALPHA_MASK", "1")],
        "mesh.frag.mask.spv",
    ),
    (
        "mesh.frag",
        naga::ShaderStage::Fragment,
        &[("ALPHA_BLEND", "1")],
        "mesh.frag.blend.spv",
    ),
    (
        "wireframe.frag",
        naga::ShaderStage::Fragment,
        &[],
        "wireframe.frag.spv",
    ),
    (
        "shadow.vert",
        naga::ShaderStage::Vertex,
        &[],
        "shadow.vert.spv",
    ),
    (
        "shadow.vert",
        naga::ShaderStage::Vertex,
        &[("LOCAL_SHADOW", "1")],
        "shadow.local.vert.spv",
    ),
    (
        "shadow.frag",
        naga::ShaderStage::Fragment,
        &[],
        "shadow.frag.spv",
    ),
    (
        "overlay.vert",
        naga::ShaderStage::Vertex,
        &[],
        "overlay.vert.spv",
    ),
    (
        "overlay.frag",
        naga::ShaderStage::Fragment,
        &[],
        "overlay.frag.spv",
    ),
    ("ui.vert", naga::ShaderStage::Vertex, &[], "ui.vert.spv"),
    ("ui.frag", naga::ShaderStage::Fragment, &[], "ui.frag.spv"),
    (
        "fullscreen.vert",
        naga::ShaderStage::Vertex,
        &[],
        "fullscreen.vert.spv",
    ),
    (
        "tonemap.frag",
        naga::ShaderStage::Fragment,
        &[],
        "tonemap.frag.spv",
    ),
    (
        "output.frag",
        naga::ShaderStage::Fragment,
        &[],
        "output.frag.spv",
    ),
    ("taa.frag", naga::ShaderStage::Fragment, &[], "taa.frag.spv"),
    (
        "skybox.vert",
        naga::ShaderStage::Vertex,
        &[],
        "skybox.vert.spv",
    ),
    (
        "skybox.frag",
        naga::ShaderStage::Fragment,
        &[],
        "skybox.frag.spv",
    ),
];

fn main() {
    if env::var_os("CARGO_FEATURE_DLSS").is_some() {
        let target_os = env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
        let target_arch = env::var("CARGO_CFG_TARGET_ARCH").unwrap_or_default();
        let target_env = env::var("CARGO_CFG_TARGET_ENV").unwrap_or_default();
        if target_os != "windows" || target_arch != "x86_64" || target_env != "msvc" {
            panic!("engine-render feature `dlss` requires the Windows x86_64 MSVC target");
        }
        let sdk_root = env::var_os("RVE_DLSS_SDK").unwrap_or_else(|| {
            panic!("engine-render feature `dlss` requires RVE_DLSS_SDK to point to the NVIDIA DLSS SDK root")
        });
        let sdk_root = PathBuf::from(sdk_root);
        let library_directory = sdk_root.join("lib/Windows_x86_64/x64");
        let library = library_directory.join("nvsdk_ngx_s.lib");
        if !library.is_file() {
            panic!(
                "NVIDIA DLSS SDK library is missing: {}; set RVE_DLSS_SDK to a Windows_x86_64 SDK root containing lib/Windows_x86_64/x64/nvsdk_ngx_s.lib",
                library.display()
            );
        }
        println!(
            "cargo:rustc-link-search=native={}",
            library_directory.display()
        );
        println!("cargo:rustc-link-lib=static=nvsdk_ngx_s");
        println!("cargo:rerun-if-env-changed=RVE_DLSS_SDK");
    }
    let out_dir = PathBuf::from(env::var_os("OUT_DIR").expect("Cargo задаёт OUT_DIR"));
    let cluster_count = CLUSTER_GRID_X * CLUSTER_GRID_Y * CLUSTER_GRID_Z;
    fs::write(
        out_dir.join("render_constants.rs"),
        format!(
            "pub const CLUSTER_GRID_X: usize = {CLUSTER_GRID_X};\n\
             pub const CLUSTER_GRID_Y: usize = {CLUSTER_GRID_Y};\n\
             pub const CLUSTER_GRID_Z: usize = {CLUSTER_GRID_Z};\n\
             pub const MAX_CLUSTER_LIGHTS: usize = {MAX_CLUSTER_LIGHTS};\n\
             pub const MAX_RENDERED_LIGHTS: usize = {MAX_RENDERED_LIGHTS};\n\
             /// Maximum point and spot shadow layers shared by a renderer.\n\
             pub const MAX_LOCAL_SHADOW_LAYERS: usize = {MAX_LOCAL_SHADOW_LAYERS};\n\
             /// Resolution of each shared point or spot shadow layer.\n\
             pub const LOCAL_SHADOW_MAP_RESOLUTION: u32 = {LOCAL_SHADOW_MAP_RESOLUTION};\n\
             pub const CLUSTER_COMPUTE_LOCAL_SIZE: u32 = {CLUSTER_COMPUTE_LOCAL_SIZE};\n\
             pub const CLUSTER_COUNT: usize = {cluster_count};\n"
        ),
    )
    .expect("не удалось записать constants renderer");
    let mut frontend = glsl::Frontend::default();
    for &(name, stage, defines, output) in SHADERS {
        let path = PathBuf::from("shaders").join(name);
        println!("cargo::rerun-if-changed={}", path.display());
        let source = fs::read_to_string(&path)
            .unwrap_or_else(|error| panic!("не удалось прочитать {}: {error}", path.display()));
        let mut shader_options = glsl::Options::from(stage);
        shader_options.defines.extend(
            defines
                .iter()
                .map(|(name, value)| ((*name).to_owned(), (*value).to_owned())),
        );
        if name == "mesh.frag" || name == "cluster.comp" {
            shader_options.defines.extend([
                ("CLUSTER_GRID_X".to_owned(), CLUSTER_GRID_X.to_string()),
                ("CLUSTER_GRID_Y".to_owned(), CLUSTER_GRID_Y.to_string()),
                ("CLUSTER_GRID_Z".to_owned(), CLUSTER_GRID_Z.to_string()),
                (
                    "MAX_CLUSTER_LIGHTS".to_owned(),
                    MAX_CLUSTER_LIGHTS.to_string(),
                ),
            ]);
        }
        if name == "mesh.frag" {
            shader_options.defines.extend([
                (
                    "MAX_LOCAL_SHADOW_LAYERS".to_owned(),
                    MAX_LOCAL_SHADOW_LAYERS.to_string(),
                ),
                (
                    "LOCAL_SHADOW_MAP_RESOLUTION".to_owned(),
                    LOCAL_SHADOW_MAP_RESOLUTION.to_string(),
                ),
            ]);
        }
        if name == "shadow.vert" && shader_options.defines.contains_key("LOCAL_SHADOW") {
            shader_options.defines.insert(
                "MAX_LOCAL_SHADOW_LAYERS".to_owned(),
                MAX_LOCAL_SHADOW_LAYERS.to_string(),
            );
        }
        if name == "skin.comp" {
            shader_options.defines.insert(
                "SKIN_COMPUTE_LOCAL_SIZE".to_owned(),
                SKIN_COMPUTE_LOCAL_SIZE.to_string(),
            );
        }
        if name == "cluster.comp" {
            shader_options.defines.insert(
                "MAX_RENDERED_LIGHTS".to_owned(),
                MAX_RENDERED_LIGHTS.to_string(),
            );
            shader_options.defines.insert(
                "CLUSTER_COMPUTE_LOCAL_SIZE".to_owned(),
                CLUSTER_COMPUTE_LOCAL_SIZE.to_string(),
            );
        }
        let module = frontend
            .parse(&shader_options, &source)
            .unwrap_or_else(|error| {
                panic!("{name}: {}", error.emit_to_string(&source));
            });
        let info = Validator::new(ValidationFlags::all(), Capabilities::IMMEDIATES)
            .validate(&module)
            .unwrap_or_else(|error| panic!("{name}: {}", error.emit_to_string(&source)));
        // Координаты не переворачиваются: шейдер следует соглашениям Vulkan,
        // а ось Y направляет вверх отрицательная высота viewport в renderer.
        let options = spv::Options {
            flags: spv::Options::default().flags - spv::WriterFlags::ADJUST_COORDINATE_SPACE,
            ..spv::Options::default()
        };
        let words = spv::write_vec(&module, &info, &options, None)
            .unwrap_or_else(|error| panic!("{name}: {error}"));
        let bytes = words
            .iter()
            .flat_map(|word| word.to_le_bytes())
            .collect::<Vec<_>>();
        fs::write(out_dir.join(output), bytes)
            .unwrap_or_else(|error| panic!("не удалось записать {output}: {error}"));
    }
}
