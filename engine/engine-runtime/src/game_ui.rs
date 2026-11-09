//! Игровой UI рантайма: `.rveui` и их ассеты на CPU, маскирование ввода, перевод кадра в ввод
//! `engine-gui` и GPU-ресурсы UI на render thread.
//!
//! Логика UI живёт в `engine-gui` и `engine-script`; здесь только соединение с окном, потоками и
//! renderer. Чистые функции (маскирование, сборка `UiInput`, решение об атласе) тестируются без GPU.

use crate::prefabs::PrefabToken;
use crate::scene::read_utf8;
use engine_assets::{Assets, TextureData};
use engine_files::VirtualPath;
use engine_gui::capture::{InputCapture, InputFrame, InputId};
use engine_gui::document::{AssetKind, UiDocument, asset_kind};
use engine_gui::input::{TextInputEvent, UiAction, UiClaims, UiInput};
use engine_gui::paint::{FontAtlas, UiDrawList, UiPaint, UiTexture};
use engine_gui::system::UiSystem;
use engine_render::{RenderError, Renderer, UiImage, UiPrimitive, UiTextureId, UiVertex};
use engine_resources::ResourceHandle;
use engine_script::{Key, MouseButton, ScriptHost, ScriptInput};
use epaint::Vec2;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

/// Число кодов клавиш: маска нажатий скриптов — `u128`.
const KEY_CODES: u32 = 128;
/// Коды кнопок мыши ABI.
const MOUSE_CODES: u32 = 5;
/// Кнопки геймпада ABI: биты 0..=16.
const PAD_BITS: u32 = 17;
/// Идентификаторы захвата: клавиши — код клавиши, мышь и пад — со смещением.
const MOUSE_BASE: InputId = 1_000;
const PAD_BASE: InputId = 2_000;
/// Порог отклонения стика, с которого он даёт направление навигации.
const STICK_THRESHOLD: f32 = 0.5;
/// Биты кнопок геймпада ABI (код кнопки = номер бита).
const PAD_SOUTH: u32 = 0;
const PAD_EAST: u32 = 1;
const PAD_DPAD_UP: u32 = 13;
const PAD_DPAD_DOWN: u32 = 14;
const PAD_DPAD_LEFT: u32 = 15;
const PAD_DPAD_RIGHT: u32 = 16;

/// Картинка UI в premultiplied sRGB RGBA8: цвет умножен на альфу при загрузке.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct UiImageData {
    pub size: [u32; 2],
    pub rgba: Vec<u8>,
}

/// `.rveui` и их ассеты, загруженные на CPU. Ключи — канонические пути без ведущего слеша.
#[derive(Clone, Debug, Default)]
pub(crate) struct PreparedUi {
    pub documents: BTreeMap<String, Arc<UiDocument>>,
    pub fonts: BTreeMap<String, Arc<Vec<u8>>>,
    pub images: BTreeMap<String, Arc<UiImageData>>,
}

/// Пустой снимок UI для кадра без канвасов.
pub(crate) fn empty_paint() -> UiPaint {
    UiPaint {
        draw_list: Arc::default(),
        atlas: Arc::default(),
    }
}

pub(crate) fn is_ui_document(path: &VirtualPath) -> bool {
    path.as_str().ends_with(".rveui")
}

/// Загружает документы из путей и все их ассеты. Ошибка разбора, отсутствующий или нечитаемый
/// ассет — ошибка загрузки сцены с путём. Шрифты и размеры картинок проверяются теми же правилами,
/// что и при регистрации в скриптах.
pub(crate) fn load_ui<'a>(
    resources: &ResourceHandle,
    assets: &Assets,
    paths: impl IntoIterator<Item = &'a VirtualPath>,
) -> Result<PreparedUi, String> {
    let mut ui = PreparedUi::default();
    let mut probe = UiSystem::default();
    for path in paths {
        if !is_ui_document(path) || ui.documents.contains_key(path.as_str()) {
            continue;
        }
        let label = format!("/{}", path.as_str());
        let text = read_utf8(resources, &label)?;
        let document =
            UiDocument::parse(label.clone(), &text).map_err(|error| format!("{label}: {error}"))?;
        for asset in document.assets() {
            let key = asset.path.as_str().to_owned();
            let asset_label = format!("/{key}");
            match asset_kind(&key) {
                Some(AssetKind::Image) => {
                    if ui.images.contains_key(&key) {
                        continue;
                    }
                    let texture = assets
                        .load_texture(&asset_label)
                        .map_err(|error| format!("{asset_label}: {error}"))?;
                    let image = premultiplied(texture);
                    probe
                        .register_image(&key, image.size)
                        .map_err(|error| format!("{asset_label}: {error}"))?;
                    ui.images.insert(key, Arc::new(image));
                }
                Some(AssetKind::Font) => {
                    if ui.fonts.contains_key(&key) {
                        continue;
                    }
                    let bytes = read_bytes(resources, &asset_label)?;
                    probe
                        .register_font(&key, bytes.clone())
                        .map_err(|error| format!("{asset_label}: {error}"))?;
                    ui.fonts.insert(key, Arc::new(bytes));
                }
                None => {
                    return Err(format!("{label}: неподдерживаемый ассет {key:?}"));
                }
            }
        }
        ui.documents
            .insert(path.as_str().to_owned(), Arc::new(document));
    }
    Ok(ui)
}

