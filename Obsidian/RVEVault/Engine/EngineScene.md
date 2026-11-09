# EngineScene

Parent: [[Index]]

## Назначение

`engine-scene` разбирает и проверяет TOML-документы `.rvescene`, создаёт из них runtime-граф `World` и готовит мировые матрицы, камеру, направленный свет, environment и экземпляры мешей для кадра. Он использует `glam` для математики, `engine-files::VirtualPath` для путей и serde/TOML для документов. От renderer, окна и script runtime крейт не зависит.

## Источники

| Файл | Значимый символ | Роль |
|---|---|---|
| `engine/engine-scene/src/transform.rs` | `Transform` | Положение, вращение и масштаб с матрицей `T · R · S` |
| `engine/engine-scene/src/camera.rs` | `Camera`, `OrbitCamera` | Перспективная камера и горизонтальная орбита вокруг цели |
| `engine/engine-scene/src/camera_settings.rs` | `CameraSettings` | Настройки проекции, render scale, clear color и anti-aliasing для `.camerasettings`; устаревший блок `color` |
| `engine/engine-scene/src/post_process.rs` | `PostProcessSettings`, `resolve_post_process` | Постобработка камеры из `.postprocess`, см. [[EnginePostProcess]] |
| `engine/engine-scene/src/physics_material.rs` | `PhysicsMaterialSettings`, `CombineMode` | Физический материал коллайдера для `.rvephysmat`: трение покоя и скольжения, отскок, правила совмещения; загрузка разрешённого значения в объект |
| `engine/engine-scene/src/light.rs` | `sun_direction`, `OrbitingSun`, `ShadowSettings`, `cascade_view_projections`, `point_shadow_view_projections`, `spot_shadow_view_projection` | Направление света, camera-relative каскады и Vulkan-depth матрицы point/spot shadow views без зависимости от renderer |
| `engine/engine-scene/src/document.rs` | `SceneDocument`, компоненты, `ParamValue`, `SceneError` | Схема `.rvescene`, TOML-сериализация и проверка документов, ссылок и базисов |
| `engine/engine-scene/src/asset_kind.rs` | `AssetKind` | Общая классификация ассетов по расширению для схем скриптов, редактора, сборщика и runtime |
| engine/engine-scene/src/material_asset.rs | MaterialAsset, ResolvedMaterialAsset | Формат .rvemat, shader/texture dependencies и schema-resolved значения; детали в [[Engine/EngineMaterials]] |
| engine/engine-scene/src/shader_asset.rs | ShaderAsset, ShaderSchema | Формат .rveshader, проверка GLSL property declarations и slot layout; детали в [[Engine/EngineMaterials]] |
| `engine/engine-scene/src/prefab.rs` | `PrefabInstance`, `PrefabOverride`, `expand`, `Expansion`, `PrefabTemplate` | Экземпляры префабов, component overrides, раскрытие в плоский документ и шаблон spawn-префаба |
| `engine/engine-scene/src/params.rs` | `ParamSchema`, `ParamSpec`, `ParamKind` | Схема параметров скрипта и разрешение значений экземпляра |
| `engine/engine-scene/src/world.rs` | `ObjectHandle`, `World`, `WorldObject`, `SpawnOptions`, `SceneFrame`, `EnvironmentState`, `SceneWarning` | Хендлы с поколениями, runtime-объекты, spawn/despawn, топологический порядок, мировые матрицы и кэш мировых вращений, источник/ориентация окружения и данные кадра |
| `engine/engine-scene/src/network.rs` | `NetworkIdentityComponent`, `NetworkTransformComponent`, `NetId`, `scene_net_id`, `NetRequest`, `NetEvent`, лимиты значений сообщения | Сетевые компоненты объектов и общие типы сетевого модуля; протокол — в [[Engine/EngineNetwork]] |
| `engine/engine-scene/Cargo.toml` | зависимости крейта | glam, engine-files, blake3, serde и toml; нет renderer/script/оконных и сетевых API |

## Документ сцены `.rvescene`

