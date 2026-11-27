# EngineAssets

Parent: [[Index]]

## Назначение

`engine-assets` загружает FBX, [[Glossary#Готовая модель|готовые модели]], текстуры PNG и environment panoramas Radiance HDR в память CPU. `Assets::with_resources` читает байты через [[Engine/EngineResources]] из loose files или RVEPAK; `Assets::new(root)` сохраняет совместимый файловый API. Крейт не зависит от renderer; `engine-runtime`, редактор и [[Engine/Sandbox]] переводят CPU-данные в ресурсы [[Engine/EngineRender]].

## Источники

| Файл | Значимый символ | Роль |
|---|---|---|
| `engine/engine-assets/src/lib.rs` | `ASSETS_DIR`, `Assets`, `Model`, `ModelMesh`, `ModelMaterial`, `ModelVertex`, `TextureData`, `HdrImageData`, `AssetError`, `decode_png`, `decode_hdr` | Пути ресурсов, загрузка FBX и готовых моделей, PNG через png и Radiance HDR RGBE |
| `engine/engine-assets/src/prebuilt.rs` | `PREBUILT_MODEL_VERSION`, `PREBUILT_MODEL_MAX_BYTES`, `is_prebuilt_model` | Версионированный кодек готовой модели и проверка её структуры |
| `engine/engine-assets/src/skin.rs` | `ModelSkeleton`, `ModelNode`, `ModelJoint`, `ModelSkin`, `ModelInheritMode`, `ModelTransform`, `influences` | Скелет, веса вершин, inherit modes и rigid-привязка частей |
| `engine/engine-assets/src/animation.rs` | `ModelAnimation`, `ModelTrack`, `ModelKey`, `bake_clip_file` | Клипы: TRS-ключи узлов по именам, запечённые из stacks ufbx |
| `engine/engine-assets/Cargo.toml` | feature `fbx`, `engine-resources`, опциональные `ufbx` и `bevy_mikktspace`, `png` | Разбор FBX и MikkTSpace включаются feature `fbx` (по умолчанию); готовый формат, PNG и Radiance RGBE доступны без неё |
| `engine/engine-assets/examples/model_load_bench.rs` | `model_load_bench` | Сравнение медианы загрузки FBX и готовых моделей demo-game |

## Публичные контракты

| Контракт | Поведение и условия |
|---|---|
| `ASSETS_DIR` | `"assets"` — каталог ресурсов относительно рабочего каталога; при обычном запуске это `build/assets` |
| `Assets::new(root)`, `Assets::with_resources(handle)`, `Assets::root()` | Каталог, в котором ищутся ресурсы; сервисный вариант сохраняет root из handle |
| `Assets::resolve(&self, asset_path: &str) -> Result<PathBuf, AssetError>` | `/model/cube.fbx` → `<root>/model/cube.fbx`; ведущий `/` необязателен. Разделитель — `/`. Пустой путь, пустые компоненты, `.`, `..`, `\` и `:` дают `InvalidPath`, поэтому путь не выходит за пределы каталога ресурсов |
| `Assets::load_texture(&self, asset_path) -> Result<TextureData, AssetError>` | Требует расширение `.png` (регистр не важен), читает resource bytes и декодирует их |
| `Assets::load_hdr(&self, asset_path) -> Result<HdrImageData, AssetError>` | Требует расширение `.hdr` (регистр не важен), использует тот же loose/packed resource source и декодирует Radiance RGBE |
| `Assets::load_model(&self, asset_path) -> Result<Model, AssetError>` | Требует расширение `.fbx`; по magic читает готовую модель или, при feature `fbx`, разбирает FBX и загружает его PNG-текстуры. Без `fbx` обычный FBX даёт понятный `Decode`; форматная версия готовой модели проверяется отдельно |
| `Assets::convert_model(&self, asset_path) -> Result<Vec<u8>, AssetError>` | При feature `fbx` преобразует один FBX-снимок в готовый формат; `convert_model_from_bytes` использует переданные байты, чтобы сборщик хешировал и конвертировал один и тот же снимок |
| `PREBUILT_MODEL_VERSION`, `PREBUILT_MODEL_MAX_BYTES`, `is_prebuilt_model(bytes)` | Версия формата, проверка magic `RVEMODEL` и предел размера 256 МиБ для входа и результата |
| `Assets::load_model_cancellable(&self, asset_path, cancel: &AtomicBool)` | Как `load_model`; проверяет флаг до чтения, во время разбора ufbx и перед частями и текстурами готовой модели |
| `Assets::model_dependencies(&self, asset_path) -> Result<Vec<String>, AssetError>` | Возвращает уникальные внешние PNG как ведущие `/` virtual paths в порядке base-color, normal, roughness, metallic, emissive, occlusion и opacity. Работает для FBX и готовой модели, проверяет кандидаты через общий resolver и `stat` того же loose/packed источника, не декодируя PNG. Clip-only файл с stacks возвращает пустой список; сборщик вызывает функцию для каждого объявленного FBX |
| `Assets::load_animations(&self, asset_path) -> Result<Vec<ModelAnimation>, AssetError>` | Читает готовую модель либо FBX при feature `fbx`; требует хотя бы один stack с ключами, иначе `Decode`. Имя клипа — stem файла, если stack один; иначе `stem:stack`. Ключи запекаются через `bake_anim` |
| `Model::skeleton: Option<ModelSkeleton>` | Узлы (имя, родитель, `rest` TRS, `inherit_mode`, `inherit_scale_node`) в порядке родитель-первым и `joints` (индекс узла и `inverse_bind` по столбцам). Один узел может быть joint несколько раз с разными `inverse_bind` |
| `ModelMesh::skin: Option<ModelSkin>` | Для каждой вершины по четыре `joints` и `weights`, выровненные по `vertices`. Веса берутся по control point, поэтому развёрнутые грани одной вершины получают одинаковые веса. Четыре крупнейших веса отбираются вставкой в фиксированный массив без аллокаций (`skin::influences`); при равных весах раньше остаётся кандидат, который раньше встретился в записях деформера. Веса нормализуются, нулевые и не конечные отбрасываются; вершина без положительных весов привязывается к узлу меша с весом 1 |
| `Model::animations: Vec<ModelAnimation>` | Клипы из stacks самой модели: `name`, `duration`, `tracks` (узел по имени и ключи translation/rotation/scale) |
| `decode_png(bytes) -> Result<TextureData, String>` | Любой цветовой тип и глубина PNG → RGBA8: палитра раскрывается, 16 бит усекаются до старшего байта, серый и RGB получают непрозрачную альфу. Предел памяти декодера — 512 МиБ |
| `decode_hdr(bytes) -> Result<HdrImageData, String>` | Принимает `#?RADIANCE`/`#?RGBE`, `FORMAT=32-bit_rle_rgbe` и одну из восьми ориентаций: `±Y height ±X width` или `±X width ±Y height`. Оси должны различаться; число после X — ширина, после Y — высота. Первая ось задаёт scanline, знаки — направление обхода. Декодирует flat RGBE и Radiance new-style scanline RLE (маркер сравнивается с длиной scanline) в линейный RGB `f32`, записывая пиксели в каноническую раскладку; ограничивает размеры, пиксельные буферы и промежуточные аллокации бюджетом 512 МиБ и проверяет переполнения до выделения памяти |
| `HdrImageData` | `width`, `height`, `rgb: Vec<f32>`; три линейных неотрицательных канала на пиксель, строки сверху вниз, пиксели слева направо независимо от ориентации файла. Некорректная строка ориентации (повтор оси, нет знака, лишние или недостающие токены, нулевой размер) и форматы HDR вне Radiance RGBE отвергаются как `AssetError::Decode` |
| `TextureData` | `width`, `height`, `rgba: Vec<u8>` — RGBA8 по строкам сверху вниз; цветовое пространство задаётся использованием: base-color и emissive — sRGB, metallic-roughness, normal и occlusion — linear |
| `Model` | `meshes: Vec<ModelMesh>` и `textures: Vec<TextureData>`; `bounds()` возвращает минимальный и максимальный углы всех вершин или `None` без вершин |
| `ModelMesh` | Часть модели с одним материалом: `vertices`, `indices` (каждые три — треугольник) и `material`. Вершины FBX уникальны внутри части, индексы ссылаются на общие вершины |
| `ModelMaterial` | Линейный `base_color`, HDR `emissive`, `opacity`, `alpha_mode` (`Opaque`/`Blend`), `double_sided`, `occlusion_strength`, metallic/roughness, scale нормали и необязательные индексы base-color, metallic-roughness, normal, emissive и occlusion PNG в `Model::textures`; default — белый opaque dielectric с roughness 1, чёрным emissive, `double_sided = false` и AO strength 1 |
| `ModelVertex` | `position` в метрах, единичная `normal`, `uv` от левого верхнего угла текстуры и `tangent` (`[x, y, z, w]`): единичное направление ∂P/∂u в пространстве модели и знак `w` ±1; bitangent = `cross(normal, tangent.xyz) * w` указывает в сторону роста исходного FBX `v`. `[0, 0, 0, 0]` — tangent отсутствует (нет UV или вырожденный базис). Renderer передаёт его в `Vertex.tangent` ([[Engine/EngineRender]], раздел «Tangent-space normal map»). Оси +X вправо, +Y вверх, +Z к зрителю |
| `AssetError` | `InvalidPath`, `UnsupportedFormat` (не то расширение), `Io` (ошибка чтения, в том числе отсутствие файла), `Decode` (повреждённый PNG, FBX или готовая модель, неподдерживаемая версия, модель без треугольников), `MissingTexture` со списком проверенных путей и `Cancelled` (загрузка прервана флагом отмены); реализует `Display` и `Error` |

## Готовая модель

Файл начинается с magic `RVEMODEL` и номера формата `u32` (сейчас 1). `load_model`, `load_animations` и `model_dependencies` проверяют magic до FBX-пути: готовая модель загружается без `fbx`; байты без magic требуют feature `fbx`, иначе возвращается `Decode` с просьбой собрать релиз через `rve-build`. Неизвестная версия и повреждённая структура также дают `Decode` с путём ресурса.

Формат хранит геометрию, skin, запечённые клипы, ссылки на внешние текстуры и рецепты материалов. PNG разрешаются и декодируются при загрузке модели тем же способом, что и для FBX; внешний файл остаётся актуальным, и `alpha_mode` пересчитывается по его пикселям. Встроенный PNG хранится в готовой модели. Счётчики и ссылки проверяются до выделения массивов, индексы и AABB валидируются, хвостовые байты запрещены; вход, декодирование и результат ограничены 256 МиБ.

Для замера из каталога `build/`: `cargo run --release -p engine-assets --example model_load_bench`. Пример сравнивает медиану 31 вызова `load_model`, `load_animations` и `model_dependencies` на FBX и готовой модели из `examples/demo-game`.

## Загрузка FBX

- Скелет и skin: кость — узел ufbx кластера; `inverse_bind = cluster.geometry_to_bone × inverse(geometry_to_world файла)`, потому что вершины уже переведены в пространство модели через `geometry_to_world`. Одна кость может встречаться несколько раз, по одному joint на кластер.
- Клипы: каждый `AnimStack` запекается через `ufbx::bake_anim` в локальные TRS-ключи узлов. Ключи ограничены бюджетом памяти, как PNG и HDR.
- Анимацией считается только stack с хотя бы одним ключом. Пустой stack (например, `Take 001` статичной модели) не строит скелет и не попадает в `Model::animations`; статичная модель остаётся без `skeleton`, как до скиннинга. Скелет строится, если есть skin deformer или stack с ключами.
- Тестовые фикстуры `tests/fixtures`: `static_empty_take.fbx` (пустой stack), `zup_skin.fbx` (UpAxis=Z, поворот на root, клип на кости). Последняя сверяется с `ufbx::evaluate_scene` в `engine-animation/tests/oracle.rs`.
- Клип-only FBX (`load_animations`) не содержит обязательных мешей; сопоставление с костями модели выполняет [[Engine/EngineAnimation]].

- ufbx разбирает FBX через `load_memory` с virtual filename модели и приводит сцену к осям +X вправо, +Y вверх, +Z вперёд и к метрам (`target_axes`, `target_unit_meters = 1`, `SpaceConversion::ModifyGeometry`). Внешнее чтение ufbx отключено; все файлы запрашиваются через `ResourceHandle`, а CPU-разбор выполняется вызывающим потоком до окна. Отсутствующие нормали ufbx генерирует сам.
- Обходятся все узлы с мешем. Позиции переводятся матрицей `geometry_to_world` узла, нормали — `matrix_for_normals` от неё, а затем нормализуются; у вырожденных нормалей результат `[0, 1, 0]`. При отрицательном determinant `geometry_to_world` порядок индексов каждого треугольника разворачивается, чтобы сохранить совпадение winding с преобразованными нормалями.
- Меш делится на части по материалам (`material_parts`). Материалы экземпляра в узле имеют приоритет над материалами геометрии. Многоугольники триангулируются ufbx. Углы треугольников части объединяются в одну вершину только при побитовом совпадении всех итоговых атрибутов (`position`, `normal`, `uv`, `tangent`, а у частей со skin — `joints` и `weights`; `MeshPartBuilder` в `lib.rs`). Поэтому швы UV, нормалей и знака tangent остаются раздельными вершинами, а `-0.0` и `0.0` различаются. Порядок треугольников и углов в них сохраняется; при отрицательном determinant разворачиваются индексы, а не вершины. `ModelMaterial.double_sided` читается из `ufbx::Material.features.double_sided.enabled`.
- Tangent берётся из слоёв файла `vertex_tangent` и `vertex_bitangent`, только если заданы оба. Иначе при наличии UV он считается MikkTSpace через `bevy_mikktspace` 1.0.0 (`MikkGeometry` в `lib.rs`) по каждой части. MikkTSpace получает итоговый winding (после разворота зеркального узла) и исходный FBX `v`, а не `1 − v`: знак `w` равен знаку площади треугольника в UV, поэтому зеркальный узел даёт `w = −1`. Слой файла: tangent ортогонализуется к нормали, `w` задаётся знаком проекции bitangent на `cross(normal, tangent)`. Без UV и без слоёв — `[0, 0, 0, 0]`. Углы с пустым результатом MikkTSpace тоже получают `[0, 0, 0, 0]`; ошибок у `bevy_mikktspace` 1.0 нет (тип ошибки пуст).
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

- ufbx 0.11.5 — Rust-обёртка C-библиотеки ufbx; используется только с feature `fbx`, её исходник собирается крейтом `cc`, поэтому для FBX-сборки нужен C-компилятор (MSVC на Windows, clang на macOS). Поддерживает двоичный и ASCII FBX.
- png 0.18.1 — чистый Rust-декодер PNG.
- bevy_mikktspace 1.0.0 — безопасный Rust-порт MikkTSpace без собственных зависимостей; включается feature `fbx` и используется только при отсутствии слоёв tangent и bitangent в FBX. Лицензия `Zlib AND (MIT OR Apache-2.0)`. Знак `w` берётся из `TangentSpace::tangent_encoded` (`is_orientation_preserving` → +1), то есть из знака площади треугольника в UV на итоговом winding.
- Используется в `engine-runtime` и [[Engine/Sandbox]]; service-backed чтение и startup порядок описаны в [[Engine/EngineResources]], [[Engine/EngineRuntime]] и [[Engine/Sandbox]].

## Ограничения

- Поддерживаются FBX и PNG для моделей/текстур и Radiance RGBE `.hdr` для environment. Другие форматы моделей и изображений, а также встроенные в FBX не-PNG изображения дают ошибку.
- Из FBX читаются скелет, веса, TRS-клипы узлов и геометрия. Камеры и источники света из FBX по-прежнему игнорируются; морф-таргеты не поддерживаются.
- Части модели со скелетом, не привязанные к костям, получают rigid-привязку к своему узлу (вес 1, `inverse_bind = inverse(node_to_world)`), поэтому узлы без skin (например, оружие в руке) анимируются клипом. Модели без скелета и клипов загружаются прежним путём.
- Поддерживаются base color, metallic-roughness, tangent-space normal, emissive, occlusion и opacity для описанных выше ufbx map fields; остальные PBR-свойства, bump и normal-map mip filtering не реализованы. Alpha mask не выводится из FBX: загрузчик задаёт Opaque или Blend, а cutoff mask задаётся сценой.
- `engine-assets` загружает ресурсы синхронно и не хранит кэш между вызовами. Готовая модель в release RVEPAK пропускает разбор FBX при запуске; её генерация и дисковый кэш относятся к [[Engine/RveBuild]].
