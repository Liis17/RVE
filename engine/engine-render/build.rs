//! Компилирует GLSL-шейдеры renderer в SPIR-V через naga.

use naga::back::spv;
use naga::front::glsl;
use naga::valid::{Capabilities, ValidationFlags, Validator};
use std::env;
use std::fs;
use std::path::PathBuf;

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
        "shadow.vert",
        naga::ShaderStage::Vertex,
        &[],
        "shadow.vert.spv",
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
    let out_dir = PathBuf::from(env::var_os("OUT_DIR").expect("Cargo задаёт OUT_DIR"));
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