- Корневые таблицы `[[assets]]` объявляют канонические virtual paths и необязательное имя `archive` и лимит `max_instances`; `[[objects]]` задают объекты в стабильном порядке документа. У объекта есть уникальный `id`, необязательные `name` и `parent`, локальный `transform` и компоненты `camera`, `mesh_renderer`, `lod_group`, `directional_light`, `point_light`, `spot_light`, `environment`, `collider`, `rigid_body`, `character_controller`, `lite_scripts` и `heavy_scripts`. Неизвестные поля схемы запрещены.
- Путь объекта-ресурса сериализуется со слешем в начале (`/models/ship.fbx`). Пути ассетов уникальны; модель, base-color replacement, карты `MaterialOverride`, настройки камеры, environment и lite-скрипт должны ссылаться на объявленный asset с расширением `.fbx`, `.png`, `.camerasettings`, `.hdr` или `.lua` соответственно без учёта регистра. В `MeshRenderer.material` каждый override-фактор, карта и `double_sided` необязательны: `base_color` задаёт линейный цвет, `metallic`, `roughness`, `opacity` и `occlusion_strength` лежат в `[0, 1]`, `emissive` — конечный неотрицательный HDR RGB, `normal_scale` должен быть конечным. `alpha_mode` принимает `opaque`, `mask` или `blend`; `alpha_cutoff` допустим только для `mask` и лежит в `[0, 1]` (если не задан, runtime использует 0.5). Metallic-roughness, tangent-space normal, sRGB emissive и linear occlusion карты используют объявленные PNG-пути. Неуказанные поля, включая `double_sided`, сохраняют значения материала FBX. Неизвестные поля запрещены. MeshRenderer.materials также ссылается на объявленные .rvemat; форматы и shader dependencies описаны в [[Engine/EngineMaterials]].
- MeshRenderer.materials — массив слотов по индексу FBX part; пустая строка "" сериализует пустой слот (None) и сохраняет FBX material. Те же индексы используются для всех LOD levels; слоты за числом parts модели не рисуются. Назначенный .rvemat заменяет FBX material этой части; Lit применяет затем MeshRenderer.texture и заданные поля MeshRenderer.material, custom shader игнорирует оба legacy override. Prefab MeshRendererOverride.materials заменяет список целиком.
- `lod_group` требует `mesh_renderer` и содержит от 1 до 6 дополнительных `levels` (итого 2–7 моделей вместе с базовой). Каждый уровень задаёт объявленную FBX-модель, необязательную объявленную PNG-текстуру и конечный положительный `distance`; пороги строго возрастают. `cull_distance` необязателен, конечен и должен превышать последний порог. Неуказанная текстура уровня наследует `mesh_renderer.texture`, а при её отсутствии сохраняется встроенная текстура FBX-модели.
- `collider` необязателен. `shape` принимает `box` (default), `sphere`, `capsule`, `convex_mesh` или `triangle_mesh`; defaults размеров — `half_extents = [0.5, 0.5, 0.5]`, `radius = 0.5`, `height = 1.0`, `offset = [0, 0, 0]`; `trigger = false`, `friction = 0.5`, `restitution = 0.0`. Для box все half-extents положительны, для sphere положителен radius, для capsule положительны radius и высота цилиндрической части. Все числа конечны; friction неотрицателен, restitution лежит в `[0, 1]`. Mesh-формы требуют `mesh_renderer`; их геометрия берётся из базовой `mesh_renderer.model`, LOD для этого контракта не задаётся.
- `collider.material` — необязательный путь `.rvephysmat` из `[[assets]]`. Непустой `material` заменяет инлайн `friction` и `restitution` (они остаются в документе, но в физике не используются). Проверка пути та же, что у `camera.settings`: ресурс должен быть объявлен и иметь расширение `.rvephysmat`. Override `collider` в экземпляре префаба заменяет компонент целиком, включая `material`, и проверяет путь так же.
- Физический материал — TOML `.rvephysmat` с полями `static_friction`, `dynamic_friction` (оба конечны и `>= 0`), `bounciness` в `[0, 1]`, `friction_combine` и `bounce_combine` в значениях `average`, `minimum`, `multiply`, `maximum`. Defaults — как в Unity: трение 0.6, отскок 0, оба правила `average`; неизвестные поля запрещены. Значение загружает runtime или редактор после `World::from_document` и для шаблонов префабов (`PrefabTemplate::resolve_physics_materials`): результат хранится в `ColliderComponent.resolved_material` и не сериализуется. Ошибка чтения или разбора — ошибка загрузки объекта с путём файла.
- `rigid_body` необязателен и требует `collider`. `kind` — `static`, `kinematic` или `dynamic` (default `dynamic`); defaults остальных полей: `mass = 1.0`, `linear_damping = 0.0`, `angular_damping = 0.05`, `gravity_scale = 1.0`, `lock_rotation = false`. Mass положительна, damping неотрицателен, числовые значения конечны. `dynamic` несовместим с `triangle_mesh`.
- `character_controller` задаёт собственную капсулу и несовместим с `collider` и `rigid_body`. Defaults: `radius = 0.35`, `height = 1.1`, `step_height = 0.3`, `max_slope_deg = 45`, `snap_distance = 0.2`, `skin = 0.02`, `push_strength = 1.0`. Все числа конечны; radius и height положительны, slope лежит в `(0, 90)`, остальные поля неотрицательны.
- Поза `dynamic` rigid body и `character_controller` записывается физикой, поэтому такие объекты должны быть корневыми. Их дети разрешены. `static` и `kinematic` объекты могут иметь родителей. `engine-scene` проверяет и хранит эту схему, но не выполняет симуляцию.
- Объект имеет `active` (по умолчанию `true`, не сериализуется при `true`), `tag` (`[A-Za-z0-9_-]{1,64}` или пусто) и `layer` (`0..32`, по умолчанию `0`). Флаг `enabled` (по умолчанию `true`) есть у `lite_scripts` и `heavy_scripts` и задаёт начальное состояние `script:set_enabled`. Имена тегов и слоёв проекта лежат в `[tags]` и `[layers]` [[Engine/EngineProject]]; `engine-scene` проверяет формат и диапазон, но не сверяет их с проектом.
- Имена архивов соответствуют `[A-Za-z0-9_-]{1,64}`. `base` и `shared` зарезервированы без учёта ASCII-регистра. Документ сцены не проверяет коллизии имён разных архивов, различающихся только регистром; это требование к сборщику проекта.
- Идентификаторы объектов соответствуют `[A-Za-z0-9_-]{1,64}`. Родитель должен существовать; иерархия не содержит циклов. В документе должна быть хотя бы одна камера, не более одного направленного света и не более одного environment; количество point/spot lights не ограничено этим правилом. Камера допускает `settings` (virtual path `.camerasettings`) и `active`; если камера одна, она активна по умолчанию. При нескольких камерах ровно одна должна иметь `active = true`. Таблица `[objects.camera.cursor]` необязательна и содержит флаги `hide_on_start` и `lock_on_start` (оба по умолчанию `false`, неизвестные поля запрещены). Флаги независимы и действуют при старте сцены только для активной камеры; если таблица содержит значения по умолчанию, при сохранении её нет. Старые сцены читаются с выключенными флагами. Prefab override камеры заменяет компонент целиком, поэтому флаги курсора переносятся вместе с ним.
- `environment` выбирает ровно один источник: объявленный `hdr` с расширением `.hdr` либо таблицу `procedural` с линейными `zenith_color`, `horizon_color`, `ground_color`, `sun_size_deg` и `sun_intensity`. Неизвестные поля запрещены; `intensity` конечен и `>= 0`, цвета конечны и неотрицательны, `sun_size_deg` лежит в `(0, 180]`, `sun_intensity` конечен и `>= 0`. `skybox` по умолчанию `true`. При отсутствии компонента используется встроенный ambient environment renderer.
- `transform.position`, `rotation` и `scale` — конечные тройки чисел; целочисленные TOML-литералы принимаются и для компонентов `f32`. Вращение задаётся в градусах, локальная матрица — `T · R · S` с порядком Эйлера YXZ, мировая матрица — `parent · local`.
- Настройки камеры задаются отдельным TOML `.camerasettings` с таблицами `projection`, `render` и устаревшей `color`; неизвестные поля запрещены, все поля имеют defaults. `projection.fov_deg` конечен в `(0, 180)`, `0 < near < far`; FOV задаётся по короткой стороне окна. `render.render_scale` конечен в `(0, 1]`, `clear_color` — конечная тройка с компонентами `>= 0`, `anti_aliasing` — `fxaa`, `none`, `taa`, `fsr` или `dlss` (serde значения в нижнем регистре). TAA использует render-scale и temporal history output-resolution; EV остаётся отдельной post-resolve настройкой. `color.exposure_ev` конечен в `[-16, 16]`, `tonemapper` — `aces`, `neutral` или `none`. Старые поля `fov_deg`, `near` и `far` внутри `[objects.camera]` считаются неизвестными и вызывают ошибку. Поле `post_process` в `[objects.camera]` ссылается на объявленный `.postprocess`; таблица `color` устарела, её exposure и tonemapper разрешаются по правилам из [[EnginePostProcess]]. Массив `screen_shaders` в `[objects.camera]` задаёт до четырёх экранных шейдеров камеры (`shader` — объявленный `.glsl`, `stage`, `enabled`, до шестнадцати `params`); проверка выполняется `validate_screen_shaders`, см. [[Engine/EngineScreenShaders]]. `World::set_screen_shader` меняет только включение и параметры существующего слота, путь и стадия фиксированы.
- Направление forward камеры извлекается из мирового −Z, up — из мирового +Y. После нормализации обоих векторов модуль `forward × up` должен быть не меньше `1e-4`. Цель рассчитывается как `position + forward * max(1.0, position.length())`, сохраняя точность вдали от начала координат.
- У направленного света значения по умолчанию: `color = [1, 1, 1]`, `ambient = [0.15, 0.15, 0.15]`, `shadows = false`, `shadow_distance = 100`, `shadow_cascades = 4`. Цвет и ambient должны быть конечными и иметь компоненты `>= 0`; дистанция теней конечна и строго положительна, число каскадов — от 1 до 4. Удалённые `shadow_center` и `shadow_radius` считаются неизвестными полями.
- `point_light` и `spot_light` разрешены на одном объекте и на любом числе объектов независимо от единственного directional light. Их defaults: белый цвет, интенсивность 1, range 10 и `shadows = false`; у spot также `inner_angle_deg = 20`, `outer_angle_deg = 30`. Цвет и интенсивность конечны и неотрицательны, range конечен и положителен; spot требует `0 < inner <= outer <= 90`. Неизвестные поля запрещены. `shadows = true` запрашивает локальные тени. `point_shadow_view_projections` строит шесть квадратных 90° view-projection матриц с глубиной Vulkan `[0, 1]` в порядке `+X, -X, +Y, -Y, +Z, -Z`; `spot_shadow_view_projection` строит перспективу из позиции, направления, внешнего угла и range. Near plane равен `min(range × 0.01, 0.05)`; при finite range `< 0.001` свет остаётся активным, но матрица не создаётся, поэтому renderer его не затеняет. Перспектива 180° сингулярна: spot с внешним углом 90° сохраняет исходный световой конус, а shadow coverage ограничивается конечным FOV `π − 0.01` (примерно 179.43°). Невалидный базис или матрица отключает только тень этого источника; его освещение сохраняется.
- `lite_scripts` содержат путь `.lua` к объявленному asset. У `heavy_scripts` поле `name` соответствует `[A-Za-z0-9_]{1,64}`. У каждого скрипта поле `params` — таблица ключей и значений.
- `ParamValue` принимает bool, целое число диапазона `i64`, конечное вещественное число, строку, массивы и ссылки `{ object = "id" }` / `{ asset = "path" }`. В массиве допустимы обычные значения и ссылки. Таблицы с другими ключами и TOML-даты/время отклоняются. Пустая ссылка задаётся отсутствующим ключом параметра и получает значение none из схемы.

