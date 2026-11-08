# Sandbox

Parent: [[Index]]

## Назначение

`sandbox` — оконное приложение и ручная проверка рабочего пути RVE: время, окно, рисование мешей с текстурой и светом через Vulkan, загрузка моделей FBX и текстур PNG через фоновый сервис ресурсов, аргументы запуска и изменение размера.

## Источники

| Файл | Значимый символ | Роль |
|---|---|---|
| `engine/sandbox/src/main.rs` | `parse_args`, `load_content`, `Sandbox`, `WindowHandler`, `main` | Подготовка Vulkan-окружения до потоков, разбор CLI, загрузка ресурсов, инициализация renderer, кадры и коды завершения |
| `engine/sandbox/src/content.rs` | `Content`, `Mesh`, `local_light_shadows` | Меши, материалы и текстуры кадра из демонстрационной сцены или модели; point/spot shadow matrices showcase, вписывание модели, окружение и замена base-color map |
| `engine/sandbox/src/scene.rs` | `Scene`, `plane_mesh`, `materials_plane_mesh`, `cube_mesh`, `material_sphere_meshes`, `sinusoidal_normal_texture`, `checker_texture`, `triangle_view_projection`, `cube_view_projection`, `shadows_light_view_projection`, `materials_light_view_projection` | Геометрия и параметры демонстрационных сцен; камеры и свет используют [[Engine/EngineScene]] |
| `engine/sandbox/src/debug_overlay.rs` | `DebugOverlay` | Метрики `--debug-overlay`, их история и геометрия панели со шрифтом font8x8 |
| `engine/engine-project/src/bootstrap.rs` | `BootstrapConfig::{parse, load}` | Общий разбор и загрузка начальных настроек `bootstrap.rves` |
| `bootstrap.rves.example` | `WindowSizeX`, `WindowSizeY`, `WindowTitle`, `AllowMultipleInstances` | Шаблон локального файла настроек |
| `engine/sandbox/Cargo.toml` | зависимости `engine-core`, `engine-files`, `engine-project`, `engine-platform`, `engine-window`, `engine-render`, `engine-assets`, `engine-resources`, `engine-scene`, `glam`, `sysinfo`, `font8x8` | Сборка приложения из рабочих крейтов, подготовка Vulkan-окружения, сцена, загрузка ресурсов, статистика и bitmap-шрифт оверлея |

## Публичные контракты

У бинарного крейта нет публичной Rust API. Командная строка:

| Аргумент | Поведение |
|---|---|
| `--frames N` | Выход после N вызовов презентации со статусом `Presented`; N — положительный `u64` |
| `--render-scale S` | Масштаб render resolution; конечное число в `(0, 1]`, передаётся в `Frame::view` |
| `--camera-settings /путь.camerasettings` | Загружает TOML-настройки проекции и вывода из assets; требует расширение `.camerasettings` |
| `--resize-test` | Запускает сценарий изменения размера и сворачивания/восстановления |
| `--scene triangle\|cube\|shadows\|materials\|clustered` | Демонстрационная сцена; по умолчанию `shadows` |
| `--model /путь.fbx` | Модель FBX из `assets`; сочетание допустимо со сценами `cube`, `shadows` и `materials`, но не с `triangle` |
| `--texture /путь.png` | PNG из `assets`, заменяющий base-color map всех мешей сцены или модели |
| `--environment procedural\|/путь.hdr` | Использует procedural sky либо Radiance `.hdr` из `assets` для IBL и skybox; поддерживает end-to-end проверку с `--resize-test` |
| `--debug-overlay` | Панель метрик в левом верхнем углу окна: FPS, память, CPU, GPU, средние CPU-фазы и GPU-проходы первого вида за 500 мс |

Повторные или неизвестные флаги, отсутствие значения, некорректное значение и сочетание `--scene triangle --model …` дают код 2. Ошибка запуска `ResourceSystem`, загрузки содержимого (`AssetError`) или разбора `.camerasettings` выводится до создания окна и даёт код 2. Это относится и к повреждённым архивам, отсутствующим владельцам и дублирующим virtual paths при процедурной сцене. При сочетании `--resize-test` и `--frames` требуется N ≥ 120. Ошибка окна или renderer даёт код 1; успешное закрытие или завершение лимита — код 0.

Перед созданием окна sandbox загружает `BootstrapConfig` из `bootstrap.rves` в cwd
(обычно `build/`). Он применяет размер и заголовок окна; `AllowMultipleInstances` и
`AppIcon` разбираются, но sandbox их не использует. Поля, defaults, ошибки и пути
bootstrap для остальных потребителей описаны в [[Engine/EngineProject]]. Ошибка
настроек завершается кодом 2 до создания окна. Шаблон хранится в репозитории,
локальный `build/bootstrap.rves` игнорируется Git и не перезаписывается приложением.