fn read_bytes(resources: &ResourceHandle, label: &str) -> Result<Vec<u8>, String> {
    resources
        .read_all(label)
        .map_err(|error| format!("{label}: {error}"))?
        .wait()
        .map_err(|error| format!("{label}: {error}"))
}

/// Умножает цвет на альфу в sRGB-значениях: так ожидает UI-текстура renderer.
fn premultiplied(texture: TextureData) -> UiImageData {
    let mut rgba = texture.rgba;
    for pixel in rgba.as_chunks_mut::<4>().0 {
        let alpha = u32::from(pixel[3]);
        for channel in &mut pixel[..3] {
            *channel = ((u32::from(*channel) * alpha + 127) / 255) as u8;
        }
    }
    UiImageData {
        size: [texture.width, texture.height],
        rgba,
    }
}

/// Что уже зарегистрировано в скриптах: повторная регистрация шрифта пересоздаёт все шрифты UI,
/// поэтому каждый ключ регистрируется один раз, сколько бы префабов его ни несло.
#[derive(Default)]
pub(crate) struct UiRegistry {
    documents: HashSet<String>,
    fonts: HashSet<String>,
    images: HashSet<String>,
}

impl UiRegistry {
    /// Регистрирует документы для `open` и канвасов компонентов, шрифты и размеры картинок.
    pub(crate) fn register(
        &mut self,
        scripts: &mut ScriptHost,
        ui: &PreparedUi,
    ) -> Result<(), String> {
        for (path, document) in &ui.documents {
            if self.documents.insert(path.clone()) {
                let path = VirtualPath::parse(path)
                    .map_err(|error| format!("UI-документ {path}: {error}"))?;
                scripts.register_ui_document(path, Arc::clone(document));
            }
        }
        let mut system = scripts.ui_mut();
        for (path, bytes) in &ui.fonts {
            if self.fonts.insert(path.clone()) {
                system
                    .register_font(path, bytes.to_vec())
                    .map_err(|error| error.to_string())?;
            }
        }
        for (path, image) in &ui.images {
            if self.images.insert(path.clone()) {
                system
                    .register_image(path, image.size)
                    .map_err(|error| error.to_string())?;
            }
        }
        Ok(())
    }
}

/// Идентификатор клавиши для захвата: её код.
fn key_id(key: Key) -> InputId {
    key as u32
}

fn mouse_id(button: MouseButton) -> InputId {
    MOUSE_BASE + button as u32
}

fn pad_id(bit: u32) -> InputId {
    PAD_BASE + bit
}

/// Первый подключённый геймпад (индекс слота с нуля): навигация UI берёт только его.
fn ui_pad(input: &ScriptInput) -> Option<usize> {
    (0..4).find(|&index| {
        input
            .gamepad_state(index as u32 + 1)
            .is_some_and(|state| state.connected != 0)
    })
}

/// Состояние кадра в идентификаторах захвата: только нажатия, удержания и отпускания.
fn input_frame(input: &ScriptInput, pad: Option<usize>) -> InputFrame {
    let mut frame = InputFrame::default();
    for code in 0..KEY_CODES {
        let Some(key) = Key::from_code(code) else {
            continue;
        };
        if input.key_pressed(key) {
            frame.pressed.insert(key_id(key));
        }
        if input.key_down(key) {
            frame.down.insert(key_id(key));
        }
        if input.key_released(key) {
            frame.released.insert(key_id(key));
        }
    }
    for code in 0..MOUSE_CODES {
        let Some(button) = MouseButton::from_code(code) else {
            continue;
        };
        if input.mouse_pressed(button) {
            frame.pressed.insert(mouse_id(button));
        }
        if input.mouse_down(button) {
            frame.down.insert(mouse_id(button));
        }
        if input.mouse_released(button) {
            frame.released.insert(mouse_id(button));
        }
    }
    if let Some(state) = pad.and_then(|index| input.gamepad_state(index as u32 + 1)) {
        for bit in 0..PAD_BITS {
            let mask = 1 << bit;
            if state.pressed & mask != 0 {
                frame.pressed.insert(pad_id(bit));
            }
            if state.down & mask != 0 {
                frame.down.insert(pad_id(bit));
            }
            if state.released & mask != 0 {
                frame.released.insert(pad_id(bit));
            }
        }
    }
    frame
}