## Компоненты физики в World

- `character_controller` содержит `use_gravity` (по умолчанию `false`), `gravity_scale` (`1.0`) и `jump_height` (`1.0`). Значения по умолчанию не сериализуются, поэтому старые сцены загружаются и сохраняются без изменений.
- `World::set_collider`, `set_rigid_body` и `set_character_controller` заменяют компонент уже существующего объекта после проверки значений. Они не добавляют и не удаляют компоненты: физическая синхронизация создаёт тела при загрузке и активации. Скрипты пишут через заявки транзакции, см. [[Engine/EngineScript]]. Коллайдер принимает только примитивы; mesh-формы задаются моделью при загрузке.

## Префабы

`engine/engine-scene/src/prefab.rs` — единственное описание формата; остальные заметки ссылаются сюда.

**Префаб** — TOML той же схемы, что сцена (`[[assets]]`, `[[objects]]`, `[[instances]]`). Разбирается `SceneDocument::parse_prefab` (`is_prefab()`). Правила:
- ровно один корень — объект или экземпляр без `parent`;
- камера не обязательна;
- ≤1 directional light; point/spot lights допускаются множественно.

**Экземпляр** `[[instances]]` допустим и в сцене, и в префабе. Поля:
- `id` — id корня раскрытого префаба, авторский формат;
- `name`, `parent` — необязательные; пустое имя экземпляра очищает имя корня; `parent` может быть любым id раскрытого документа;
- `prefab` — путь `.rveprefab`, объявленный в `[[assets]]` этого документа;
- `transform` — заменяет transform корня целиком;
- `[[instances.overrides]]` — overrides.

