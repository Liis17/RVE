# EngineAssets

Parent: [[Index]]

## Назначение

`engine-assets` загружает модели FBX и текстуры PNG в память CPU. `Assets::with_resources` читает байты через [[Engine/EngineResources]] из loose files или RVEPAK; `Assets::new(root)` сохраняет совместимый файловый API. Крейт не зависит от renderer; `engine-runtime` и [[Engine/Sandbox]] переводят CPU-данные в ресурсы [[Engine/EngineRender]].

## Источники

| Файл | Значимый символ | Роль |
|---|---|---|
| `engine/engine-assets/src/lib.rs` | `ASSETS_DIR`, `Assets`, `Model`, `ModelMesh`, `ModelVertex`, `TextureData`, `AssetError`, `decode_png` | Пути ресурсов, загрузка FBX через ufbx и PNG через png |
| `engine/engine-assets/Cargo.toml` | `engine-resources`, `ufbx`, `png` | Чтение и stat ресурсов, разбор FBX и декодирование PNG |

## Публичные контракты

| Контракт | Поведение и условия |
|---|---|
| `ASSETS_DIR` | `"assets"` — каталог ресурсов относительно рабочего каталога; при обычном запуске это `build/assets` |
| `Assets::new(root)`, `Assets::with_resources(handle)`, `Assets::root()` | Каталог, в котором ищутся ресурсы; сервисный вариант сохраняет root из handle |
| `Assets::resolve(&self, asset_path: &str) -> Result<PathBuf, AssetError>` | `/model/cube.fbx` → `<root>/model/cube.fbx`; ведущий `/` необязателен. Разделитель — `/`. Пустой путь, пустые компоненты, `.`, `..`, `\` и `:` дают `InvalidPath`, поэтому путь не выходит за пределы каталога ресурсов |
| `Assets::load_texture(&self, asset_path) -> Result<TextureData, AssetError>` | Требует расширение `.png` (регистр не важен), читает resource bytes и декодирует их |
| `Assets::load_model(&self, asset_path) -> Result<Model, AssetError>` | Требует расширение `.fbx`, читает FBX в память и загружает все меши и их PNG-текстуры |
| `Assets::model_dependencies(&self, asset_path) -> Result<Vec<String>, AssetError>` | Возвращает уникальные внешние PNG как ведущие `/` virtual paths в порядке использования моделью; проверяет кандидаты через общий с `load_model` resolver и `stat` того же loose/packed источника, не читая и не декодируя PNG. Embedded-текстуры пропускаются, отсутствующая внешняя текстура даёт тот же `MissingTexture` |
| `decode_png(bytes) -> Result<TextureData, String>` | Любой цветовой тип и глубина PNG → RGBA8: палитра раскрывается, 16 бит усекаются до старшего байта, серый и RGB получают непрозрачную альфу. Предел памяти декодера — 512 МиБ |
| `TextureData` | `width`, `height`, `rgba: Vec<u8>` — sRGB RGBA8 по строкам сверху вниз; совпадает с входом `Renderer::create_texture` |
| `Model` | `meshes: Vec<ModelMesh>` и `textures: Vec<TextureData>`; `bounds()` возвращает минимальный и максимальный углы всех вершин или `None` без вершин |
| `ModelMesh` | Часть модели с одним материалом: `vertices`, `indices` (каждые три — треугольник) и `texture: Option<usize>` — индекс в `Model::textures` |
| `ModelVertex` | `position` в метрах, единичная `normal` и `uv` от левого верхнего угла текстуры; оси +X вправо, +Y вверх, +Z к зрителю |
| `AssetError` | `InvalidPath`, `UnsupportedFormat` (не то расширение), `Io` (ошибка чтения, в том числе отсутствие файла), `Decode` (повреждённый PNG или FBX, модель без треугольников) и `MissingTexture` со списком проверенных путей; реализует `Display` и `Error` |

## Загрузка FBX

- ufbx разбирает FBX через `load_memory` с virtual filename модели и приводит сцену к осям +X вправо, +Y вверх, +Z вперёд и к метрам (`target_axes`, `target_unit_meters = 1`, `SpaceConversion::ModifyGeometry`). Внешнее чтение ufbx отключено; все файлы запрашиваются через `ResourceHandle`, а CPU-разбор выполняется вызывающим потоком до окна. Отсутствующие нормали ufbx генерирует сам.
- Обходятся все узлы с мешем. Позиции переводятся матрицей `geometry_to_world` узла, нормали — `matrix_for_normals` от неё, а затем нормализуются; у вырожденных нормалей результат `[0, 1, 0]`.
- Меш делится на части по материалам (`material_parts`). Материалы экземпляра в узле имеют приоритет над материалами геометрии. Многоугольники триангулируются ufbx. Каждый угол треугольника становится отдельной вершиной: индексы последовательные, дедупликации вершин нет.
- В FBX ось `v` текстуры направлена вверх, поэтому загрузчик пишет `1 - v`. Без UV-слоя `uv = [0, 0]`.
- Текстура части — файловая текстура `DiffuseColor` материала FBX либо `base_color` PBR-материала. Одна текстура FBX загружается один раз и разделяется частями.
- `model_dependencies` использует тот же список кандидатов, нормализацию, проверку расширения и typed `NotFound`, что и `load_model`; ошибка доступа/источника останавливает поиск. Поэтому builder может включить транзитивные текстуры без декодирования PNG.
- Встроенный в FBX PNG декодируется из содержимого файла. Внешняя текстура ищется по нормализованному virtual path из FBX и относительным путям внутри resource root; разделители `/` и `\` поддерживаются, а `..` разрешён только пока нормализация остаётся внутри root. Для host paths используется только basename рядом с моделью, файловая система за root недоступна. Первый найденный кандидат проверяется на PNG и читается; только `NotFound` допускает следующий кандидат. Ошибка чтения, бюджета или декодирования останавливает поиск.

## Зависимости и взаимодействия

- ufbx 0.11.5 — Rust-обёртка C-библиотеки ufbx; её исходник собирается крейтом `cc` при `cargo build`, поэтому нужен C-компилятор (MSVC на Windows, clang на macOS). Поддерживает двоичный и ASCII FBX.
- png 0.18.1 — чистый Rust-декодер PNG.
- Используется в `engine-runtime` и [[Engine/Sandbox]]; service-backed чтение и startup порядок описаны в [[Engine/EngineResources]], [[Engine/EngineRuntime]] и [[Engine/Sandbox]].

## Ограничения

- Поддерживаются только FBX и PNG; другие форматы моделей и изображений, а также встроенные в FBX не-PNG изображения дают ошибку.
- Анимации, скиннинг, камеры и источники света из FBX игнорируются; читается только статическая геометрия в позе файла.
- Из материала берётся только текстура основного цвета; цвета материала, нормальные карты и прочие карты не читаются.
- Загрузка синхронная и без кэша между вызовами.