/// Клавиши, которыми действие UI можно выполнить с клавиатуры.
fn action_keys(action: UiAction) -> &'static [Key] {
    match action {
        UiAction::Up => &[Key::ArrowUp],
        UiAction::Down => &[Key::ArrowDown],
        UiAction::Left => &[Key::ArrowLeft],
        UiAction::Right => &[Key::ArrowRight],
        UiAction::Confirm => &[Key::Enter],
        UiAction::Cancel => &[Key::Escape],
        UiAction::Next | UiAction::Previous => &[Key::Tab],
    }
}

/// Кнопки геймпада, которыми действие UI выполняется с пада.
fn action_pad_bits(action: UiAction) -> &'static [u32] {
    match action {
        UiAction::Up => &[PAD_DPAD_UP],
        UiAction::Down => &[PAD_DPAD_DOWN],
        UiAction::Left => &[PAD_DPAD_LEFT],
        UiAction::Right => &[PAD_DPAD_RIGHT],
        UiAction::Confirm => &[PAD_SOUTH],
        UiAction::Cancel => &[PAD_EAST],
        UiAction::Next | UiAction::Previous => &[],
    }
}

/// Идентификаторы, которые UI поглотил в этом кадре: нажатие мыши, навигационные клавиши и кнопки
/// пада, а при фокусе InputField — все новые нажатия клавиатуры.
fn claimed_ids(claims: &UiClaims, input: &ScriptInput) -> BTreeSet<InputId> {
    let mut claimed = BTreeSet::new();
    if claims.pointer_primary {
        claimed.insert(mouse_id(MouseButton::Left));
    }
    for &action in &claims.actions {
        for &key in action_keys(action) {
            claimed.insert(key_id(key));
        }
        for &bit in action_pad_bits(action) {
            claimed.insert(pad_id(bit));
        }
    }
    if claims.text_focused {
        for code in 0..KEY_CODES {
            if let Some(key) = Key::from_code(code)
                && input.key_pressed(key)
            {
                claimed.insert(key_id(key));
            }
        }
    }
    claimed
}

/// Маскирует кадр для скриптов: захваченный UI ввод скрыт в нажатии, удержании и отпускании.
/// `capture` хранит захват между кадрами, поэтому удержанное нажатие остаётся скрытым до отпускания.
pub(crate) fn mask_frame(
    capture: &mut InputCapture,
    input: &ScriptInput,
    claims: &UiClaims,
    pad: Option<usize>,
) -> ScriptInput {
    let raw = input_frame(input, pad);
    let claimed = claimed_ids(claims, input);
    let visible = capture.apply(&raw, &claimed, claims.wheel);
    let visible_ids: BTreeSet<InputId> = visible
        .pressed
        .iter()
        .chain(&visible.down)
        .chain(&visible.released)
        .copied()
        .collect();
    let mut masked = *input;
    let mut pad_mask = 0_u32;
    let hidden = raw
        .pressed
        .iter()
        .chain(&raw.down)
        .chain(&raw.released)
        .copied()
        .filter(|id| !visible_ids.contains(id))
        .collect::<BTreeSet<InputId>>();
    for id in hidden {
        if id >= PAD_BASE {
            pad_mask |= 1 << (id - PAD_BASE);
        } else if id >= MOUSE_BASE {
            if let Some(button) = MouseButton::from_code(id - MOUSE_BASE) {
                masked.hide_mouse_button(button);
            }
        } else if let Some(key) = Key::from_code(id) {
            masked.hide_key(key);
        }
    }
    if let Some(index) = pad {
        masked.hide_pad_buttons(index, pad_mask);
    }
    if claims.wheel {
        masked.hide_wheel();
    }
    masked
}

/// Направление левого стика, если он отклонён дальше порога.
fn stick_action(axes: [f32; 6]) -> Option<UiAction> {
    let (x, y) = (axes[0], axes[1]);
    if x.abs() < STICK_THRESHOLD && y.abs() < STICK_THRESHOLD {
        return None;
    }
    Some(if x.abs() >= y.abs() {
        if x > 0.0 {
            UiAction::Right
        } else {
            UiAction::Left
        }
    } else if y > 0.0 {
        UiAction::Up
    } else {
        UiAction::Down
    })
}

