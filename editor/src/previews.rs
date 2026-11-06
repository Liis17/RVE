//! Превью PNG для панели Project: декодирование в фоне и LRU-кэш UI-текстур.

use crate::console::ConsoleLog;
use engine_assets::{HdrImageData, TextureData, decode_hdr, decode_png};
use engine_files::VirtualPath;
use engine_render::{RenderError, Renderer, UiTextureId};
use engine_ui::render::UiBridge;
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::mpsc::{Receiver, Sender, channel};

const MAX_PREVIEWS: usize = 64;
const MAX_PREVIEW_SIDE: u32 = 128;

type DecodeResult = (String, Result<TextureData, String>);

struct Cached {
    texture: UiTextureId,
    id: egui::TextureId,
    last_used: u64,
}

/// Кэш загруженных превью и очередь фонового декодирования.
#[derive(Default)]
pub struct PreviewCache {
    worker: Option<Worker>,
    cached: HashMap<String, Cached>,
    sources: HashMap<String, PathBuf>,
    policy: PreviewPolicy,
    clock: u64,
    clear_requested: bool,
}

#[derive(Default)]
struct PreviewPolicy {
    pending: HashSet<String>,
    stale: HashSet<String>,
    failed: HashSet<String>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PreviewCompletion {
    Retry,
    Insert,
    Replace,
    KeepPrevious,
    Failed,
}

impl PreviewPolicy {
    fn can_request(&self, path: &str) -> bool {
        !self.pending.contains(path) && !self.failed.contains(path)
    }

    fn begin(&mut self, path: &str) {
        self.failed.remove(path);
        self.pending.insert(path.to_owned());
    }

    fn fail(&mut self, path: &str) {
        self.failed.insert(path.to_owned());
    }

    fn refresh(&mut self, path: &str) -> bool {
        if self.pending.contains(path) {
            self.stale.insert(path.to_owned());
            false
        } else {
            self.failed.remove(path);
            true
        }
    }

    fn finish(&mut self, path: &str, success: bool, has_cached: bool) -> PreviewCompletion {
        self.pending.remove(path);
        if self.stale.remove(path) {
            return PreviewCompletion::Retry;
        }
        if success {
            self.failed.remove(path);
            if has_cached {
                PreviewCompletion::Replace
            } else {
                PreviewCompletion::Insert
            }
        } else {
            self.failed.insert(path.to_owned());
            if has_cached {
                PreviewCompletion::KeepPrevious
            } else {
                PreviewCompletion::Failed
            }
        }
    }
}

struct Worker {
    requests: Sender<(String, PathBuf)>,
    results: Receiver<DecodeResult>,
}

impl Worker {
    fn start() -> std::io::Result<Self> {
        let (requests, request_rx) = channel::<(String, PathBuf)>();
        let (result_tx, results) = channel();
        // Поток завершается, когда кэш удаляет отправитель запросов.
        std::thread::Builder::new()
            .name("rve-editor-previews".to_owned())
            .spawn(move || {
                for (path, file) in request_rx {
                    let decoded = std::fs::read(&file)
                        .map_err(|error| error.to_string())
                        .and_then(|bytes| {
                            if file
                                .extension()
                                .and_then(|extension| extension.to_str())
                                .is_some_and(|extension| extension.eq_ignore_ascii_case("hdr"))
                            {
                                decode_hdr(&bytes).map(|data| hdr_preview(&data, MAX_PREVIEW_SIDE))
                            } else {
                                decode_png(&bytes)
                                    .map(|data| preview_image(&data, MAX_PREVIEW_SIDE))
                            }
                        });
                    if result_tx.send((path, decoded)).is_err() {
                        break;
                    }
                }
            })?;
        Ok(Self { requests, results })
    }
}

impl PreviewCache {
    /// Возвращает готовое превью или ставит файл в очередь декодирования.
    pub fn get(
        &mut self,
        path: &str,
        file: impl FnOnce() -> Option<PathBuf>,
    ) -> Option<egui::TextureId> {
        self.clock += 1;
        if let Some(cached) = self.cached.get_mut(path) {
            cached.last_used = self.clock;
            return Some(cached.id);
        }
        if !self.policy.can_request(path) {
            return None;
        }
        let Some(file) = file() else {
            self.policy.fail(path);
            return None;
        };
        self.sources.insert(path.to_owned(), file.clone());
        if self.worker.is_none() {
            self.worker = Worker::start().ok();
        }
        let requested = self
            .worker
            .as_ref()
            .is_some_and(|worker| worker.requests.send((path.to_owned(), file)).is_ok());
        if requested {
            self.policy.begin(path);
        } else {
            self.policy.fail(path);
        }
        None
    }