**Overrides** — разреженные правки одного объекта префаба по `target`:
- `target` — id в раскрытом префабе, у вложенных через точку (`lock.bolt`);
- `name` — заменяется; пустая строка явно очищает унаследованное имя;
- `transform` — частично, по заданным полям;
- mesh_renderer — model/texture заменяются, materials заменяет весь список per-part слотов, а поля material (факторы, alpha mode/cutoff, двусторонность и карты) сливаются по отдельности;
- `camera`, `directional_light`, `point_light`, `spot_light`, `environment`, `collider`, `rigid_body`, `character_controller` — заменяются целиком;
- `lite_params` по пути скрипта и `heavy_params` по имени — слияние ключей.

Ссылки `ParamValue::Object` внутри префаба указывают только на объекты этого префаба; ссылка на внешний объект отклоняется. При раскрытии ссылки из самого префаба получают итоговый id экземпляра. Ссылки, заданные в overrides, остаются в контексте документа экземпляра: они могут адресовать внешний объект или полный id вроде `inst.child`. При `World::spawn` ссылки на объекты шаблона переназначаются на созданные id.

Ограничения overrides:
- компоненты не добавляются и не удаляются: override отсутствующего компонента или несуществующий `target` — ошибка;
- `target` в одном экземпляре не повторяется;
- ресурсы, которые вводит override, объявляются в `[[assets]]` документа с экземпляром.

**Идентификаторы.** Авторские id — `[A-Za-z0-9_-]{1,64}`, точка запрещена. Корень раскрытого префаба получает id экземпляра, остальные объекты — `<экземпляр>.<id>`, вложенные — `a.b.c`. Раскрытый id не длиннее 255 байт. Скрипты находят такие объекты обычным `rve.find("door1.handle")` / `World::handle`.