/// Действие UI по нажатой клавише; Shift + Tab — предыдущий элемент.
fn key_action(key: Key, shift: bool) -> Option<UiAction> {
    Some(match key {
        Key::ArrowUp => UiAction::Up,
        Key::ArrowDown => UiAction::Down,
        Key::ArrowLeft => UiAction::Left,
        Key::ArrowRight => UiAction::Right,
        Key::Enter => UiAction::Confirm,
        Key::Escape => UiAction::Cancel,
        Key::Tab if shift => UiAction::Previous,
        Key::Tab => UiAction::Next,
        _ => return None,
    })
}

/// Направления, которые можно удерживать для повтора: стрелки и крестовина пада.
const HELD_KEYS: [(Key, UiAction); 4] = [
    (Key::ArrowUp, UiAction::Up),
    (Key::ArrowDown, UiAction::Down),
    (Key::ArrowLeft, UiAction::Left),
    (Key::ArrowRight, UiAction::Right),
];

/// Ввод UI на scripts thread: захват между кадрами, состояние стика и флаг текстового ввода.
pub(crate) struct UiInputBridge {
    capture: InputCapture,
    /// UI получал ввод в прошлом тике; при переходе к закрытому вводу взаимодействие сбрасывается.
    active: bool,
    /// Направление стика, которое уже дало навигацию: новое отклонение даёт действие один раз.
    stick: Option<UiAction>,
    /// Фокус InputField для главного потока: по нему включается IME.
    text_focus: Arc<AtomicBool>,
}

impl UiInputBridge {
    pub(crate) fn new(text_focus: Arc<AtomicBool>) -> Self {
        Self {
            capture: InputCapture::default(),
            active: false,
            stick: None,
            text_focus,
        }
    }

    /// Сбрасывает hover, нажатие и фокус UI при уходе в фон или потере ввода. Уже поглощённые
    /// нажатия остаются скрытыми до отпускания: их решает захват.
    pub(crate) fn suspend(&mut self, scripts: &mut ScriptHost) {
        if !self.active {
            return;
        }
        self.active = false;
        self.stick = None;
        self.text_focus.store(false, Ordering::Release);
        let events = scripts.ui_mut().reset_interaction();
        scripts.queue_ui_events(events);
    }

    /// Обрабатывает ввод тика: обновляет UI, ставит его события в очередь скриптов и возвращает
    /// кадр, который увидят скрипты. `accepting` — окно в фокусе, консоль закрыта и не фон.
    pub(crate) fn tick(
        &mut self,
        scripts: &mut ScriptHost,
        input: ScriptInput,
        accepting: bool,
        text: Vec<TextInputEvent>,
        screen_px: [u32; 2],
        now: f64,
    ) -> ScriptInput {
        if !accepting {
            self.suspend(scripts);
        }
        self.active = accepting;
        let pad = ui_pad(&input);
        let ui_input = if accepting {
            self.ui_input(&input, pad, text)
        } else {
            UiInput::default()
        };
        let update = scripts.ui_mut().update(
            &ui_input,
            now,
            Vec2::new(screen_px[0] as f32, screen_px[1] as f32),
        );
        scripts.queue_ui_events(update.events);
        self.text_focus
            .store(accepting && update.claims.text_focused, Ordering::Release);
        mask_frame(&mut self.capture, &input, &update.claims, pad)
    }

    /// Ввод `engine-gui` из кадра скриптов: указатель, колесо, нажатия навигации и удержание.
    fn ui_input(
        &mut self,
        input: &ScriptInput,
        pad: Option<usize>,
        text: Vec<TextInputEvent>,
    ) -> UiInput {
        let cursor = input.cursor();
        let wheel = input.wheel();
        let mut actions = Vec::new();
        let shift = input.key_down(Key::ShiftLeft) || input.key_down(Key::ShiftRight);
        for code in 0..KEY_CODES {
            let Some(key) = Key::from_code(code) else {
                continue;
            };
            if input.key_pressed(key)
                && let Some(action) = key_action(key, shift)
            {
                actions.push(action);
            }
        }
        let mut held = HELD_KEYS
            .iter()
            .find(|(key, _)| input.key_down(*key))
            .map(|(_, action)| *action);
        if let Some(state) = pad.and_then(|index| input.gamepad_state(index as u32 + 1)) {
            for (bit, action) in [
                (PAD_DPAD_UP, UiAction::Up),
                (PAD_DPAD_DOWN, UiAction::Down),
                (PAD_DPAD_LEFT, UiAction::Left),
                (PAD_DPAD_RIGHT, UiAction::Right),
            ] {
                if state.pressed & (1 << bit) != 0 {
                    actions.push(action);
                }
                if held.is_none() && state.down & (1 << bit) != 0 {
                    held = Some(action);
                }
            }
            if state.pressed & (1 << PAD_SOUTH) != 0 {
                actions.push(UiAction::Confirm);
            }
            if state.pressed & (1 << PAD_EAST) != 0 {
                actions.push(UiAction::Cancel);
            }
        }
        let axes = pad
            .and_then(|index| input.gamepad_state(index as u32 + 1))
            .map(|state| state.axes)
            .unwrap_or_default();
        let stick = stick_action(axes);
        if stick != self.stick
            && let Some(action) = stick
        {
            actions.push(action);
        }
        self.stick = stick;
        if held.is_none() {
            held = stick;
        }
        UiInput {
            pointer: Some(Vec2::new(cursor[0], cursor[1])),
            primary_pressed: input.mouse_pressed(MouseButton::Left),
            primary_down: input.mouse_down(MouseButton::Left),
            primary_released: input.mouse_released(MouseButton::Left),
            wheel: Vec2::new(wheel[0], wheel[1]),
            actions,
            held,
            text,
        }
    }
}

