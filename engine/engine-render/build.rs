//! Компилирует GLSL-шейдеры renderer в SPIR-V через naga.

use naga::back::spv;
use naga::front::glsl;
use naga::valid::{Capabilities, ValidationFlags, Validator};
use std::env;
use std::fs;
use std::path::PathBuf;

const SHADERS: [(&str, naga::ShaderStage); 5] = [
    ("mesh.vert", naga::ShaderStage::Vertex),
    ("mesh.frag", naga::ShaderStage::Fragment),
    ("shadow.vert", naga::ShaderStage::Vertex),
    ("overlay.vert", naga::ShaderStage::Vertex),
    ("overlay.frag", naga::ShaderStage::Fragment),
];

fn main() {
    let out_dir = PathBuf::from(env::var_os("OUT_DIR").expect("Cargo задаёт OUT_DIR"));
    let mut frontend = glsl::Frontend::default();
    for (name, stage) in SHADERS {
        let path = PathBuf::from("shaders").join(name);
        println!("cargo::rerun-if-changed={}", path.display());
        let source = fs::read_to_string(&path)
            .unwrap_or_else(|error| panic!("не удалось прочитать {}: {error}", path.display()));
        let module = frontend
            .parse(&glsl::Options::from(stage), &source)
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
        fs::write(out_dir.join(format!("{name}.spv")), bytes)
            .unwrap_or_else(|error| panic!("не удалось записать {name}.spv: {error}"));
    }
}