**Раскрытие** — `expand(document, load) -> Expansion { document, prefabs, origins }`:
- Префабы раскрываются рекурсивно с кэшем по пути. Цикл — ошибка с цепочкой путей, вложенность ограничена 16.
- Каждый префаб после раскрытия проверяется целиком по правилам префаба; ошибка указывает его метку.
- Порядок для одного экземпляра:
  1. вложенные уровни со своими overrides;
  2. overrides этого экземпляра — внешний побеждает внутренний;
  3. `name`/`transform`/`parent` самого экземпляра — побеждают override корня.
- Объекты раскрытого документа: собственные объекты, затем экземпляры в порядке объявления. Поэтому «первая камера» — первая среди собственных объектов, если она есть.
- `[[assets]]` объединяются по пути. Явный `archive` документа с экземпляром побеждает; разные явные архивы из разных префабов — ошибка.
- `prefabs` — все использованные файлы, включая вложенные.
- `origins` сопоставляет объекты экземпляров верхнего уровня с `(instance, target)`.
- `expand_without_overrides(document, instance, load)` раскрывает без overrides одного экземпляра — база для вычисления overrides в редакторе.
- `apply_override` применяет один override к объекту.

**Проверка.**
- Документ без `instances` проверяется как раньше, побайтовый формат сохранения не меняется.
- С `instances` `parse`/`validate` проверяют только локальные правила: формат, объявленный префаб, уникальность id, overrides.
- Существование родителей, циклы, ≥1 камера, ≤1 свет, ≤1 environment и базисы проверяются на раскрытом документе.
- `World::from_document` отклоняет нераскрытый документ и документ без камеры.
- Раскрытый документ (`is_expanded()`) допускает id с точками, но `to_toml` для него — ошибка: сохраняется только авторский документ.

## Схема параметров скриптов

`engine/engine-scene/src/params.rs` (`ParamSchema`, `ParamSpec`, `ParamKind`) описывает параметры, которые объявляет сам скрипт. Сцена хранит только заданные значения; документ схему не знает и не проверяет её.

- Типы `ParamKind`: `bool`, `int`, `float`, `string`, `vec3`, `object`, `component`, `asset`, `enum`, `color` и `array`. Object/component значения хранят id объекта; asset — virtual path. `component` задаёт компонент сцены либо скрипт; `asset` может ограничивать расширение. `enum` требует непустые уникальные строки, `color` — RGB из трёх чисел в `[0, 1]`, `array` рекурсивно проверяет элементы по вложенному типу. `vec3` хранится массивом из трёх чисел.
- Lua объявляет их как `{ type = "object" }`, `{ type = "component", component = "rigid_body" }` или `{ type = "component", script = "scripts/actor.lua" }`, `{ type = "asset", asset = "model" }`, `{ type = "enum", values = { "idle", "run" } }`, `{ type = "color" }` и `{ type = "array", of = { type = "float" } }`. Допустимые расширения ассетов берёт общий `AssetKind`.
- `ParamSchema::new` сохраняет порядок объявлений. Он проверяет:
  - имена — идентификаторы `[A-Za-z_][A-Za-z0-9_]*` до 64 байт без повторов;
  - `min`/`max` — только у `int`/`float`, конечные, `min <= max`;
  - значение по умолчанию — по типу и диапазону.
- `ParamSchema::resolve(params)` даёт значения экземпляра:
  - недостающие ключи получают значения по умолчанию;
  - целое число приводится к `float` и к компонентам `vec3`;
  - проверяются тип и диапазон;
  - необъявленный ключ — ошибка, чтобы опечатка в сцене не терялась молча.
- Проверка типа и расширения выполняется схемой; существование object/component ссылок runtime проверяет после создания `World`. Отсутствующий объект или компонент — ошибка загрузки скрипта.
- Скрипт без схемы получает `params` сцены без проверок. Где схема берётся и когда применяется — [[Engine/EngineScript]].

## Звуковой источник

`audio_source` — необязательный компонент объекта (`engine-scene/src/audio.rs`). Поля: `clip` (обязательный virtual path `.wav` или `.ogg`, объявленный в `[[assets]]`), `group` (`music`, `effects` по умолчанию или `ambient`; `master` здесь запрещён), `volume` (конечное число от 0 до 4 включительно, `MAX_AUDIO_VOLUME`; по умолчанию 1), `looping`, `play_on_start`, `spatial` (пространственное затухание и панорама, по умолчанию выключено), `min_distance` и `max_distance` (`0 < min_distance < max_distance`, по умолчанию 1 и 30). Компонент без `spatial` не пишется в TOML, поэтому сцены без звука не меняются. Проверка клипа и дистанций выполняется при разборе и при каждой правке редактора. Prefab override заменяет компонент целиком и тоже проверяет клип. `World::audio_source(handle)` отдаёт компонент, `World::asset_paths()` — объявленные `[[assets]]` для предзагрузки и предупреждений. Воспроизведение делает [[Engine/EngineAudio]].

## Аниматор

