//! Документы `.rveui`: проверенная TOML-схема экранного UI, независимая от renderer и окна.

use engine_files::VirtualPath;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::collections::HashMap;
use std::error::Error;
use std::fmt;

/// Ошибка разбора или проверки `.rveui` с меткой источника и необязательным id элемента.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UiError {
    pub source: String,
    pub element: Option<String>,
    pub message: String,
}

impl UiError {
    fn new(source: &str, element: Option<&str>, message: impl Into<String>) -> Self {
        Self {
            source: source.to_owned(),
            element: element.map(str::to_owned),
            message: message.into(),
        }
    }
}

impl fmt::Display for UiError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.element {
            Some(id) => write!(f, "{}: элемент {id:?}: {}", self.source, self.message),
            None => write!(f, "{}: {}", self.source, self.message),
        }
    }
}

impl Error for UiError {}

/// Канонический путь ассета: в TOML записывается с ведущим слешем, хранится без него.
#[derive(Clone, Debug, Eq, PartialEq, Hash)]
pub struct UiPath(String);

impl UiPath {
    /// Разбирает путь с необязательным ведущим слешем; ошибка — текст причины.
    pub fn parse(input: &str) -> Result<Self, String> {
        VirtualPath::parse(input)
            .map(|path| Self(path.as_str().to_owned()))
            .map_err(|error| error.to_string())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl Serialize for UiPath {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&format!("/{}", self.0))
    }
}

impl<'de> Deserialize<'de> for UiPath {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let input = String::deserialize(deserializer)?;
        Self::parse(&input).map_err(<D::Error as serde::de::Error>::custom)
    }
}

/// Вид ассета, определяемый расширением: картинка `.png` или шрифт `.ttf`/`.otf`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AssetKind {
    Image,
    Font,
}