## Зависимости и взаимодействия

- Первая операция `main` получает канонический `exe_dir` и вызывает `unsafe engine_platform::prepare_vulkan_environment` с SAFETY-условием: до этого не разбираются аргументы, не запускаются ресурсы и не создаются потоки. При отсутствии bundled-файлов вызов ничего не меняет.
- Использует [[Engine/EngineCore]] для `Time`.
- Через [[Engine/EngineWindow]] реализует `WindowHandler`.
- После проверки аргументов и `bootstrap.rves` запускает [[Engine/EngineResources]] с корнем `ASSETS_DIR` (`assets` в рабочем каталоге); верхнеуровневый `.rvepak` выбирает packed-only режим.
- Через [[Engine/EngineAssets]] и `Assets::with_resources` загружает модель и заменяющую текстуру до запуска цикла окна. FBX/PNG декодируются на startup-потоке; `ResourceSystem` остаётся жив после уничтожения `Sandbox` и renderer. Настройки камеры читаются тем же ресурсным сервисом и проверяются как `.camerasettings`.
- Через [[Engine/EngineScene]] получает матрицы камеры, модели и света для выбранной сцены; FOV и clipping planes камеры сцены `shadows` берутся из `CameraSettings::projection`, а showcase `materials` сохраняет собственную проекцию.
- Через [[Engine/EngineRender]] создаёт Vulkan renderer, загружает меши, sRGB base-color textures, linear metallic-roughness/normal textures, immutable материалы и выбранный environment, затем вызывает `render_frame` с `FrameCamera { view, projection, position }`, optional `FrameEnvironment`, directional/local lights, `LocalLightShadow` matrices, HDR clear color, per-camera `ViewSettings` и `DrawCommand` с `MaterialId` на меш. `Content::local_light_shadows` использует матричные функции [[Engine/EngineScene]] для point/spot showcase. `--environment procedural` создаёт тестовое процедурное небо; путь `/...hdr` загружает Radiance panorama. Объект получает матрицу движения и fit, окружение — identity. С `--debug-overlay` также передаются вершины панели в `Frame::overlay`, иначе пустой срез.

## Ограничения и важные детали