`animator` — необязательный компонент объекта (`engine-scene/src/animation.rs`), схема без состояния проигрывания. Нужен `mesh_renderer` и запрещён вместе с `lod_group`. Поля: `clips` (объявленные `.fbx` в `[[assets]]`, уникальные без учёта регистра имени файла), `default_motion` (имя движения для запуска), `play_on_start` (по умолчанию `true`), `looping` (по умолчанию `true`), `speed` (конечное число не меньше 0, по умолчанию 1) и список `blend_trees` с `name`, `parameter` и `points` (от 1 до 16 точек с `clip` и строго возрастающим конечным `threshold`). Имена tree и параметров — идентификаторы `[A-Za-z_][A-Za-z0-9_]*`; имя tree не совпадает с именем файла клипа, неизвестные поля запрещены. Значения по умолчанию не сериализуются, старые сцены без компонента сохраняются побайтово.

- Существование клипов и модели со скелетом проверяется при загрузке моста, а не в документе: документ не читает FBX.
- `World::animator(handle)` возвращает компонент. Prefab override заменяет компонент целиком, включая клипы.
- `AnimationRequest` (`Play`, `SetWeight`, `SetParameter`, `SetSpeed`, `Stop`) — очередь запросов, которую скрипты ставят в транзакцию; состояние проигрывания живёт в [[Engine/EngineAnimation]].

## Сетевые компоненты

`network_identity` и `network_transform` — необязательные компоненты объекта (`engine-scene/src/network.rs`). Протокол, репликация и лимиты сети описаны в [[Engine/EngineNetwork]].

- `network_identity` задаёт `authority`: `server` (по умолчанию) или `owner`. Значение по умолчанию не сериализуется, поэтому компонент без явного поля пишется как пустая таблица `[objects.network_identity]`.
- `network_transform` требует `network_identity`. Поля: `sync_position` и `sync_rotation` (включены по умолчанию), `sync_scale` (выключен), `send_rate_hz` (1..=60, по умолчанию 20), `interpolate` (включено), `interpolation_delay_ms` (0..=1000, по умолчанию 100) и `snap_distance` (конечное число больше нуля, по умолчанию 5). Синхронизируется локальная трансформация. Значения по умолчанию в TOML не пишутся, поэтому сцены без сетевых компонентов сохраняются побайтово.
- Prefab override заменяет компонент целиком; компоненты допустимы и в spawn-префабе.
- `World::network_identity(handle)` и `World::network_transform(handle)` возвращают компоненты. Проверка документа (`SceneDocument::validate`) отвергает неверные диапазоны и `network_transform` без `network_identity`.
- Сетевой id объекта сцены — `scene_net_id(id)`: BLAKE3 от строкового id с учётом класса. Два объекта с одинаковым сетевым id — ошибка сцены (`validate_scene_net_ids`).
- Общие типы сети (`NetId`, `NetMode`, `NetTarget`, `NetRequest`, `NetEvent`, `NetAuthority`, `PeerId`) и лимиты значений сообщения (глубина, число элементов и суммарный размер строк) находятся в том же модуле. `engine-network` и `engine-script` используют их отсюда, поэтому между ними нет прямой зависимости.

## Runtime-контракты