    /// Повторно декодирует известные превью, затронутые путями или каталогами.
    pub fn refresh_paths(&mut self, paths: &HashSet<VirtualPath>) {
        let affected = affected_paths(self.known_paths(), paths, false);
        for path in affected {
            self.refresh_path(&path);
        }
    }

    /// Повторно декодирует все известные превью после потери событий watcher.
    pub fn refresh_all(&mut self) {
        let affected = affected_paths(self.known_paths(), &HashSet::new(), true);
        for path in affected {
            self.refresh_path(&path);
        }
    }

    /// Полностью очищает записи при смене проекта.
    pub fn clear(&mut self) {
        self.clear_requested = true;
        self.worker = None;
        self.policy = PreviewPolicy::default();
        self.sources.clear();
    }

    /// Есть незавершённые декодирования: окно должно проснуться, чтобы показать результат.
    pub fn has_pending(&self) -> bool {
        !self.policy.pending.is_empty()
    }

    /// Загружает готовые изображения в GPU и освобождает лишние; вызывается до построения UI.
    pub fn upload(
        &mut self,
        renderer: &mut Renderer,
        bridge: &mut UiBridge,
        log: &mut ConsoleLog,
    ) -> Result<(), RenderError> {
        if std::mem::take(&mut self.clear_requested) {
            for (_, cached) in self.cached.drain() {
                bridge.unregister(cached.id);
                renderer.free_ui_texture(cached.texture)?;
            }
        }
        let finished = self
            .worker
            .as_ref()
            .map(|worker| worker.results.try_iter().collect::<Vec<_>>())
            .unwrap_or_default();
        for (path, decoded) in finished {
            let has_cached = self.cached.contains_key(&path);
            let action = self.policy.finish(&path, decoded.is_ok(), has_cached);
            if action == PreviewCompletion::Retry {
                if let Some(file) = self.sources.get(&path).cloned() {
                    self.request(&path, file);
                } else {
                    self.policy.fail(&path);
                }
                continue;
            }
            match decoded {
                Ok(data) => {
                    let texture =
                        renderer.create_ui_texture(data.width, data.height, &data.rgba)?;
                    let id = bridge.register_texture(texture);
                    self.clock += 1;
                    let previous = self.cached.insert(
                        path,
                        Cached {
                            texture,
                            id,
                            last_used: self.clock,
                        },
                    );
                    if let Some(previous) = previous {
                        bridge.unregister(previous.id);
                        renderer.free_ui_texture(previous.texture)?;
                    }
                }
                Err(error) => {
                    if action == PreviewCompletion::KeepPrevious {
                        log.error(format!("{path}: {error}; сохранена прежняя версия превью"));
                    } else {
                        log.error(format!("{path}: {error}"));
                    }
                }
            }
        }
        for path in eviction_order(
            self.cached
                .iter()
                .map(|(path, entry)| (path, entry.last_used)),
            MAX_PREVIEWS,
        ) {
            if let Some(cached) = self.cached.remove(&path) {
                bridge.unregister(cached.id);
                renderer.free_ui_texture(cached.texture)?;
            }
            if !self.policy.pending.contains(&path) && !self.policy.failed.contains(&path) {
                self.sources.remove(&path);
            }
        }
        Ok(())
    }

    fn known_paths(&self) -> HashSet<String> {
        self.cached
            .keys()
            .chain(self.policy.pending.iter())
            .chain(self.policy.failed.iter())
            .cloned()
            .collect()
    }

    fn refresh_path(&mut self, path: &str) {
        if !self.policy.refresh(path) {
            return;
        }
        if let Some(file) = self.sources.get(path).cloned() {
            self.request(path, file);
        }
    }