/// Решает, нужна ли загрузка атласа: только когда версия отличается от последней загруженной.
pub(crate) fn atlas_upload_needed(loaded: Option<u64>, version: u64) -> bool {
    loaded != Some(version)
}

/// Текстура батча: атлас шрифтов или картинка по пути. `None` — картинки нет среди загруженных.
fn batch_texture(
    texture: &UiTexture,
    atlas: Option<UiTextureId>,
    images: &HashMap<String, (UiTextureId, usize)>,
) -> Option<UiTextureId> {
    match texture {
        UiTexture::FontAtlas => atlas,
        UiTexture::Image(path) => images.get(path).map(|(id, _)| *id),
    }
}

/// GPU-ресурсы игрового UI на render thread: картинки по пути с числом ссылок (сцена и префабы),
/// атлас шрифтов по версии и буфер вершин, пересобираемый только при новом draw list.
#[derive(Default)]
pub(crate) struct GameUi {
    /// Загруженная версия атласа и её текстура; `None` — атлас пустой.
    atlas: Option<(u64, Option<UiTextureId>)>,
    images: HashMap<String, (UiTextureId, usize)>,
    prefab_images: HashMap<PrefabToken, Vec<String>>,
    vertices: Vec<UiVertex>,
    converted: Option<Arc<UiDrawList>>,
    missing_warned: HashSet<String>,
}

impl GameUi {
    /// Картинки сцены, созданные при загрузке: каждая имеет одну ссылку сцены.
    pub(crate) fn with_scene_images(images: HashMap<String, UiTextureId>) -> Self {
        Self {
            images: images
                .into_iter()
                .map(|(path, id)| (path, (id, 1)))
                .collect(),
            ..Self::default()
        }
    }

    /// Загружает атлас целиком, если его версия новее загруженной; старая текстура освобождается
    /// после замены. Снимок с выброшенным атласом не портит состояние: следующий снимок несёт
    /// новую версию и снова сравнивается с загруженной.
    pub(crate) fn sync_atlas(
        &mut self,
        renderer: &mut Renderer,
        atlas: &FontAtlas,
    ) -> Result<(), RenderError> {
        let loaded = self.atlas.map(|(version, _)| version);
        if !atlas_upload_needed(loaded, atlas.version) {
            return Ok(());
        }
        let texture = if atlas.size[0] > 0 && atlas.size[1] > 0 && !atlas.pixels.is_empty() {
            let rgba: Vec<u8> = atlas.pixels.iter().flatten().copied().collect();
            Some(renderer.create_ui_texture(atlas.size[0] as u32, atlas.size[1] as u32, &rgba)?)
        } else {
            None
        };
        if let Some((_, Some(old))) = self.atlas.replace((atlas.version, texture)) {
            free_texture(renderer, old);
        }
        Ok(())
    }

    /// Добавляет картинку по пути или увеличивает число ссылок уже загруженной.
    fn add_image(
        &mut self,
        renderer: &mut Renderer,
        path: &str,
        image: &UiImageData,
    ) -> Result<(), RenderError> {
        if let Some((_, users)) = self.images.get_mut(path) {
            *users += 1;
            return Ok(());
        }
        let id = renderer.create_ui_texture(image.size[0], image.size[1], &image.rgba)?;
        self.images.insert(path.to_owned(), (id, 1));
        Ok(())
    }

    fn release_image(&mut self, renderer: &mut Renderer, path: &str) {
        let Some((_, users)) = self.images.get_mut(path) else {
            return;
        };
        *users -= 1;
        if *users == 0
            && let Some((id, _)) = self.images.remove(path)
        {
            free_texture(renderer, id);
        }
    }