- `SceneDocument::validate` публичен: [[Engine/RveEditor]] проверяет им документ после каждой правки. Метод применяет те же правила, что `parse`, кроме проверки исходной записи virtual path в TOML.
- Валидация документа и `World` используют общий расчёт: `LocalTransform::matrix`, общий топологический порядок и общие функции извлечения камеры и направления света. Поэтому проверка сцены и `World::build_frame` одинаково трактуют базисы и мировые матрицы.
- Отдельный `Transform::matrix` также задаёт `T · R · S` для столбцовых векторов; default — единичное преобразование.
- Невалидный мировой базис камеры, направления света или rotation environment при загрузке даёт `SceneError`. Во время игры сохраняется последнее валидное состояние; `SceneWarning` называет компонент и возвращается один раз на объект. Библиотека сама не пишет предупреждение в лог.
- Без directional-компонента `LightState` имеет направление `[0, -1, 0]`, чёрный цвет, ambient `[1, 1, 1]` и выключенные тени. Направление лучей directional light нормализовано из мирового −Z. `SceneFrame.local_lights` содержит отдельные point/spot состояния: позиция — перенос полной мировой матрицы объекта, направление spot — нормализованный мировой −Z, преобразованный полной матрицей (включая родительские, неравномерные и отрицательные масштабы). При невалидной мировой позиции/оси во время игры остаётся последнее валидное состояние и выдаётся `SceneWarning`; первоначально невалидный компонент отвергается при проверке документа.
- `World::active_camera_cursor()` возвращает флаги `[objects.camera.cursor]` активной камеры; без camera component — значения по умолчанию. Флаги применяются только при старте сцены; камера, которую выбрал скрипт позже, их не применяет.
- `World::from_document` хранит редактируемые локальные трансформации и handles с поколением 0 в порядке документа. `WorldObject` также предоставляет physics components без запуска симуляции. `World::set_root_pose` принимает позицию и quaternion только для корня, переводит вращение в градусы Euler YXZ, сохраняет scale и сразу пересчитывает мировые матрицы детей; некорневой handle даёт `SceneError`. `World::world_rotation` читает кэш мировых quaternion независимо от scale, в том числе при нулевом масштабе. `World::set_camera_settings` проверяет и назначает настройки конкретной камеры; `camera_settings`, `active_camera` и `set_active_camera` обслуживают доступ и переключение. `build_frame` пересчитывает матрицы родитель→потомок и возвращает активную камеру, её `view`-настройки, directional light, `SceneFrame.local_lights`, необязательный `EnvironmentState` и мировые матрицы объектов с `mesh_renderer`. Environment rotation берётся из мирового quaternion владельца; направление к солнцу — противоположно нормализованному направлению лучей единственного directional light, если он есть. Нулевой масштаб объекта без зависимого базиса (например, mesh) допустим.
- **Хендлы.** `ObjectHandle` — индекс слота и поколение. Освобождение слота (`despawn_many`, `release_reservation`) увеличивает поколение, поэтому устаревший хендл не живёт даже после переиспользования слота: `is_alive` даёт `false`, методы с ним возвращают `None`/ошибку. Поколение ограничено `MAX_GENERATION` (2^31 − 1); исчерпанный слот больше не переиспользуется. ABI-значение — `to_abi()` = `(generation << 32) | (index + 1)`, ноль означает «нет объекта»; `from_abi` декодирует его без проверки живости, её делает `World::is_alive`.
- **Резерв.** `reserve_object(&self)` берёт слот через `RefCell`, поэтому транзакция скриптов работает с `*const World`. `World` остаётся `Send`, но не `Sync`. `release_reservation` возвращает слот с новым поколением.
- **Шаблон.** `PrefabTemplate::load(path, read_text)` раскрывает префаб через синтетический экземпляр и ошибку возвращает только для нечитаемого, неразобранного или структурно неверного префаба. Объекты хранятся с id относительно корня (корень — пустой, дети — `a.b`). `mesh_collider_models()` даёт пути моделей мешевых коллайдеров шаблона, а `document::mesh_collider_model` — общее правило выбора такой модели для сцены, префабов и редактора. Допуск к spawn отдельно проверяет `PrefabTemplate::check_spawnable()`: в spawn-префабе не должно быть `camera`, `directional_light` и `environment`. `World::spawn` отказывает шаблону без допуска. Dynamic rigid body и character controller допустимы только у корня (это правило валидации документа).
- **Spawn.** `World::spawn(reserved, template, SpawnOptions)` создаёт объекты шаблона на зарезервированных хендлах (по одному на объект, в порядке шаблона) и возвращает их. Новые объекты дописываются в конец `objects()`, их матрицы пересчитываются. Id — явный `SpawnOptions.id` в авторском формате `[A-Za-z0-9_-]{1,64}` или сгенерированный `<stem>@<serial>`, который пропускает номера, занятые живыми объектами; дети получают `<корень>.<id>`, раскрытый id не длиннее 255 байт. Отказ проверяет формат и коллизию явного id с живыми объектами, существование родителя, dynamic/character только без родителя, конечность transform и лимит `MAX_LIVE_OBJECTS` (65 535). При отказе резервы освобождаются, мир не меняется. Резервы без id `World` не видит, поэтому коллизии с ними проверяет вызывающая сторона.
- **Despawn.** `World::subtree(roots)` возвращает живые объекты поддеревьев, включая корни, в порядке «родитель раньше потомка» одним проходом по топологии; мёртвые хендлы пропускает. `World::despawn_many(handles)` удаляет поддеревья и возвращает удалённые хендлы: дети раньше родителей. Отказывает для мёртвого хендла и если в поддереве активная камера; при отказе мир не меняется. Плотные массивы уплотняются за один проход, порядок `objects()` сохраняется. Удаление directional light или environment сбрасывает их последнее валидное состояние.
- **Лимит экземпляров.** `SceneAsset.max_instances` допустим только для `.rveprefab` и больше нуля; при отсутствии не сериализуется. Применяет его runtime, сам `engine-scene` лимит не считает. При раскрытии записи `[[assets]]`, пришедшие из вложенных префабов, не переносят лимит: действует только запись самого документа.
- `LodGroup::level_for_distance` выбирает следующий уровень при `distance >= threshold` и возвращает `None` при достижении `cull_distance`. `World::lod_level(handle, eye)` использует евклидово расстояние от камеры до мировой позиции объекта (`world_matrix.w_axis`), не учитывая масштаб; объект без группы всегда получает уровень 0.
- `Camera` использует правую систему координат и Y-up. `view` переводит мировое пространство в камеру; `projection` использует глубину `[0, 1]`. `Camera::fov_narrow` измеряется в радианах по короткой стороне; нулевая ширина или высота окна трактуется как 1. Для определённого view нужны конечные `position`, `target` и `up`, ненулевое направление взгляда и ненулевой up, не параллельный взгляду; для projection — конечные `0 < fov_narrow < π` и `0 < near < far`. При нарушении прямые методы камеры возвращают матрицу с NaN, не паникуя.
- `OrbitCamera::camera_at(t)` задаёт позицию `target + (r·sin(ωt), height, r·cos(ωt))`. При `t = 0` камера находится со стороны `+Z`; положительная скорость смещает её к `+X`, то есть вправо относительно направления взгляда.
- `sun_direction(elevation, azimuth)` возвращает единичное направление лучей с `y = −sin(elevation)`. `OrbitingSun` меняет азимут со временем; отрицательная скорость вращает в сторону, противоположную положительной орбите камеры. `cascade_view_projections(camera, aspect, direction, settings, resolution)` строит до четырёх каскадов по practical splits (`lambda = 0.75`) от camera near до `min(shadow_distance, camera.far)`. Сфера покрывает каждый frustum slice; её центр привязан к texel-сетке в плоскости света, а диапазон глубины оставляет место для caster-ов со стороны источника. Невалидные входы или интервал, не достигающий camera near, дают пустой список. Сцена не зависит от renderer; вызывающий код передаёт совпадающее разрешение карты.
- **Иерархия и мировые трансформации.** `World::set_parent(handle, parent, keep_world)` перестраивает топологический порядок и отказывает для циклов, для родителя-самого объекта, для dynamic `rigid_body` и `character_controller` (они остаются корнями) и для скрытия поддерева с активной камерой. `keep_world` пересчитывает локальную трансформацию по мировой матрице; при неравномерном масштабе родителя с поворотом результат округляется. `set_world_position` и `set_world_rotation` вычисляют локальную трансформацию относительно родителя по текущим локальным значениям. Мировые геттеры и сеттеры не читают кэш кадра.
- **Активность.** `active_in_hierarchy` означает собственный флаг и флаги всех предков. `build_frame` не выводит меши, частицы, точечный и прожекторный свет, направленный свет и environment неактивных объектов; камера от активности не зависит, а скрыть поддерево с активной камерой нельзя. Стартовая активная камера выбирается по действующей активности: камера под неактивным предком не выбирается. `take_activity_changes()` возвращает объекты, у которых действующая активность изменилась с прошлого вызова; по ней runtime и редактор Play создают и удаляют тела физики и останавливают звук. `find_by_name`, `find_all_by_name`, `find_with_tag` и `find_all_with_tag` находят и неактивные объекты.
- **Очередь правок.** `ObjectEdit` (активность, родитель, мировые позиция и поворот, тег, слой) — изменения мира, которые транзакция скрипта ставит в очередь. `apply_edit` применяет их по порядку; транзакция проверяет всю очередь до записи, поэтому отказ любой правки отбрасывает весь вызов. Копия мира нужна только для правок активности, родителя и камеры и если предок позируемого объекта получает трансформ в этом же вызове; тег, слой и мировая поза проверяются на текущем мире `check_world_position` / `check_world_rotation` без клонирования.

