# EngineAssets

Parent: [[Index]]

## Назначение

`engine-assets` загружает модели FBX, текстуры PNG и environment panoramas Radiance HDR в память CPU. `Assets::with_resources` читает байты через [[Engine/EngineResources]] из loose files или RVEPAK; `Assets::new(root)` сохраняет совместимый файловый API. Крейт не зависит от renderer; `engine-runtime`, редактор и [[Engine/Sandbox]] переводят CPU-данные в ресурсы [[Engine/EngineRender]].

## Источники

| Файл | Значимый символ | Роль |
|---|---|---|
| `engine/engine-assets/src/lib.rs` | `ASSETS_DIR`, `Assets`, `Model`, `ModelMesh`, `ModelMaterial`, `ModelVertex`, `TextureData`, `HdrImageData`, `AssetError`, `decode_png`, `decode_hdr` | Пути ресурсов, загрузка FBX через ufbx, PNG через png и Radiance HDR RGBE |
| `engine/engine-assets/src/skin.rs` | `ModelSkeleton`, `ModelNode`, `ModelJoint`, `ModelSkin`, `ModelInheritMode`, `ModelTransform`, `influences` | Скелет, веса вершин, inherit modes и rigid-привязка частей |
| `engine/engine-assets/src/animation.rs` | `ModelAnimation`, `ModelTrack`, `ModelKey`, `bake_clip_file` | Клипы: TRS-ключи узлов по именам, запечённые из stacks ufbx |
| `engine/engine-assets/Cargo.toml` | `engine-resources`, `ufbx`, `png` | Чтение и stat ресурсов, разбор FBX и PNG; Radiance RGBE декодируется встроенным кодом без новой зависимости |

## Публичные контракты

| Контракт | Поведение и условия |
|---|---|
| `ASSETS_DIR` | `"assets"` — каталог ресурсов относительно рабочего каталога; при обычном запуске это `build/assets` |
| `Assets::new(root)`, `Assets::with_resources(handle)`, `Assets::root()` | Каталог, в котором ищутся ресурсы; сервисный вариант сохраняет root из handle |
| `Assets::resolve(&self, asset_path: &str) -> Result<PathBuf, AssetError>` | `/model/cube.fbx` → `<root>/model/cube.fbx`; ведущий `/` необязателен. Разделитель — `/`. Пустой путь, пустые компоненты, `.`, `..`, `\` и `:` дают `InvalidPath`, поэтому путь не выходит за пределы каталога ресурсов |
| `Assets::load_texture(&self, asset_path) -> Result<TextureData, AssetError>` | Требует расширение `.png` (регистр не важен), читает resource bytes и декодирует их |
| `Assets::load_hdr(&self, asset_path) -> Result<HdrImageData, AssetError>` | Требует расширение `.hdr` (регистр не важен), использует тот же loose/packed resource source и декодирует Radiance RGBE |
| `Assets::load_model(&self, asset_path) -> Result<Model, AssetError>` | Требует расширение `.fbx`, читает FBX в память и загружает используемые FBX-текстуры PNG |
| `Assets::load_model_cancellable(&self, asset_path, cancel: &AtomicBool)` | Как `load_model`, но при `cancel = true` возвращает `Cancelled`: флаг проверяется до чтения, в `progress_cb` ufbx во время разбора и перед каждым узлом при сборке мешей |
| `Assets::model_dependencies(&self, asset_path) -> Result<Vec<String>, AssetError>` | Возвращает уникальные внешние PNG как ведущие `/` virtual paths в порядке base-color, normal, roughness, metallic, emissive, occlusion и opacity. Проверяет кандидаты через общий с `load_model` resolver и `stat` того же loose/packed источника, не читая и не декодируя PNG. Embedded-текстуры и metallic/roughness-карты классических шейдеров пропускаются; отсутствующая внешняя текстура даёт тот же `MissingTexture`. FBX без треугольников, но с анимационными stacks (клип-only файл), возвращает пустой список; сборка вызывает эту функцию для каждого объявленного FBX |
| `Assets::load_animations(&self, asset_path) -> Result<Vec<ModelAnimation>, AssetError>` | Требует расширение `.fbx` и хотя бы один stack с ключами, иначе `Decode` («в FBX нет анимационных стеков»). Имя клипа — stem файла, если stack один; иначе `stem:stack`. Ключи запекаются через `bake_anim` |
| `Model::skeleton: Option<ModelSkeleton>` | Узлы (имя, родитель, `rest` TRS, `inherit_mode`, `inherit_scale_node`) в порядке родитель-первым и `joints` (индекс узла и `inverse_bind` по столбцам). Один узел может быть joint несколько раз с разными `inverse_bind` |
| `ModelMesh::skin: Option<ModelSkin>` | Для каждой вершины по четыре `joints` и `weights`, выровненные по `vertices`. Веса берутся по control point, поэтому развёрнутые грани одной вершины получают одинаковые веса. Нормализуются четыре крупнейших веса; вершина без положительных весов привязывается к узлу меша с весом 1 |
| `Model::animations: Vec<ModelAnimation>` | Клипы из stacks самой модели: `name`, `duration`, `tracks` (узел по имени и ключи translation/rotation/scale) |
| `decode_png(bytes) -> Result<TextureData, String>` | Любой цветовой тип и глубина PNG → RGBA8: палитра раскрывается, 16 бит усекаются до старшего байта, серый и RGB получают непрозрачную альфу. Предел памяти декодера — 512 МиБ |
| `decode_hdr(bytes) -> Result<HdrImageData, String>` | Принимает `#?RADIANCE`/`#?RGBE`, `FORMAT=32-bit_rle_rgbe` и resolution только `-Y height +X width`. Декодирует flat RGBE и Radiance new-style scanline RLE в линейный RGB `f32`; ограничивает размеры, пиксельные буферы и промежуточные аллокации бюджетом 512 МиБ и проверяет переполнения до выделения памяти |
| `HdrImageData` | `width`, `height`, `rgb: Vec<f32>`; три линейных неотрицательных канала на пиксель, scanlines сверху вниз. Другие orientations/форматы HDR отвергаются как `AssetError::Decode` |
| `TextureData` | `width`, `height`, `rgba: Vec<u8>` — RGBA8 по строкам сверху вниз; цветовое пространство задаётся использованием: base-color и emissive — sRGB, metallic-roughness, normal и occlusion — linear |
| `Model` | `meshes: Vec<ModelMesh>` и `textures: Vec<TextureData>`; `bounds()` возвращает минимальный и максимальный углы всех вершин или `None` без вершин |
| `ModelMesh` | Часть модели с одним материалом: `vertices`, `indices` (каждые три — треугольник) и `material` |
| `ModelMaterial` | Линейный `base_color`, HDR `emissive`, `opacity`, `alpha_mode` (`Opaque`/`Blend`), `double_sided`, `occlusion_strength`, metallic/roughness, scale нормали и необязательные индексы base-color, metallic-roughness, normal, emissive и occlusion PNG в `Model::textures`; default — белый opaque dielectric с roughness 1, чёрным emissive, `double_sided = false` и AO strength 1 |
| `ModelVertex` | `position` в метрах, единичная `normal` и `uv` от левого верхнего угла текстуры; оси +X вправо, +Y вверх, +Z к зрителю |
| `AssetError` | `InvalidPath`, `UnsupportedFormat` (не то расширение), `Io` (ошибка чтения, в том числе отсутствие файла), `Decode` (повреждённый PNG или FBX, модель без треугольников), `MissingTexture` со списком проверенных путей и `Cancelled` (загрузка прервана флагом отмены); реализует `Display` и `Error` |

