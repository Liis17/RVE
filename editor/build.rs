//! Растеризует SVG из `IconsPack` в предумноженные RGBA-буферы для иконок панели Project.
//!
//! Каждый файл `IconsPack/**/*.svg` превращается в `icon_<n>.rgba` в `OUT_DIR` размером
//! 128×128. Список `icons_generated.rs` подключает `editor/src/icons.rs`. Отсутствующий
//! каталог даёт пустой список: редактор тогда рисует прежнюю заглушку.

use resvg::{tiny_skia, usvg};
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

const ICON_SIZE: u32 = 128;

fn main() {
    if let Err(error) = run() {
        panic!("не удалось подготовить иконки из IconsPack: {error}");
    }
}

fn run() -> Result<(), String> {
    println!("cargo:rerun-if-changed=build.rs");
    let icons_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../IconsPack");
    let out_dir = PathBuf::from(std::env::var("OUT_DIR").map_err(|error| error.to_string())?);

    let mut svgs = Vec::new();
    collect_svgs(&icons_dir, &mut svgs)?;

    let mut list = String::from("&[\n");
    for (index, svg) in svgs.iter().enumerate() {
        let rgba = render(svg)?;
        let rgba_path = out_dir.join(format!("icon_{index}.rgba"));
        std::fs::write(&rgba_path, rgba).map_err(|error| error.to_string())?;
        let name = icon_name(&icons_dir, svg)?;
        writeln!(
            list,
            "    SourceIcon {{ name: {name:?}, width: {ICON_SIZE}, height: {ICON_SIZE}, \
             rgba: include_bytes!(concat!(env!(\"OUT_DIR\"), \"/icon_{index}.rgba\")) }},"
        )
        .map_err(|error| error.to_string())?;
    }
    list.push_str("]\n");
    std::fs::write(out_dir.join("icons_generated.rs"), list).map_err(|error| error.to_string())
}

/// Рекурсивно собирает SVG и регистрирует каталоги и файлы для `rerun-if-changed`.
fn collect_svgs(dir: &Path, svgs: &mut Vec<PathBuf>) -> Result<(), String> {
    println!("cargo:rerun-if-changed={}", dir.display());
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Ok(());
    };
    let mut paths = entries
        .map(|entry| {
            entry
                .map(|entry| entry.path())
                .map_err(|error| error.to_string())
        })
        .collect::<Result<Vec<_>, _>>()?;
    paths.sort();
    for path in paths {
        println!("cargo:rerun-if-changed={}", path.display());
        if path.is_dir() {
            collect_svgs(&path, svgs)?;
        } else if path
            .extension()
            .is_some_and(|extension| extension.eq_ignore_ascii_case("svg"))
        {
            svgs.push(path);
        }
    }
    Ok(())
}

/// Имя иконки относительно `IconsPack` без расширения, с `/` как разделителем.
fn icon_name(icons_dir: &Path, svg: &Path) -> Result<String, String> {
    let relative = svg
        .strip_prefix(icons_dir)
        .map_err(|error| error.to_string())?
        .with_extension("");
    Ok(relative
        .components()
        .map(|component| component.as_os_str().to_string_lossy().into_owned())
        .collect::<Vec<_>>()
        .join("/"))
}

/// Вписывает SVG по центру квадрата и возвращает предумноженный RGBA8 (premultiplied).
fn render(svg: &Path) -> Result<Vec<u8>, String> {
    println!("cargo:rerun-if-changed={}", svg.display());
    let data = std::fs::read(svg).map_err(|error| format!("{}: {error}", svg.display()))?;
    let tree = usvg::Tree::from_data(&data, &usvg::Options::default())
        .map_err(|error| format!("{}: {error}", svg.display()))?;
    let size = tree.size();
    let size_px = ICON_SIZE as f32;
    let scale = (size_px / size.width()).min(size_px / size.height());
    let offset_x = (size_px - size.width() * scale) / 2.0;
    let offset_y = (size_px - size.height() * scale) / 2.0;
    let mut pixmap = tiny_skia::Pixmap::new(ICON_SIZE, ICON_SIZE)
        .ok_or_else(|| format!("{}: не удалось создать холст", svg.display()))?;
    let transform = tiny_skia::Transform::from_row(scale, 0.0, 0.0, scale, offset_x, offset_y);
    resvg::render(&tree, transform, &mut pixmap.as_mut());
    Ok(pixmap.take())
}