## Зависимости и размещение

`engine-scene` зависит от glam, engine-files, blake3, serde и toml. `engine-render` не зависит от него и не принимает типы glam; runtime и скрипты также не являются зависимостями сцены. `engine-runtime` загружает `.rvescene` в `World` для `rve-player`; sandbox сохраняет собственную процедурную демонстрационную сцену. Поток загрузки игры описан в [[Engine/EngineRuntime]].

## Эмиттер частиц

Компонент `[objects.particle_emitter]` описан в `engine-scene/src/particles.rs`. Вложенные таблицы: `shape` (внутренне тегированный enum `point`, `sphere`, `cone`, `box`), `flipbook`, `collision`, массивы `bursts`, `color_over_life` (до четырёх ключей, альфа в [0, 1]) и `size_over_life`. Неизвестные поля отклоняются на каждом уровне. Пустые опциональные таблицы не сериализуются, поэтому сцены без частиц не меняются. Ёмкость проверяется по blend: `MAX_ADDITIVE_PARTICLE_CAPACITY = 262144`, `MAX_ALPHA_PARTICLE_CAPACITY = 65536`. Константы совпадают с `engine-render`, согласованность проверяет тест в engine-runtime. Текстура обязана быть PNG, объявленным в `[[assets]]`.

Контракты: `SceneObject.particle_emitter` и `WorldObject.particle_emitter` (accessor `World::particle_emitter`); `PrefabChange.particle_emitter` (override заменяет компонент целиком); `SceneFrame.particle_emitters: Vec<ParticleEmitterInstance { object, model }>` в порядке объектов сцены. Spawn-объекты с эмиттером попадают в этот список наравне с объектами документа; компонент допустим в spawn-префабе.) Симуляции здесь нет: шаги и отрисовка — в [[Engine/EngineRuntime]] и [[Engine/EngineRender]].