## Загрузка FBX

- Скелет и skin: кость — узел ufbx кластера; `inverse_bind = cluster.geometry_to_bone × inverse(geometry_to_world файла)`, потому что вершины уже переведены в пространство модели через `geometry_to_world`. Одна кость может встречаться несколько раз, по одному joint на кластер.
- Клипы: каждый `AnimStack` запекается через `ufbx::bake_anim` в локальные TRS-ключи узлов. Ключи ограничены бюджетом памяти, как PNG и HDR.
- Анимацией считается только stack с хотя бы одним ключом. Пустой stack (например, `Take 001` статичной модели) не строит скелет и не попадает в `Model::animations`; статичная модель остаётся без `skeleton`, как до скиннинга. Скелет строится, если есть skin deformer или stack с ключами.
- Тестовые фикстуры `tests/fixtures`: `static_empty_take.fbx` (пустой stack), `zup_skin.fbx` (UpAxis=Z, поворот на root, клип на кости). Последняя сверяется с `ufbx::evaluate_scene` в `engine-animation/tests/oracle.rs`.
- Клип-only FBX (`load_animations`) не содержит обязательных мешей; сопоставление с костями модели выполняет [[Engine/EngineAnimation]].

- ufbx разбирает FBX через `load_memory` с virtual filename модели и приводит сцену к осям +X вправо, +Y вверх, +Z вперёд и к метрам (`target_axes`, `target_unit_meters = 1`, `SpaceConversion::ModifyGeometry`). Внешнее чтение ufbx отключено; все файлы запрашиваются через `ResourceHandle`, а CPU-разбор выполняется вызывающим потоком до окна. Отсутствующие нормали ufbx генерирует сам.
- Обходятся все узлы с мешем. Позиции переводятся матрицей `geometry_to_world` узла, нормали — `matrix_for_normals` от неё, а затем нормализуются; у вырожденных нормалей результат `[0, 1, 0]`. При отрицательном determinant `geometry_to_world` порядок индексов каждого треугольника разворачивается, чтобы сохранить совпадение winding с преобразованными нормалями.
- Меш делится на части по материалам (`material_parts`). Материалы экземпляра в узле имеют приоритет над материалами геометрии. Многоугольники триангулируются ufbx. Каждый угол треугольника становится отдельной вершиной: индексы последовательные, дедупликации вершин нет. `ModelMaterial.double_sided` читается из `ufbx::Material.features.double_sided.enabled`.
- В FBX ось `v` текстуры направлена вверх, поэтому загрузчик пишет `1 - v`. Без UV-слоя `uv = [0, 0]`.
- Base-color текстура части — файловая `DiffuseColor` материала FBX либо `base_color` PBR-карты; при её наличии цветовой фактор равен белому. Иначе берётся значение `pbr.base_color`, затем `fbx.diffuse_color * diffuse_factor`, затем белый. Значения передаются линейными без gamma-конверсии.
- Metallic и roughness извлекаются только для `OslStandardSurface`, `ArnoldStandardSurface`, `E3DsMaxPhysicalMaterial`, `E3DsMaxPbrMetalRough`, `E3DsMaxPbrSpecGloss`, `GltfMaterial`, `OpenpbrMaterial` и `ShaderfxGraph`. Классические Lambert/Phong, BlenderPhong, Wavefront и неизвестные шейдеры всегда дают metallic 0, roughness 1 и не используют PBR-карты; это не превращает отражение и shininess классического Phong в PBR.
- Одна общая metallic-roughness карта уже содержит G=roughness и B=metallic. Раздельные карты упаковываются из своих красных каналов в G/B; отсутствующий канал равен 255, R=0, A=255. Разные размеры дают `AssetError::Decode`. Исходные картинки кэшируются по `element_id`, упакованные пары — по id roughness/metallic.
- Normal map берётся сначала из `pbr.normal_map`, затем из `fbx.normal_map`; scale равен 1. Bump не используется. Base, normal, roughness и metallic текстуры разделяют общий кэш исходных файлов.
- Emissive берётся как `pbr.emission_color × pbr.emission_factor` либо из соответствующих FBX emission maps; его текстура передаётся как sRGB. Occlusion берётся из `pbr.ambient_occlusion`; карта читается как Linear, runtime шейдер использует красный канал, а scalar map value становится `occlusion_strength`.
- Opacity берётся из `pbr.opacity`, иначе как `1 − transparency_factor × transparency_color` из FBX. Наличие opacity<1, opacity map или прозрачной base-color alpha выбирает `ModelAlphaMode::Blend`. File opacity map упаковывается в alpha base-color карты (красный канал; FBX transparency умножается на парный скаляр и затем инвертируется; при этом material opacity равен 1), чтобы текущий renderer мог читать alpha без дополнительного texture binding. При несовпадении размеров base-color и opacity карт загрузка завершается `AssetError::Decode`; неподдерживаемый тип opacity texture тоже даёт Decode вместо молчаливого пропуска. Встроенные PNG допустимы и не являются builder dependencies.
- `model_dependencies` использует тот же список кандидатов, нормализацию, проверку расширения и typed `NotFound`, что и `load_model`; ошибка доступа/источника останавливает поиск. Поэтому builder может включить транзитивные текстуры без декодирования PNG.
- Встроенный в FBX PNG декодируется из содержимого файла. Внешняя текстура ищется по нормализованному virtual path из FBX и относительным путям внутри resource root; разделители `/` и `\` поддерживаются, а `..` разрешён только пока нормализация остаётся внутри root. Для host paths используется только basename рядом с моделью, файловая система за root недоступна. Первый найденный кандидат проверяется на PNG и читается; только `NotFound` допускает следующий кандидат. Ошибка чтения, бюджета или декодирования останавливает поиск.

## Зависимости и взаимодействия

- ufbx 0.11.5 — Rust-обёртка C-библиотеки ufbx; её исходник собирается крейтом `cc` при `cargo build`, поэтому нужен C-компилятор (MSVC на Windows, clang на macOS). Поддерживает двоичный и ASCII FBX.
- png 0.18.1 — чистый Rust-декодер PNG.
- Используется в `engine-runtime` и [[Engine/Sandbox]]; service-backed чтение и startup порядок описаны в [[Engine/EngineResources]], [[Engine/EngineRuntime]] и [[Engine/Sandbox]].

## Ограничения

- Поддерживаются FBX и PNG для моделей/текстур и Radiance RGBE `.hdr` для environment. Другие форматы моделей и изображений, а также встроенные в FBX не-PNG изображения дают ошибку.
- Из FBX читаются скелет, веса, TRS-клипы узлов и геометрия. Камеры и источники света из FBX по-прежнему игнорируются; морф-таргеты не поддерживаются.
- Части модели со скелетом, не привязанные к костям, получают rigid-привязку к своему узлу (вес 1, `inverse_bind = inverse(node_to_world)`), поэтому узлы без skin (например, оружие в руке) анимируются клипом. Модели без скелета и клипов загружаются прежним путём.
- Поддерживаются base color, metallic-roughness, tangent-space normal, emissive, occlusion и opacity для описанных выше ufbx map fields; остальные PBR-свойства, bump и normal-map mip filtering не реализованы. Alpha mask не выводится из FBX: загрузчик задаёт Opaque или Blend, а cutoff mask задаётся сценой.
- Загрузка синхронная и без кэша между вызовами.