/// Вид ассета по пути; `None` для неподдерживаемого расширения.
pub fn asset_kind(path: &str) -> Option<AssetKind> {
    let (_, extension) = path.rsplit_once('.')?;
    match extension.to_ascii_lowercase().as_str() {
        "png" => Some(AssetKind::Image),
        "ttf" | "otf" => Some(AssetKind::Font),
        _ => None,
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ScaleMode {
    /// Масштаб фиксирован: `scale` физических пикселей на единицу канваса.
    #[default]
    Constant,
    /// Масштаб зависит от размера экрана относительно `reference_resolution`.
    Screen,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct CanvasSettings {
    pub layer: i32,
    pub scale_mode: ScaleMode,
    pub scale: f32,
    pub reference_resolution: [f32; 2],
    /// Для `screen`: 0 — подгонка по ширине, 1 — по высоте, между ними — логарифмическое среднее.
    #[serde(rename = "match")]
    pub match_factor: f32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub default_focus: Option<String>,
}

impl Default for CanvasSettings {
    fn default() -> Self {
        Self {
            layer: 0,
            scale_mode: ScaleMode::Constant,
            scale: 1.0,
            reference_resolution: [1920.0, 1080.0],
            match_factor: 0.5,
            default_focus: None,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UiAsset {
    pub path: UiPath,
}

/// Прямоугольник элемента относительно родителя (RectTransform-семантика, Y вниз).
///
/// На растянутой оси (`anchor_min != anchor_max`) `offset_min`/`offset_max` — отступы
/// от левого/верхнего и правого/нижнего якорей. На точечной оси `offset_min` — позиция
/// pivot относительно якоря, а `offset_max` — размер ≥ 0; pivot задаёт, какая точка
/// прямоугольника стоит на позиции.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct RectSpec {
    pub anchor_min: [f32; 2],
    pub anchor_max: [f32; 2],
    pub pivot: [f32; 2],
    pub offset_min: [f32; 2],
    pub offset_max: [f32; 2],
}

impl Default for RectSpec {
    fn default() -> Self {
        Self {
            anchor_min: [0.0, 0.0],
            anchor_max: [1.0, 1.0],
            pivot: [0.5, 0.5],
            offset_min: [0.0, 0.0],
            offset_max: [0.0, 0.0],
        }
    }
}

/// Явные соседи для навигации с клавиатуры и геймпада; пустое поле — автоматический выбор.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct NavSpec {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub up: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub down: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub left: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub right: Option<String>,
}

impl NavSpec {
    fn is_empty(&self) -> bool {
        self.up.is_none() && self.down.is_none() && self.left.is_none() && self.right.is_none()
    }

    fn targets(&self) -> [Option<&String>; 4] {
        [&self.up, &self.down, &self.left, &self.right].map(Option::as_ref)
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Axis {
    #[default]
    Horizontal,
    Vertical,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Corner {
    #[default]
    TopLeft,
    TopRight,
    BottomLeft,
    BottomRight,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChildAlign {
    #[default]
    Start,
    Center,
    End,
    Stretch,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum HAlign {
    #[default]
    Left,
    Center,
    Right,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum VAlign {
    #[default]
    Top,
    Middle,
    Bottom,
}

/// Допустимое содержимое InputField: `password` скрывает текст маской.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ContentType {
    #[default]
    Standard,
    Integer,
    Decimal,
    Password,
}

/// Проверяет, что текст допустим для вида содержимого. Промежуточные значения (`-`, `1.`)
/// допустимы, поэтому функция годится и для проверки редактирования по символам.
pub fn accepts_text(content: ContentType, text: &str) -> bool {
    let unsigned = text.strip_prefix('-').unwrap_or(text);
    match content {
        ContentType::Standard | ContentType::Password => true,
        ContentType::Integer => unsigned.chars().all(|c| c.is_ascii_digit()),
        ContentType::Decimal => {
            unsigned.chars().all(|c| c.is_ascii_digit() || c == '.')
                && unsigned.matches('.').count() <= 1
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct PanelSpec {
    pub color: [u8; 4],
    pub corner_radius: f32,
    pub border_width: f32,
    pub border_color: [u8; 4],
}

impl Default for PanelSpec {
    fn default() -> Self {
        Self {
            color: [255; 4],
            corner_radius: 0.0,
            border_width: 0.0,
            border_color: [0, 0, 0, 255],
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ImageSpec {
    pub path: UiPath,
    #[serde(default = "white")]
    pub tint: [u8; 4],
    /// 9-slice: отступы слева, сверху, справа, снизу в пикселях исходной картинки.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub slice: Option<[f32; 4]>,
    #[serde(default)]
    pub preserve_aspect: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct TextSpec {
    pub string: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub font: Option<UiPath>,
    pub size: f32,
    pub color: [u8; 4],
    pub h_align: HAlign,
    pub v_align: VAlign,
    pub wrap: bool,
}

impl Default for TextSpec {
    fn default() -> Self {
        Self {
            string: String::new(),
            font: None,
            size: 16.0,
            color: [255; 4],
            h_align: HAlign::Left,
            v_align: VAlign::Top,
            wrap: false,
        }
    }
}

/// Значение, которое зависит от состояния кнопки; пустое состояние берётся из `normal`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct StateValues<T> {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub normal: Option<T>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hover: Option<T>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pressed: Option<T>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub disabled: Option<T>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub focused: Option<T>,
}

impl<T> Default for StateValues<T> {
    fn default() -> Self {
        Self {
            normal: None,
            hover: None,
            pressed: None,
            disabled: None,
            focused: None,
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct ButtonSpec {
    pub tint: StateValues<[u8; 4]>,
    pub image: StateValues<UiPath>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct ToggleSpec {
    pub is_on: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub checkmark_image: Option<UiPath>,
    pub checkmark_color: [u8; 4],
}

impl Default for ToggleSpec {
    fn default() -> Self {
        Self {
            is_on: false,
            checkmark_image: None,
            checkmark_color: [255; 4],
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct SliderSpec {
    pub min: f32,
    pub max: f32,
    pub value: f32,
    pub whole_numbers: bool,
    pub direction: Axis,
    pub handle_size: f32,
    pub track_color: [u8; 4],
    pub fill_color: [u8; 4],
    pub handle_color: [u8; 4],
}

impl Default for SliderSpec {
    fn default() -> Self {
        Self {
            min: 0.0,
            max: 1.0,
            value: 0.0,
            whole_numbers: false,
            direction: Axis::Horizontal,
            handle_size: 12.0,
            track_color: [64, 64, 64, 255],
            fill_color: [80, 160, 255, 255],
            handle_color: [255; 4],
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct ProgressBarSpec {
    /// Доля заполнения от 0 до 1.
    pub value: f32,
    pub direction: Axis,
    pub background_color: [u8; 4],
    pub fill_color: [u8; 4],
}

impl Default for ProgressBarSpec {
    fn default() -> Self {
        Self {
            value: 0.0,
            direction: Axis::Horizontal,
            background_color: [64, 64, 64, 255],
            fill_color: [80, 160, 255, 255],
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct InputFieldSpec {
    pub text: String,
    pub placeholder: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_length: Option<usize>,
    pub content: ContentType,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub font: Option<UiPath>,
    pub size: f32,
    pub color: [u8; 4],
    pub placeholder_color: [u8; 4],
}

impl Default for InputFieldSpec {
    fn default() -> Self {
        Self {
            text: String::new(),
            placeholder: String::new(),
            max_length: None,
            content: ContentType::Standard,
            font: None,
            size: 16.0,
            color: [255; 4],
            placeholder_color: [160, 160, 160, 255],
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct ScrollViewSpec {
    pub horizontal: bool,
    pub vertical: bool,
    /// Смещение в единицах канваса на одну единицу колеса мыши.
    pub sensitivity: f32,
}

impl Default for ScrollViewSpec {
    fn default() -> Self {
        Self {
            horizontal: false,
            vertical: true,
            sensitivity: 40.0,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct DropdownSpec {
    pub options: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub selected: Option<usize>,
    pub popup_height: f32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub font: Option<UiPath>,
    pub size: f32,
    pub color: [u8; 4],
    pub background_color: [u8; 4],
}

impl Default for DropdownSpec {
    fn default() -> Self {
        Self {
            options: Vec::new(),
            selected: None,
            popup_height: 200.0,
            font: None,
            size: 16.0,
            color: [255; 4],
            background_color: [40, 40, 40, 255],
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct GridSpec {
    pub cell_size: [f32; 2],
    pub spacing: [f32; 2],
    pub padding: f32,
    pub columns: u32,
    pub start_corner: Corner,
}

impl Default for GridSpec {
    fn default() -> Self {
        Self {
            cell_size: [100.0, 100.0],
            spacing: [0.0, 0.0],
            padding: 0.0,
            columns: 1,
            start_corner: Corner::TopLeft,
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct StackSpec {
    pub spacing: f32,
    pub padding: f32,
    pub child_align: ChildAlign,
    /// Лишнее место по главной оси делится поровну между детьми.
    pub expand: bool,
}

fn white() -> [u8; 4] {
    [255; 4]
}

/// Вид элемента: ровно один из вариантов. В TOML — одна таблица с именем вида.
#[derive(Clone, Debug, PartialEq)]
pub enum ElementKind {
    Panel(PanelSpec),
    Image(ImageSpec),
    Text(TextSpec),
    Button(ButtonSpec),
    Toggle(ToggleSpec),
    Slider(SliderSpec),
    ProgressBar(ProgressBarSpec),
    InputField(InputFieldSpec),
    ScrollView(ScrollViewSpec),
    Dropdown(DropdownSpec),
    Grid(GridSpec),
    Horizontal(StackSpec),
    Vertical(StackSpec),
}

impl ElementKind {
    /// Виды, на которые реагируют ввод и навигация.
    pub fn is_interactive(&self) -> bool {
        matches!(
            self,
            Self::Button(_)
                | Self::Toggle(_)
                | Self::Slider(_)
                | Self::InputField(_)
                | Self::ScrollView(_)
                | Self::Dropdown(_)
        )
    }
}

/// Элемент документа. Сериализуется через [`RawElement`], где каждый вид — отдельная таблица.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(try_from = "RawElement", into = "RawElement")]
pub struct UiElement {
    pub id: String,
    pub parent: Option<String>,
    pub rect: RectSpec,
    pub visible: bool,
    pub interactable: bool,
    pub nav: NavSpec,
    pub kind: ElementKind,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawElement {
    id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    parent: Option<String>,
    #[serde(default)]
    rect: RectSpec,
    #[serde(default = "yes")]
    visible: bool,
    #[serde(default = "yes")]
    interactable: bool,
    #[serde(default, skip_serializing_if = "NavSpec::is_empty")]
    nav: NavSpec,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    panel: Option<PanelSpec>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    image: Option<ImageSpec>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    text: Option<TextSpec>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    button: Option<ButtonSpec>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    toggle: Option<ToggleSpec>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    slider: Option<SliderSpec>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    progress_bar: Option<ProgressBarSpec>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    input_field: Option<InputFieldSpec>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    scroll_view: Option<ScrollViewSpec>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    dropdown: Option<DropdownSpec>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    grid: Option<GridSpec>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    horizontal: Option<StackSpec>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    vertical: Option<StackSpec>,
}

fn yes() -> bool {
    true
}

impl TryFrom<RawElement> for UiElement {
    type Error = String;

    fn try_from(raw: RawElement) -> Result<Self, Self::Error> {
        let candidates = [
            raw.panel.map(ElementKind::Panel),
            raw.image.map(ElementKind::Image),
            raw.text.map(ElementKind::Text),
            raw.button.map(ElementKind::Button),
            raw.toggle.map(ElementKind::Toggle),
            raw.slider.map(ElementKind::Slider),
            raw.progress_bar.map(ElementKind::ProgressBar),
            raw.input_field.map(ElementKind::InputField),
            raw.scroll_view.map(ElementKind::ScrollView),
            raw.dropdown.map(ElementKind::Dropdown),
            raw.grid.map(ElementKind::Grid),
            raw.horizontal.map(ElementKind::Horizontal),
            raw.vertical.map(ElementKind::Vertical),
        ];
        let mut kinds = candidates.into_iter().flatten();
        let kind = match (kinds.next(), kinds.next()) {
            (Some(kind), None) => kind,
            _ => {
                return Err(format!(
                    "элемент {:?}: нужна ровно одна таблица вида элемента",
                    raw.id
                ));
            }
        };
        Ok(Self {
            id: raw.id,
            parent: raw.parent,
            rect: raw.rect,
            visible: raw.visible,
            interactable: raw.interactable,
            nav: raw.nav,
            kind,
        })
    }
}

impl From<UiElement> for RawElement {
    fn from(element: UiElement) -> Self {
        let mut raw = Self {
            id: element.id,
            parent: element.parent,
            rect: element.rect,
            visible: element.visible,
            interactable: element.interactable,
            nav: element.nav,
            panel: None,
            image: None,
            text: None,
            button: None,
            toggle: None,
            slider: None,
            progress_bar: None,
            input_field: None,
            scroll_view: None,
            dropdown: None,
            grid: None,
            horizontal: None,
            vertical: None,
        };
        match element.kind {
            ElementKind::Panel(spec) => raw.panel = Some(spec),
            ElementKind::Image(spec) => raw.image = Some(spec),
            ElementKind::Text(spec) => raw.text = Some(spec),
            ElementKind::Button(spec) => raw.button = Some(spec),
            ElementKind::Toggle(spec) => raw.toggle = Some(spec),
            ElementKind::Slider(spec) => raw.slider = Some(spec),
            ElementKind::ProgressBar(spec) => raw.progress_bar = Some(spec),
            ElementKind::InputField(spec) => raw.input_field = Some(spec),
            ElementKind::ScrollView(spec) => raw.scroll_view = Some(spec),
            ElementKind::Dropdown(spec) => raw.dropdown = Some(spec),
            ElementKind::Grid(spec) => raw.grid = Some(spec),
            ElementKind::Horizontal(spec) => raw.horizontal = Some(spec),
            ElementKind::Vertical(spec) => raw.vertical = Some(spec),
        }
        raw
    }
}

/// Дерево элементов по индексам документа; строится при проверке и не меняется.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Tree {
    pub(crate) parents: Vec<Option<usize>>,
    /// Дети в порядке объявления в документе.
    pub(crate) children: Vec<Vec<usize>>,
    pub(crate) roots: Vec<usize>,
    pub(crate) ids: HashMap<String, usize>,
}

/// Разобранный и проверенный документ `.rveui`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UiDocument {
    #[serde(default)]
    pub canvas: CanvasSettings,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    assets: Vec<UiAsset>,
    #[serde(default)]
    elements: Vec<UiElement>,
    #[serde(skip)]
    source_label: String,
    #[serde(skip)]
    tree: Tree,
}

impl UiDocument {
    /// Разбирает TOML и проверяет ссылки, диапазоны и ресурсы.
    pub fn parse(source_label: impl Into<String>, input: &str) -> Result<Self, UiError> {
        let source_label = source_label.into();
        let mut document: Self = toml::from_str(input)
            .map_err(|error| UiError::new(&source_label, None, error.to_string()))?;
        document.source_label = source_label;
        document.validate()?;
        Ok(document)
    }

    /// Сериализует документ в TOML; результат разбирается обратно в тот же документ.
    pub fn to_toml(&self) -> Result<String, UiError> {
        toml::to_string_pretty(self)
            .map_err(|error| UiError::new(&self.source_label, None, error.to_string()))
    }

    pub fn source_label(&self) -> &str {
        &self.source_label
    }

    pub fn assets(&self) -> &[UiAsset] {
        &self.assets
    }

    pub fn elements(&self) -> &[UiElement] {
        &self.elements
    }

    /// Индекс элемента по id; `None` для необъявленного id.
    pub fn element_index(&self, id: &str) -> Option<usize> {
        self.tree.ids.get(id).copied()
    }

    /// Дерево, согласованное с элементами: строится при проверке документа.
    pub fn tree(&self) -> &Tree {
        &self.tree
    }

    /// Проверяет документ и пересчитывает дерево. Вызывается из [`UiDocument::parse`]
    /// и после изменения документа редактором.
    pub fn validate(&mut self) -> Result<(), UiError> {
        let source = self.source_label.clone();
        self.tree = check(&source, &self.canvas, &self.assets, &self.elements)?;
        Ok(())
    }
}

fn check(
    source: &str,
    canvas: &CanvasSettings,
    assets: &[UiAsset],
    elements: &[UiElement],
) -> Result<Tree, UiError> {
    let fail = |element: Option<&str>, message: String| UiError::new(source, element, message);

    check_canvas(canvas).map_err(|message| fail(None, message))?;

    let mut kinds = HashMap::with_capacity(assets.len());
    for asset in assets {
        let path = asset.path.as_str();
        let kind = asset_kind(path).ok_or_else(|| {
            fail(
                None,
                format!("ресурс /{path}: допустимы расширения .png, .ttf и .otf"),
            )
        })?;
        if kinds.insert(path, kind).is_some() {
            return Err(fail(None, format!("повторяется ресурс /{path}")));
        }
    }

    let mut ids = HashMap::with_capacity(elements.len());
    for (index, element) in elements.iter().enumerate() {
        let id = element.id.as_str();
        if !valid_id(id) {
            return Err(fail(
                Some(id),
                "id должен состоять из [A-Za-z0-9_-] и быть длиной 1..64".to_owned(),
            ));
        }
        if ids.insert(id.to_owned(), index).is_some() {
            return Err(fail(Some(id), "id повторяется".to_owned()));
        }
    }

    let mut parents = Vec::with_capacity(elements.len());
    let mut children = vec![Vec::new(); elements.len()];
    let mut roots = Vec::new();
    for (index, element) in elements.iter().enumerate() {
        let parent = match &element.parent {
            Some(parent_id) => {
                let parent = *ids.get(parent_id).ok_or_else(|| {
                    fail(
                        Some(&element.id),
                        format!("родитель {parent_id:?} не объявлен"),
                    )
                })?;
                children[parent].push(index);
                Some(parent)
            }
            None => {
                roots.push(index);
                None
            }
        };
        parents.push(parent);
    }

    for start in 0..elements.len() {
        let mut current = parents[start];
        let mut steps = 0;
        while let Some(parent) = current {
            if parent == start || steps > elements.len() {
                return Err(fail(
                    Some(&elements[start].id),
                    "цикл в дереве родителей".to_owned(),
                ));
            }
            current = parents[parent];
            steps += 1;
        }
    }

    if let Some(focus) = &canvas.default_focus
        && !ids.contains_key(focus)
    {
        return Err(fail(
            None,
            format!("default_focus ссылается на необъявленный элемент {focus:?}"),
        ));
    }

    for element in elements {
        let id = element.id.as_str();
        let fail = |message: String| fail(Some(id), message);
        for target in element.nav.targets().into_iter().flatten() {
            if target == id {
                return Err(fail("nav ссылается на сам элемент".to_owned()));
            }
            if !ids.contains_key(target) {
                return Err(fail(format!(
                    "nav ссылается на необъявленный элемент {target:?}"
                )));
            }
        }
        check_rect(&element.rect).map_err(&fail)?;
        check_kind(&element.kind, &kinds).map_err(&fail)?;
        if let ElementKind::ScrollView(_) = element.kind {
            let count = children[ids[id]].len();
            if count != 1 {
                return Err(fail(format!(
                    "scroll_view должен содержать ровно одного ребёнка (содержимое), найдено {count}"
                )));
            }
        }
    }

    Ok(Tree {
        parents,
        children,
        roots,
        ids,
    })
}

fn check_canvas(canvas: &CanvasSettings) -> Result<(), String> {
    if !(canvas.scale.is_finite() && canvas.scale > 0.0) {
        return Err("canvas.scale должен быть конечным числом больше 0".to_owned());
    }
    let [width, height] = canvas.reference_resolution;
    if !(width.is_finite() && width > 0.0 && height.is_finite() && height > 0.0) {
        return Err("canvas.reference_resolution должен содержать положительные числа".to_owned());
    }
    if !(canvas.match_factor.is_finite() && (0.0..=1.0).contains(&canvas.match_factor)) {
        return Err("canvas.match должен быть в диапазоне 0..1".to_owned());
    }
    Ok(())
}

fn valid_id(id: &str) -> bool {
    (1..=64).contains(&id.len())
        && id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-')
}

/// Проверяет числа и якоря прямоугольника; используется и при загрузке, и при правках API.
pub(crate) fn check_rect(rect: &RectSpec) -> Result<(), String> {
    let values = [
        rect.anchor_min,
        rect.anchor_max,
        rect.pivot,
        rect.offset_min,
        rect.offset_max,
    ];
    if !values.iter().flatten().all(|value| value.is_finite()) {
        return Err("rect содержит нечисловое значение".to_owned());
    }
    for axis in 0..2 {
        let (min, max) = (rect.anchor_min[axis], rect.anchor_max[axis]);
        if !(0.0..=1.0).contains(&min) || !(0.0..=1.0).contains(&max) || min > max {
            return Err("якоря должны быть в 0..1 и anchor_min ≤ anchor_max".to_owned());
        }
        if !(0.0..=1.0).contains(&rect.pivot[axis]) {
            return Err("pivot должен быть в диапазоне 0..1".to_owned());
        }
        if min == max && rect.offset_max[axis] < 0.0 {
            return Err(
                "размер на точечной оси (offset_max) не может быть отрицательным".to_owned(),
            );
        }
    }
    Ok(())
}

fn check_kind(kind: &ElementKind, assets: &HashMap<&str, AssetKind>) -> Result<(), String> {
    let asset = |path: &UiPath, expected: AssetKind| check_asset(assets, path, expected);
    let optional_asset = |path: &Option<UiPath>, expected: AssetKind| match path {
        Some(path) => check_asset(assets, path, expected),
        None => Ok(()),
    };
    let positive = |value: f32, name: &str| {
        if value.is_finite() && value > 0.0 {
            Ok(())
        } else {
            Err(format!("{name} должен быть конечным числом больше 0"))
        }
    };
    let non_negative = |value: f32, name: &str| {
        if value.is_finite() && value >= 0.0 {
            Ok(())
        } else {
            Err(format!("{name} должен быть конечным числом не меньше 0"))
        }
    };
    match kind {
        ElementKind::Panel(spec) => {
            non_negative(spec.corner_radius, "corner_radius")?;
            non_negative(spec.border_width, "border_width")
        }
        ElementKind::Image(spec) => {
            asset(&spec.path, AssetKind::Image)?;
            match spec.slice {
                Some(slice) if slice.iter().any(|v| !v.is_finite() || *v < 0.0) => {
                    Err("slice должен содержать неотрицательные числа".to_owned())
                }
                _ => Ok(()),
            }
        }
        ElementKind::Text(spec) => {
            positive(spec.size, "size")?;
            optional_asset(&spec.font, AssetKind::Font)
        }
        ElementKind::Button(spec) => {
            for path in [
                &spec.image.normal,
                &spec.image.hover,
                &spec.image.pressed,
                &spec.image.disabled,
                &spec.image.focused,
            ]
            .into_iter()
            .flatten()
            {
                asset(path, AssetKind::Image)?;
            }
            Ok(())
        }
        ElementKind::Toggle(spec) => optional_asset(&spec.checkmark_image, AssetKind::Image),
        ElementKind::Slider(spec) => {
            if !(spec.min.is_finite() && spec.max.is_finite() && spec.min < spec.max) {
                return Err("slider: min должен быть меньше max".to_owned());
            }
            if !(spec.value.is_finite() && (spec.min..=spec.max).contains(&spec.value)) {
                return Err("slider: value вне диапазона min..max".to_owned());
            }
            positive(spec.handle_size, "handle_size")
        }
        ElementKind::ProgressBar(spec) => {
            if !(spec.value.is_finite() && (0.0..=1.0).contains(&spec.value)) {
                return Err("progress_bar: value должен быть в диапазоне 0..1".to_owned());
            }
            Ok(())
        }
        ElementKind::InputField(spec) => {
            positive(spec.size, "size")?;
            optional_asset(&spec.font, AssetKind::Font)?;
            if !accepts_text(spec.content, &spec.text) {
                return Err("input_field: text не соответствует content".to_owned());
            }
            if let Some(max) = spec.max_length {
                if max == 0 {
                    return Err("input_field: max_length должен быть больше 0".to_owned());
                }
                if spec.text.chars().count() > max {
                    return Err("input_field: text длиннее max_length".to_owned());
                }
            }
            Ok(())
        }
        ElementKind::ScrollView(spec) => positive(spec.sensitivity, "sensitivity"),
        ElementKind::Dropdown(spec) => {
            positive(spec.popup_height, "popup_height")?;
            positive(spec.size, "size")?;
            optional_asset(&spec.font, AssetKind::Font)?;
            match spec.selected {
                Some(index) if index >= spec.options.len() => {
                    Err("dropdown: selected вне списка options".to_owned())
                }
                _ => Ok(()),
            }
        }
        ElementKind::Grid(spec) => {
            if !spec.cell_size.iter().all(|v| v.is_finite() && *v > 0.0) {
                return Err("grid: cell_size должен быть больше 0".to_owned());
            }
            if !spec.spacing.iter().all(|v| v.is_finite() && *v >= 0.0) {
                return Err("grid: spacing не может быть отрицательным".to_owned());
            }
            non_negative(spec.padding, "padding")?;
            if spec.columns == 0 {
                return Err("grid: columns должен быть больше 0".to_owned());
            }
            Ok(())
        }
        ElementKind::Horizontal(spec) | ElementKind::Vertical(spec) => {
            non_negative(spec.spacing, "spacing")?;
            non_negative(spec.padding, "padding")
        }
    }
}

fn check_asset(
    assets: &HashMap<&str, AssetKind>,
    path: &UiPath,
    expected: AssetKind,
) -> Result<(), String> {
    match assets.get(path.as_str()) {
        Some(kind) if *kind == expected => Ok(()),
        Some(_) => Err(format!("ресурс /{} имеет неподходящий вид", path.as_str())),
        None => Err(format!(
            "ресурс /{} не объявлен в [[assets]]",
            path.as_str()
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SOURCE: &str = "test.rveui";

    fn parse(input: &str) -> Result<UiDocument, UiError> {
        UiDocument::parse(SOURCE, input)
    }

    fn error_of(input: &str) -> String {
        parse(input).unwrap_err().message
    }

    const FULL: &str = r#"
[canvas]
layer = 3
scale_mode = "screen"
reference_resolution = [1280.0, 720.0]
match = 1.0
default_focus = "ok"

[[assets]]
path = "/ui/icon.png"

[[assets]]
path = "/fonts/main.ttf"

[[elements]]
id = "root"
panel = { color = [10, 20, 30, 255], corner_radius = 4.0, border_width = 2.0 }

[[elements]]
id = "label"
parent = "root"
text = { string = "Привет", font = "/fonts/main.ttf", size = 24.0, h_align = "center" }
rect = { anchor_min = [0.0, 0.0], anchor_max = [1.0, 0.0], offset_max = [0.0, 40.0] }

[[elements]]
id = "ok"
parent = "root"
button = { tint = { hover = [200, 200, 200, 255] }, image = { normal = "/ui/icon.png" } }
nav = { left = "label" }
rect = { anchor_min = [0.5, 0.5], anchor_max = [0.5, 0.5], pivot = [0.5, 0.5], offset_max = [120.0, 48.0] }

[[elements]]
id = "bar"
parent = "root"
progress_bar = { value = 0.25, direction = "vertical" }
"#;

    #[test]
    fn parses_full_document_and_round_trips() {
        let document = parse(FULL).unwrap();
        assert_eq!(document.canvas.layer, 3);
        assert_eq!(document.canvas.scale_mode, ScaleMode::Screen);
        assert_eq!(document.canvas.default_focus.as_deref(), Some("ok"));
        assert_eq!(document.assets().len(), 2);
        assert_eq!(document.elements().len(), 4);
        assert_eq!(document.element_index("ok"), Some(2));

        let text = document.to_toml().unwrap();
        let again = UiDocument::parse(SOURCE, &text).unwrap();
        assert_eq!(again, document);
    }

    #[test]
    fn default_canvas_and_rect_when_tables_are_missing() {
        let document = parse(
            r#"
[[elements]]
id = "solo"
panel = {}
"#,
        )
        .unwrap();
        assert_eq!(document.canvas, CanvasSettings::default());
        let element = &document.elements()[0];
        assert_eq!(element.rect, RectSpec::default());
        assert!(element.visible && element.interactable);
    }

    #[test]
    fn rejects_unknown_fields() {
        let message = error_of(
            r#"
[[elements]]
id = "a"
panel = {}
colour = "red"
"#,
        );
        assert!(message.contains("colour"), "{message}");

        let message = error_of("[canvas]\nunknown = 1\n");
        assert!(message.contains("unknown"), "{message}");
    }

    #[test]
    fn requires_exactly_one_kind_table() {
        let none = error_of("[[elements]]\nid = \"a\"\n");
        assert!(none.contains("ровно одна таблица"), "{none}");

        let two = error_of("[[elements]]\nid = \"a\"\npanel = {}\ntext = {}\n");
        assert!(two.contains("ровно одна таблица"), "{two}");
    }

    #[test]
    fn rejects_bad_ids_and_duplicates() {
        assert!(error_of("[[elements]]\nid = \"a b\"\npanel = {}\n").contains("id"));
        let long = "x".repeat(65);
        assert!(error_of(&format!("[[elements]]\nid = \"{long}\"\npanel = {{}}\n")).contains("id"));
        let duplicate = error_of(
            "[[elements]]\nid = \"a\"\npanel = {}\n[[elements]]\nid = \"a\"\npanel = {}\n",
        );
        assert!(duplicate.contains("повторяется"), "{duplicate}");
    }

    #[test]
    fn rejects_broken_references() {
        let parent = error_of("[[elements]]\nid = \"a\"\nparent = \"nope\"\npanel = {}\n");
        assert!(parent.contains("родитель"), "{parent}");

        let nav = error_of("[[elements]]\nid = \"a\"\nnav = { up = \"missing\" }\npanel = {}\n");
        assert!(nav.contains("nav"), "{nav}");

        let self_nav = error_of("[[elements]]\nid = \"a\"\nnav = { up = \"a\" }\npanel = {}\n");
        assert!(self_nav.contains("сам элемент"), "{self_nav}");

        let focus = error_of("[canvas]\ndefault_focus = \"gone\"\n");
        assert!(focus.contains("default_focus"), "{focus}");
    }

    #[test]
    fn rejects_cycles() {
        let cycle = error_of(
            "[[elements]]\nid = \"a\"\nparent = \"b\"\npanel = {}\n[[elements]]\nid = \"b\"\nparent = \"a\"\npanel = {}\n",
        );
        assert!(cycle.contains("цикл"), "{cycle}");

        let itself = error_of("[[elements]]\nid = \"a\"\nparent = \"a\"\npanel = {}\n");
        assert!(itself.contains("цикл"), "{itself}");
    }

    #[test]
    fn rejects_undeclared_and_wrong_assets() {
        let undeclared = error_of("[[elements]]\nid = \"a\"\nimage = { path = \"/ui/x.png\" }\n");
        assert!(undeclared.contains("не объявлен"), "{undeclared}");

        let wrong = error_of(
            "[[assets]]\npath = \"/fonts/a.ttf\"\n[[elements]]\nid = \"a\"\nimage = { path = \"/fonts/a.ttf\" }\n",
        );
        assert!(wrong.contains("неподходящий"), "{wrong}");

        let extension = error_of("[[assets]]\npath = \"/ui/x.jpg\"\n");
        assert!(extension.contains("расширения"), "{extension}");
    }

    #[test]
    fn rejects_out_of_range_values() {
        let slider = error_of("[[elements]]\nid = \"s\"\nslider = { min = 1.0, max = 0.0 }\n");
        assert!(slider.contains("min"), "{slider}");

        let slider_value =
            error_of("[[elements]]\nid = \"s\"\nslider = { min = 0.0, max = 1.0, value = 2.0 }\n");
        assert!(slider_value.contains("value"), "{slider_value}");

        let progress = error_of("[[elements]]\nid = \"p\"\nprogress_bar = { value = 1.5 }\n");
        assert!(progress.contains("0..1"), "{progress}");

        let anchors = error_of(
            "[[elements]]\nid = \"a\"\npanel = {}\nrect = { anchor_min = [0.8, 0.0], anchor_max = [0.2, 0.0] }\n",
        );
        assert!(anchors.contains("якор"), "{anchors}");

        let size = error_of(
            "[[elements]]\nid = \"a\"\npanel = {}\nrect = { anchor_min = [0.5, 0.5], anchor_max = [0.5, 0.5], offset_max = [-1.0, 1.0] }\n",
        );
        assert!(size.contains("offset_max"), "{size}");

        let scale = error_of("[canvas]\nscale = 0.0\n");
        assert!(scale.contains("scale"), "{scale}");

        let grid = error_of("[[elements]]\nid = \"g\"\ngrid = { columns = 0 }\n");
        assert!(grid.contains("columns"), "{grid}");
    }

    #[test]
    fn validates_input_field_content_and_length() {
        let integer = error_of(
            "[[elements]]\nid = \"f\"\ninput_field = { text = \"12x\", content = \"integer\" }\n",
        );
        assert!(integer.contains("content"), "{integer}");

        let length = error_of(
            "[[elements]]\nid = \"f\"\ninput_field = { text = \"abcd\", max_length = 3 }\n",
        );
        assert!(length.contains("max_length"), "{length}");

        assert!(accepts_text(ContentType::Integer, "-"));
        assert!(accepts_text(ContentType::Decimal, "-1.5"));
        assert!(!accepts_text(ContentType::Decimal, "1.2.3"));
        assert!(!accepts_text(ContentType::Integer, "1.5"));
    }

    #[test]
    fn scroll_view_requires_exactly_one_child() {
        let none = error_of("[[elements]]\nid = \"sv\"\nscroll_view = {}\n");
        assert!(none.contains("ровно одного"), "{none}");

        let two = error_of(
            "[[elements]]\nid = \"sv\"\nscroll_view = {}\n[[elements]]\nid = \"a\"\nparent = \"sv\"\npanel = {}\n[[elements]]\nid = \"b\"\nparent = \"sv\"\npanel = {}\n",
        );
        assert!(two.contains("ровно одного"), "{two}");
    }

    #[test]
    fn rejects_duplicate_assets() {
        let dup = error_of("[[assets]]\npath = \"/ui/a.png\"\n[[assets]]\npath = \"/ui/a.png\"\n");
        assert!(dup.contains("повторяется"), "{dup}");
    }

    #[test]
    fn dropdown_selection_must_be_in_options() {
        let message =
            error_of("[[elements]]\nid = \"d\"\ndropdown = { options = [\"a\"], selected = 1 }\n");
        assert!(message.contains("selected"), "{message}");
    }

    #[test]
    fn element_index_follows_document_order() {
        let document = parse(FULL).unwrap();
        assert_eq!(document.element_index("root"), Some(0));
        assert_eq!(document.element_index("missing"), None);
        let tree = document.tree();
        assert_eq!(tree.roots, vec![0]);
        assert_eq!(tree.children[0], vec![1, 2, 3]);
    }
}