    /// Загружает картинки префаба при отправке на GPU. Ошибка откатывает уже добавленные.
    pub(crate) fn attach_prefab(
        &mut self,
        renderer: &mut Renderer,
        token: &PrefabToken,
        images: &BTreeMap<String, Arc<UiImageData>>,
    ) -> Result<(), String> {
        let mut attached: Vec<String> = Vec::with_capacity(images.len());
        for (path, image) in images {
            if let Err(error) = self.add_image(renderer, path, image) {
                for path in &attached {
                    self.release_image(renderer, path);
                }
                return Err(error.to_string());
            }
            attached.push(path.clone());
        }
        self.prefab_images.insert(token.clone(), attached);
        Ok(())
    }

    /// Освобождает картинки префаба; вызывается, когда токен не нужен или загрузка провалилась.
    pub(crate) fn detach_prefab(&mut self, renderer: &mut Renderer, token: &PrefabToken) {
        for path in self.prefab_images.remove(token).unwrap_or_default() {
            self.release_image(renderer, &path);
        }
    }

    /// Освобождает все UI-текстуры сцены и префабов; вызывается до уничтожения renderer.
    pub(crate) fn release_all(&mut self, renderer: &mut Renderer) {
        if let Some((_, Some(atlas))) = self.atlas.take() {
            free_texture(renderer, atlas);
        }
        for (_, (id, _)) in self.images.drain() {
            free_texture(renderer, id);
        }
        self.prefab_images.clear();
        self.vertices.clear();
        self.converted = None;
    }

    /// Переводит вершины draw list в формат renderer, только если список новый.
    fn convert(&mut self, list: &Arc<UiDrawList>) {
        if self
            .converted
            .as_ref()
            .is_some_and(|converted| Arc::ptr_eq(converted, list))
        {
            return;
        }
        self.vertices.clear();
        self.vertices
            .extend(list.vertices.iter().map(|vertex| UiVertex {
                position: vertex.pos,
                uv: vertex.uv,
                color: vertex.color,
            }));
        self.converted = Some(Arc::clone(list));
    }

    /// Примитивы кадра: батч draw list с текстурой renderer и отсечением. Батч с картинкой, которой
    /// нет среди загруженных (например, префаб уже освобождён), пропускается; предупреждение пишется
    /// один раз на путь.
    pub(crate) fn primitives<'a>(&'a mut self, paint: &'a UiPaint) -> Vec<UiPrimitive<'a>> {
        self.convert(&paint.draw_list);
        let atlas = self.atlas.and_then(|(_, texture)| texture);
        let mut primitives = Vec::with_capacity(paint.draw_list.batches.len());
        for batch in &paint.draw_list.batches {
            let texture = batch_texture(&batch.texture, atlas, &self.images);
            let Some(texture) = texture else {
                if let UiTexture::Image(path) = &batch.texture
                    && self.missing_warned.insert(path.clone())
                {
                    log::warn!("UI-картинка {path:?} не загружена: батч пропущен");
                }
                continue;
            };
            let range = batch.indices.start as usize..batch.indices.end as usize;
            primitives.push(UiPrimitive {
                clip_rect: batch.clip_rect,
                image: UiImage::Texture(texture),
                vertices: &self.vertices,
                indices: &paint.draw_list.indices[range],
            });
        }
        primitives
    }
}