    fn request(&mut self, path: &str, file: PathBuf) {
        if !self.policy.can_request(path) {
            return;
        }
        self.sources.insert(path.to_owned(), file.clone());
        if self.worker.is_none() {
            self.worker = Worker::start().ok();
        }
        let requested = self
            .worker
            .as_ref()
            .is_some_and(|worker| worker.requests.send((path.to_owned(), file)).is_ok());
        if requested {
            self.policy.begin(path);
        } else {
            self.policy.fail(path);
        }
    }
}

fn path_is_parent(changed: &VirtualPath, key: &VirtualPath) -> bool {
    key.as_str() == changed.as_str()
        || key
            .as_str()
            .strip_prefix(changed.as_str())
            .is_some_and(|suffix| suffix.starts_with('/'))
}

fn affected_paths(
    candidates: HashSet<String>,
    changed: &HashSet<VirtualPath>,
    rescan: bool,
) -> Vec<String> {
    candidates
        .into_iter()
        .filter(|path| {
            if rescan {
                return true;
            }
            let Ok(key) = VirtualPath::parse(path) else {
                return false;
            };
            changed.iter().any(|changed| path_is_parent(changed, &key))
        })
        .collect()
}

/// Давно не использованные записи сверх лимита.
fn eviction_order<'a>(
    entries: impl Iterator<Item = (&'a String, u64)>,
    limit: usize,
) -> Vec<String> {
    let mut entries = entries.collect::<Vec<_>>();
    if entries.len() <= limit {
        return Vec::new();
    }
    entries.sort_by_key(|(_, last_used)| *last_used);
    let excess = entries.len() - limit;
    entries
        .into_iter()
        .take(excess)
        .map(|(path, _)| path.clone())
        .collect()
}

/// Уменьшает изображение до `max_side` по большей стороне и предумножает альфу для UI pass.
fn preview_image(source: &TextureData, max_side: u32) -> TextureData {
    let largest = source.width.max(source.height).max(1);
    let scale = (max_side as f32 / largest as f32).min(1.0);
    let width = ((source.width as f32 * scale).round() as u32).max(1);
    let height = ((source.height as f32 * scale).round() as u32).max(1);
    let mut rgba = Vec::with_capacity(width as usize * height as usize * 4);
    for y in 0..height {
        let source_y = (u64::from(y) * u64::from(source.height) / u64::from(height)) as usize;
        for x in 0..width {
            let source_x = (u64::from(x) * u64::from(source.width) / u64::from(width)) as usize;
            let offset = (source_y * source.width as usize + source_x) * 4;
            let [r, g, b, a] = [
                source.rgba[offset],
                source.rgba[offset + 1],
                source.rgba[offset + 2],
                source.rgba[offset + 3],
            ];
            let premultiply = |channel: u8| (u16::from(channel) * u16::from(a) / 255) as u8;
            rgba.extend_from_slice(&[premultiply(r), premultiply(g), premultiply(b), a]);
        }
    }
    TextureData {
        width,
        height,
        rgba,
    }
}

/// Тонально отображает линейный Radiance HDR в небольшой sRGB-подобный thumbnail.
fn hdr_preview(source: &HdrImageData, max_side: u32) -> TextureData {
    let largest = source.width.max(source.height).max(1);
    let scale = (max_side as f32 / largest as f32).min(1.0);
    let width = ((source.width as f32 * scale).round() as u32).max(1);
    let height = ((source.height as f32 * scale).round() as u32).max(1);
    let mut rgba = Vec::with_capacity(width as usize * height as usize * 4);
    for y in 0..height {
        let source_y = (u64::from(y) * u64::from(source.height) / u64::from(height)) as usize;
        for x in 0..width {
            let source_x = (u64::from(x) * u64::from(source.width) / u64::from(width)) as usize;
            let offset = (source_y * source.width as usize + source_x) * 3;
            for channel in &source.rgb[offset..offset + 3] {
                let linear = channel.max(0.0);
                let mapped = (linear / (1.0 + linear)).powf(1.0 / 2.2);
                rgba.push((mapped.clamp(0.0, 1.0) * 255.0).round() as u8);
            }
            rgba.push(255);
        }
    }
    TextureData {
        width,
        height,
        rgba,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn solid(width: u32, height: u32, color: [u8; 4]) -> TextureData {
        TextureData {
            width,
            height,
            rgba: color.repeat(width as usize * height as usize),
        }
    }

    #[test]
    fn large_images_shrink_keeping_the_aspect_ratio() {
        let preview = preview_image(&solid(400, 100, [1, 2, 3, 255]), 128);
        assert_eq!((preview.width, preview.height), (128, 32));
        assert_eq!(preview.rgba.len(), 128 * 32 * 4);
    }

    #[test]
    fn small_images_are_not_enlarged_and_extreme_ratios_keep_one_pixel() {
        let small = preview_image(&solid(4, 2, [0; 4]), 128);
        assert_eq!((small.width, small.height), (4, 2));
        let thin = preview_image(&solid(1000, 1, [0; 4]), 128);
        assert_eq!((thin.width, thin.height), (128, 1));
    }

    #[test]
    fn alpha_is_premultiplied() {
        let preview = preview_image(&solid(1, 1, [200, 100, 50, 128]), 128);
        assert_eq!(preview.rgba, [100, 50, 25, 128]);
    }

    #[test]
    fn eviction_removes_the_least_recently_used_over_the_limit() {
        let names = ["a".to_owned(), "b".to_owned(), "c".to_owned()];
        let entries = [(&names[0], 5), (&names[1], 1), (&names[2], 9)];
        assert_eq!(eviction_order(entries.into_iter(), 2), ["b"]);
        assert!(eviction_order(entries.into_iter(), 3).is_empty());
    }

    #[test]
    fn preview_refresh_keeps_a_working_entry_on_error_and_replaces_it_after_repair() {
        let mut policy = PreviewPolicy::default();
        policy.begin("preview.png");
        assert_eq!(
            policy.finish("preview.png", true, false),
            PreviewCompletion::Insert
        );

        assert!(policy.refresh("preview.png"));
        policy.begin("preview.png");
        assert_eq!(
            policy.finish("preview.png", false, true),
            PreviewCompletion::KeepPrevious
        );
        assert!(!policy.can_request("preview.png"));

        assert!(policy.refresh("preview.png"));
        policy.begin("preview.png");
        assert_eq!(
            policy.finish("preview.png", true, true),
            PreviewCompletion::Replace
        );
        assert!(policy.can_request("preview.png"));
    }

    #[test]
    fn preview_refresh_discards_pending_results_and_rescan_selects_every_known_path() {
        let mut policy = PreviewPolicy::default();
        policy.begin("a.png");
        assert!(!policy.refresh("a.png"));
        assert_eq!(
            policy.finish("a.png", true, false),
            PreviewCompletion::Retry
        );

        let candidates = HashSet::from(["a.png".to_owned(), "folder/b.hdr".to_owned()]);
        let all = affected_paths(candidates.clone(), &HashSet::new(), true);
        assert_eq!(all.len(), candidates.len());
        let changed = HashSet::from([VirtualPath::parse("folder").unwrap()]);
        assert_eq!(
            affected_paths(candidates, &changed, false),
            ["folder/b.hdr"]
        );
    }

    #[test]
    fn unavailable_files_are_not_requested_again() {
        let mut cache = PreviewCache::default();
        assert_eq!(cache.get("a.png", || None), None);
        assert!(!cache.has_pending());
        let mut asked = false;
        assert_eq!(
            cache.get("a.png", || {
                asked = true;
                None
            }),
            None
        );
        assert!(!asked);
    }

    #[test]
    fn hdr_previews_are_tone_mapped_and_bounded() {
        let preview = hdr_preview(
            &HdrImageData {
                width: 2,
                height: 1,
                rgb: vec![0.0, 1.0, 4.0, 1.0, 4.0, 16.0],
            },
            1,
        );
        assert_eq!((preview.width, preview.height), (1, 1));
        assert_eq!(preview.rgba.len(), 4);
        assert!(preview.rgba[1] > preview.rgba[0]);
        assert!(preview.rgba[2] > preview.rgba[1]);
        assert_eq!(preview.rgba[3], 255);
    }
}