- `--debug-overlay`: полупрозрачная чёрная панель в физических пикселях (без учёта DPI). Сверху вниз: FPS крупным шрифтом и график за 30 с; память процесса (RSS / working set) и график за 2 мин; загрузка CPU процессом, название CPU и график за 30 с; доля GPU-времени clustered compute, shadow, scene и tonemap passes ([[Engine/EngineRender]] `take_gpu_frame_time`), название GPU из `Capabilities` и график за 30 с. Выборка и перестройка геометрии — раз в 0,5 с на главном потоке; sysinfo обновляет только текущий процесс. FPS считает только `FrameStatus::Presented`. `Process::cpu_usage` sysinfo суммирует ядра, поэтому делится на число логических CPU. Подписи латиницей шрифтом 8×8 (font8x8); символы вне ASCII заменяются на `?`, имена устройств обрезаются до 36 символов. Источники метрик (память и CPU процесса, GPU по timestamps) и латиница выбраны пользователем. Output FXAA/upscale и UI не входят в timestamp.
- Под графиками выводятся средние за 500 мс CPU-фазы renderer и GPU-проходы первого вида (`take_frame_profile`) в миллисекундах; `n/a` — значения в окне не было. Блок профиля в два столбца (CPU слева, GPU справа), чтобы панель помещалась в окно 1280×720.
- По умолчанию создаётся окно `RVE Sandbox` размером 1280×720 и применяются значения `CameraSettings::default()` (`render_scale = 1.0`, clear color `[0.1, 0.1, 0.12]`, EV `0`, ACES и FXAA). `.camerasettings` задаёт также FOV и clipping planes орбитальной камеры `shadows`; явный `--render-scale` перекрывает только масштаб из файла.
- Loose assets читаются через worker pool; при наличии `.rvepak` loose fallback отсутствует. Ошибка каталога завершается до окна, даже когда сцена не запрашивает asset. Ожидание `ResourceRequest` происходит только во время startup, не в обработчиках окна или кадра.
- Сборка и запуск выполняются из `build/`; debug-бинарник находится в `build/debug/`. При инициализации sandbox сообщает фактический физический размер окна.
- Угол поворота, камера и свет используют одно значение `Time::elapsed()` на кадр; clear color задаётся camera settings.
- Сцена `triangle` сохраняет прежнее поведение: треугольник вращается вокруг оси Z, масштабируется по более узкой стороне окна и рисуется цветом вершин без текстуры. Сцена `cube` также сохраняет прежние камеру, направленный свет и вращение вокруг осей Y и X. Их матрицы и параметры света не меняются; тени выключены.
- По умолчанию создаётся окно `RVE Sandbox` размером 1280×720 и применяются значения `CameraSettings::default()` (`render_scale = 1.0`, clear color `[0.1, 0.1, 0.12]`, EV `0`, ACES и FXAA; для `shadows` FOV 60°, near 0.1, far 1000). `.camerasettings` задаёт проекцию камеры `shadows` и render/color settings для всех сцен; showcase `materials` сохраняет FOV 45°, near 0.1 и far 100. Явный `--render-scale` перекрывает только масштаб из файла.
- Loose assets читаются через worker pool; при наличии `.rvepak` loose fallback отсутствует. Ошибка каталога завершается до окна, даже когда сцена не запрашивает asset. Ожидание `ResourceRequest` происходит только во время startup, не в обработчиках окна или кадра.
- Сборка и запуск выполняются из `build/`; debug-бинарник находится в `build/debug/`. При инициализации sandbox сообщает фактический физический размер окна.
- Угол поворота, камера и свет используют одно значение `Time::elapsed()` на кадр; clear color задаётся camera settings.
- Сцена `triangle` сохраняет прежнее поведение: треугольник вращается вокруг оси Z, масштабируется по более узкой стороне окна и рисуется цветом вершин без текстуры. Сцена `cube` также сохраняет прежние камеру, направленный свет и вращение вокруг осей Y и X. Их матрицы и параметры света не меняются; тени выключены.
- Сцена `shadows` используется по умолчанию. Куб со стороной 1 вращается вокруг мировой оси Z со скоростью 1 рад/с; плоскость размером 2×2 м неподвижна на `y = −1`. Камера смотрит в начало координат с горизонтального радиуса 4.2 м и высоты 1.5 м, делает оборот за 20 с; FOV/near/far берутся из `.camerasettings` или `CameraSettings::default()`. Белое солнце высотой 55° вращается в противоположную сторону за 12 с; ambient равен 0.15. Кроме четырёх directional cascades, warm point и cool spot lights запрашивают локальные тени и отбрасывают их на геометрию сцены. Local map имеет 32 слоя 1024×1024 D32 (128 MiB); общий бюджет выбирает ближайшие источники, point стоит 6 слоёв, spot — 1. Запуск `cargo run -p sandbox -- --scene shadows --frames 300 --resize-test --debug-overlay` проверяет local shadows, resize/restore и показывает timestamp всех shadow/scene passes, если GPU поддерживает timestamps.
- Сцена `materials` показывает два ряда UV-сфер (dielectric и metallic) с roughness от 0 до 1 шагом 0.25. Сферы неподвижны; камера и белое солнце вращаются вокруг них, ambient равен 0.15, shadow map охватывает сцену радиусом 5.9 м. Под сферами лежит плоскость 8×8 м с процедурной синусоидальной tangent-space normal map 128×128; текстура строится при запуске без binary asset.
- Сцена `clustered` использует ту же компактную procedural geometry и 160 видимых point/spot lights вокруг центра, включая группу из 144 совпадающих point lights для заполнения одного cluster сверх лимита 128. Локальные тени отключены, чтобы сохранить прежний stress-набор освещения без дополнительных shadow draws. Запуск `cargo run -p sandbox -- --scene clustered --frames 300 --resize-test --debug-overlay` проверяет кластеризацию, resize/restore и отображает GPU timestamp, который включает compute pass.
- Куб содержит 24 вершины с внешними нормалями и UV-развёрткой на каждую грань; его демонстрационная текстура — процедурная шахматная 256×256 с клетками 32×32. Модель FBX получает белый цвет вершин и fit к кубу со стороной 1 с центром в начале координат. В сценах `cube`, `shadows` и `materials` модель использует движение выбранной сцены; в сценах с плоскостью она добавляется как окружение без fit и движения. `--texture` заменяет base-color map всех мешей, включая плоскость.
- Перспектива использует соглашения [[Engine/EngineScene]]: Y вверх и глубину `[0, 1]` для renderer. Размер окна для сцен `triangle` и `cube` берётся из `init` и `resized`; для `shadows` и `materials` используются орбитальные камеры.
- Resize test после 30 презентаций запрашивает физический размер 960×540; после 60 успешных презентаций сворачивает окно и восстанавливает его по wall-clock таймеру 250 мс, запрашивая 1280×720.
- Фактический размер из `request_inner_size` обрабатывается сразу, если winit его возвращает; иначе сценарий ждёт событие `Resized`. Сценарий проверяет фактические размеры и завершение восстановления.
- При свёрнутом окне sandbox не вызывает `render_frame`. Презентация засчитывается только для `FrameStatus::Presented`; `Skipped` не увеличивает счётчик `--frames`.
- `WindowHandler::exiting` удаляет renderer до освобождения окна. Ошибка сохраняется до возврата из `run`, чтобы после цикла определить код выхода.