fn free_texture(renderer: &mut Renderer, texture: UiTextureId) {
    if let Err(error) = renderer.free_ui_texture(texture) {
        log::error!("UI-текстура не освобождена: {error}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use engine_gui::paint::{UiBatch, UiVertex as GuiVertex};

    /// Кадр с новым нажатием клавиши.
    fn key_pressed_frame(key: Key) -> ScriptInput {
        let mut input = ScriptInput::default();
        input.set_key(key, true);
        input
    }

    /// Клавиша удерживается с прошлого кадра: без фронтов нажатия и отпускания.
    fn key_held(key: Key) -> ScriptInput {
        let mut input = ScriptInput::default();
        input.set_key(key, true);
        let _ = input.take_frame();
        input
    }

    /// Клавиша отпущена в этом кадре.
    fn key_released_frame(key: Key) -> ScriptInput {
        let mut input = key_held(key);
        input.set_key(key, false);
        input
    }

    fn mouse_pressed_frame() -> ScriptInput {
        let mut input = ScriptInput::default();
        input.set_mouse_button(MouseButton::Left, true);
        input
    }

    fn mouse_held() -> ScriptInput {
        let mut input = ScriptInput::default();
        input.set_mouse_button(MouseButton::Left, true);
        let _ = input.take_frame();
        input
    }

    fn mouse_released_frame() -> ScriptInput {
        let mut input = mouse_held();
        input.set_mouse_button(MouseButton::Left, false);
        input
    }

    fn claim_primary() -> UiClaims {
        UiClaims {
            pointer_primary: true,
            ..UiClaims::default()
        }
    }

    #[test]
    fn press_claimed_by_ui_stays_hidden_through_hold_and_release() {
        let mut capture = InputCapture::default();
        let press = mask_frame(&mut capture, &mouse_pressed_frame(), &claim_primary(), None);
        assert!(!press.mouse_pressed(MouseButton::Left));
        let held = mask_frame(&mut capture, &mouse_held(), &UiClaims::default(), None);
        assert!(
            !held.mouse_down(MouseButton::Left),
            "удержание поглощённого нажатия скрыто"
        );
        let released = mask_frame(
            &mut capture,
            &mouse_released_frame(),
            &UiClaims::default(),
            None,
        );
        assert!(
            !released.mouse_released(MouseButton::Left),
            "отпускание поглощённого нажатия скрыто"
        );
    }

    #[test]
    fn game_press_and_release_stay_visible_when_ui_claims_during_hold() {
        let mut capture = InputCapture::default();
        let press = mask_frame(
            &mut capture,
            &mouse_pressed_frame(),
            &UiClaims::default(),
            None,
        );
        assert!(press.mouse_pressed(MouseButton::Left));

        // InputField открылся во время удержания: новых нажатий клавиатуры он не получает, а
        // уже начатое в игре нажатие мыши остаётся видимым.
        let input_field = UiClaims {
            text_focused: true,
            ..UiClaims::default()
        };
        let held = mask_frame(&mut capture, &mouse_held(), &input_field, None);
        assert!(held.mouse_down(MouseButton::Left));
        let released = mask_frame(
            &mut capture,
            &mouse_released_frame(),
            &UiClaims::default(),
            None,
        );
        assert!(released.mouse_released(MouseButton::Left));
    }

    #[test]
    fn wheel_over_scroll_view_is_not_seen_by_scripts() {
        let mut capture = InputCapture::default();
        let mut input = ScriptInput::default();
        input.add_wheel(0.0, 3.0);
        let claims = UiClaims {
            wheel: true,
            ..UiClaims::default()
        };
        let masked = mask_frame(&mut capture, &input, &claims, None);
        assert_eq!(masked.wheel(), [0.0; 2]);
        let free = mask_frame(&mut capture, &input, &UiClaims::default(), None);
        assert_eq!(free.wheel(), [0.0, 3.0]);
    }

    #[test]
    fn focused_input_field_hides_new_key_presses_and_their_release() {
        let mut capture = InputCapture::default();
        let claims = UiClaims {
            text_focused: true,
            ..UiClaims::default()
        };
        let press = mask_frame(&mut capture, &key_pressed_frame(Key::A), &claims, None);
        assert!(!press.key_pressed(Key::A));
        let held = mask_frame(&mut capture, &key_held(Key::A), &UiClaims::default(), None);
        assert!(!held.key_down(Key::A));
        let release = mask_frame(
            &mut capture,
            &key_released_frame(Key::A),
            &UiClaims::default(),
            None,
        );
        assert!(!release.key_released(Key::A));
    }

    #[test]
    fn navigation_key_consumed_by_ui_is_hidden_from_game() {
        let mut capture = InputCapture::default();
        let claims = UiClaims {
            actions: vec![UiAction::Up],
            ..UiClaims::default()
        };
        let masked = mask_frame(
            &mut capture,
            &key_pressed_frame(Key::ArrowUp),
            &claims,
            None,
        );
        assert!(!masked.key_pressed(Key::ArrowUp));
        let other = mask_frame(
            &mut capture,
            &key_pressed_frame(Key::D),
            &UiClaims::default(),
            None,
        );
        assert!(other.key_pressed(Key::D));
    }

    #[test]
    fn pad_buttons_consumed_by_ui_are_hidden_on_the_ui_pad_only() {
        let mut capture = InputCapture::default();
        let mut input = ScriptInput::default();
        input.set_pad(0, true, 1 << PAD_DPAD_DOWN, [0.0; 6]);
        let claims = UiClaims {
            actions: vec![UiAction::Down],
            ..UiClaims::default()
        };
        let masked = mask_frame(&mut capture, &input, &claims, Some(0));
        assert_eq!(masked.gamepad_state(1).unwrap().pressed, 0);
        let free = mask_frame(&mut capture, &input, &UiClaims::default(), Some(0));
        assert_eq!(free.gamepad_state(1).unwrap().pressed, 1 << PAD_DPAD_DOWN);
    }

    #[test]
    fn ui_input_collects_pressed_navigation_text_and_held_direction() {
        let mut bridge = UiInputBridge::new(Arc::new(AtomicBool::new(false)));
        let mut input = ScriptInput::default();
        input.set_key(Key::ArrowDown, true);
        input.set_mouse_button(MouseButton::Left, true);
        input.set_cursor(12.0, 34.0);
        let text = vec![TextInputEvent::Text("a".to_owned())];
        let ui = bridge.ui_input(&input, None, text.clone());
        assert_eq!(ui.actions, vec![UiAction::Down]);
        assert_eq!(ui.held, Some(UiAction::Down));
        assert!(ui.primary_pressed);
        assert_eq!(ui.pointer, Some(Vec2::new(12.0, 34.0)));
        assert_eq!(ui.text, text);
    }

    #[test]
    fn stick_gives_one_action_per_new_direction() {
        let mut bridge = UiInputBridge::new(Arc::new(AtomicBool::new(false)));
        let mut input = ScriptInput::default();
        input.set_pad(0, true, 0, [0.9, 0.0, 0.0, 0.0, 0.0, 0.0]);
        let first = bridge.ui_input(&input, Some(0), Vec::new());
        assert_eq!(first.actions, vec![UiAction::Right]);
        assert_eq!(first.held, Some(UiAction::Right));
        let second = bridge.ui_input(&input, Some(0), Vec::new());
        assert!(
            second.actions.is_empty(),
            "удержанный стик даёт повтор, а не новое действие"
        );
        assert_eq!(second.held, Some(UiAction::Right));
    }

    #[test]
    fn shift_tab_moves_to_previous_element() {
        let mut bridge = UiInputBridge::new(Arc::new(AtomicBool::new(false)));
        let mut input = ScriptInput::default();
        input.set_key(Key::ShiftLeft, true);
        input.set_key(Key::Tab, true);
        let ui = bridge.ui_input(&input, None, Vec::new());
        assert_eq!(ui.actions, vec![UiAction::Previous]);
    }

    #[test]
    fn atlas_is_uploaded_only_when_its_version_changes() {
        assert!(atlas_upload_needed(None, 0), "первый снимок грузит атлас");
        assert!(
            !atlas_upload_needed(Some(3), 3),
            "неизменная версия без загрузки"
        );
        assert!(
            atlas_upload_needed(Some(3), 4),
            "новая версия заменяет атлас"
        );
        // Выброшенный снимок с версией 4 не меняет загруженную: следующий снимок с версией 5
        // снова решает по сравнению с загруженной версией.
        assert!(atlas_upload_needed(Some(3), 5));
    }

    #[test]
    fn unknown_image_batch_is_skipped_and_warned_once() {
        let mut ui = GameUi::default();
        let paint = UiPaint {
            draw_list: Arc::new(UiDrawList {
                vertices: vec![GuiVertex {
                    pos: [0.0; 2],
                    uv: [0.0; 2],
                    color: [255; 4],
                }],
                indices: vec![0, 0, 0],
                batches: vec![UiBatch {
                    texture: UiTexture::Image("ui/gone.png".to_owned()),
                    clip_rect: [0.0; 4],
                    indices: 0..3,
                }],
            }),
            atlas: Arc::default(),
        };
        assert!(ui.primitives(&paint).is_empty());
        assert!(ui.primitives(&paint).is_empty());
        assert_eq!(ui.missing_warned.len(), 1);
    }

    #[test]
    fn same_draw_list_is_not_reconverted() {
        let mut ui = GameUi::default();
        let list = Arc::new(UiDrawList {
            vertices: vec![GuiVertex {
                pos: [1.0, 2.0],
                uv: [0.0; 2],
                color: [255; 4],
            }],
            indices: Vec::new(),
            batches: Vec::new(),
        });
        ui.convert(&list);
        assert_eq!(ui.vertices.len(), 1);
        ui.vertices.clear();
        ui.convert(&list);
        assert!(ui.vertices.is_empty(), "тот же Arc не пересчитывается");
        ui.convert(&Arc::new((*list).clone()));
        assert_eq!(ui.vertices.len(), 1, "новый список пересчитывается");
    }

    #[test]
    fn pad_bit_constants_match_abi_buttons() {
        use rve_sdk::GamepadButton;
        assert_eq!(PAD_SOUTH, GamepadButton::South as u32);
        assert_eq!(PAD_EAST, GamepadButton::East as u32);
        assert_eq!(PAD_DPAD_UP, GamepadButton::DPadUp as u32);
        assert_eq!(PAD_DPAD_DOWN, GamepadButton::DPadDown as u32);
        assert_eq!(PAD_DPAD_LEFT, GamepadButton::DPadLeft as u32);
        assert_eq!(PAD_DPAD_RIGHT, GamepadButton::DPadRight as u32);
    }

    #[test]
    fn premultiplication_scales_color_by_alpha() {
        let image = premultiplied(TextureData {
            width: 2,
            height: 1,
            rgba: vec![200, 100, 0, 255, 200, 100, 0, 0],
        });
        assert_eq!(&image.rgba[..4], &[200, 100, 0, 255]);
        assert_eq!(&image.rgba[4..], &[0, 0, 0, 0]);
    }
}
