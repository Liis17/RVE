# EngineAnimation

Parent: [[Index]]

## Назначение

`engine-animation` — состояние скелетной анимации: позы, клипы, сопоставленные с костями по имени, crossfade, ручные веса, одномерные blend tree, разовые клипы с переходом и палитры skinning. Крейт не владеет GPU и не читает файлы: модели и клипы приходят в `AnimationBridge::build` через замыкания, поэтому один и тот же код работает в runtime, в Play редактора и в превью.

Состояние проигрывания не хранится в `World`. Сцена хранит только схему компонента `animator` ([[Engine/EngineScene]]); запросы скриптов уходят через транзакцию и применяются мостом.

## Источники

| Файл | Значимый символ | Роль |
|---|---|---|
| `engine/engine-animation/src/lib.rs` | `VERSION` | Публичные реэкспорты, версия крейта `0.1.0` |
| `engine/engine-animation/src/skeleton.rs` | `Skeleton`, `SkeletonNode`, `Joint` | Узлы в порядке родитель-первым и палитры joints |
| `engine/engine-animation/src/clip.rs` | `Clip`, `Track`, `Key` | Привязка треков FBX к узлам по имени и сэмплинг времени |
| `engine/engine-animation/src/pose.rs` | `Pose`, `Transform`, `skin_matrices`, `world_matrices` | Локальные TRS, композиция world с inherit modes и `inverse_bind` |
| `engine/engine-animation/src/blend.rs` | `blend`, `line_weights` | Смешивание поз и веса одномерного дерева по порогам |
| `engine/engine-animation/src/animator.rs` | `Animator`, `Motion`, `MotionSource`, `PlayOptions`, `MotionEvent`, `AnimatorError` | Проигрывания, crossfade, one-shot, `seek` и параметры дерева |
| `engine/engine-animation/src/bridge.rs` | `AnimationBridge`, `AnimationEvent` | Аниматоры объектов `World`, запросы, события и палитры |
| `engine/engine-animation/tests/oracle.rs` | — | Сверка поз и skinned-вершин с `ufbx::evaluate_scene` (dev-зависимость `ufbx`) |

## Публичные контракты

| Контракт | Поведение и условия |
|---|---|
| `Skeleton::from_model(&ModelSkeleton)` | Строит узлы и joints из [[Engine/EngineAssets]]; `node_index(name)` ищет узел по имени |
| `Clip::bind(&ModelAnimation, &Skeleton)` | Сопоставляет треки с узлами по имени. Неизвестные треки не участвуют и попадают в `unmatched()`; узлы без трека остаются в позе покоя |
| `Clip::sample(time, looping, &mut Pose)` | Линейная интерполяция T/S, shortest-path интерполяция R; время зажимается или переносится по длительности |
| `Pose::rest`, `Pose::skin_matrices(&Skeleton, out)` | Мировые матрицы узлов в порядке родитель-первым, затем `node_world × inverse_bind` для каждого joint. Режимы `IgnoreParentScale` и `ComponentwiseScale` учитываются при композиции |
| `blend(inputs, out)`, `line_weights(thresholds, value)` | Нормализованные веса; знаки кватернионов выравниваются к первой позе. Значение вне порогов зажимается к крайней точке, между соседями вес линейный |
| `Animator::new(clips, motions, default_looping)` | Проверяет непустые уникальные имена, индексы клипов, от 1 до 16 точек в tree и строго возрастающие конечные пороги |
| `Animator::play(motion, PlayOptions { fade, looping, then })` | Цель 1 у выбранного движения и 0 у остальных за `fade` секунд; `fade = 0` применяется сразу. Движение, уже проигрывающееся, возвращает цель 1 даже после ручного веса. `looping: None` берёт значение компонента. Ошибка имени не меняет состояние |
| `Animator::set_weight(motion, weight, fade)` | Ручной вес одного движения, зажатый в `[0, 1]`. С `fade = 0` снапает только это движение: переходы остальных продолжают идти |
| `Animator::set_parameter(name, value)`, `set_speed(speed)` | Параметры blend tree; скорость конечна и не отрицательна |
| `Animator::seek(motion, time)` | Переводит проигрывающееся движение на `time`: цикл переносит время, нецикличное зажимается. Отмотка назад снова включает событие окончания |
| `Animator::stop(fade)` | Веса уходят к нулю за `fade`; при `fade = 0` проигрывания удаляются сразу |
| `Animator::advance(dt) -> Vec<MotionEvent>` | Продвигает время и веса; возвращает `MotionEvent::Finished { motion }` для каждого окончания |
| `Animator::sample(&Skeleton, &mut Pose)` | Смесь активных движений; недостающий вес добирается позой покоя |
| `AnimationBridge::build(world, load_model, load_clips)` | Создаёт аниматор для каждого объекта с `animator` и `mesh_renderer`, если модель имеет скелет. Ошибки клипов и моделей становятся предупреждениями, объект остаётся статическим |
| `AnimationBridge::apply(requests)` | Применяет `AnimationRequest` (`Play`, `SetWeight`, `SetParameter`, `SetSpeed`, `Stop`). Запросы к объектам без компонента `animator` игнорируются. Объект с компонентом, чей аниматор не собрался, даёт предупреждение `аниматор не собран` один раз; ошибки запросов тоже уходят в предупреждения |
| `AnimationBridge::advance(dt) -> Vec<AnimationEvent>` | Продвигает все аниматоры, пересчитывает палитры и возвращает события в порядке объектов сцены |
| `AnimationBridge::palette(object)`, `animator(object)`, `animator_mut(object)`, `has_animator(object)` | Палитра `[[f32;4];4]` по joints; доступ к аниматору для редактора (перемотка превью) |
| `AnimationBridge::take_warnings()` | Забирает предупреждения. Одинаковое сообщение попадает в список один раз до следующего забора, общий лимит 32 |

## Модель проигрывания

- Каждое движение — `Playback` с весом, целью, временем, `fade`, режимом цикла, `then` и флагом окончания.
- Повторный `play` уже идущего движения сохраняет время; запуск после окончания начинает с нуля.
- Нецикличное движение доходит до конца, замирает на последнем кадре и один раз выдаёт `Finished`. Если задано `then`, после окончания запускается следующее движение с тем же `fade`.
- Одномерное дерево: вес каждого ребёнка берётся из `line_weights`. Дети сэмплируются по общей нормализованной фазе (время / средняя длительность точек), поэтому ноги и руки остаются синхронными. Нецикличное дерево зажимается в конце.
- Палитра строится из смеси активных движений и позы покоя; при весе 0 у всех движений возвращается поза покоя.

## Поток мостов

- Runtime: мост создаётся на потоке скриптов из GPU-таблицы моделей и клипов. Каждый тик: `apply(drain_animation_requests)`, затем `advance(dt)`, затем `queue_animation_events` — события доходят скриптам в следующем `update` как сообщения `animation_finished` (см. [[Engine/EngineScript]]).
- Play редактора: мост строится при старте сессии из проектных ассетов и живёт в `PlaySession`, порядок тот же, что у runtime.
- Превью редактора: `project_preview_bridge` сразу останавливает все аниматоры, поэтому без выбора в инспекторе показывается поза покоя. Клип запускает только превью; документ и `play_on_start` не меняются. См. [[Engine/RveEditor]].

## Зависимости и взаимодействия

- Зависит от [[Engine/EngineAssets]] (`ModelSkeleton`, `ModelSkin`, `ModelAnimation`), [[Engine/EngineScene]] (`AnimatorComponent`, `AnimationRequest`, `World`, `ObjectHandle`), `engine-files`, glam. `ufbx` — только dev-зависимость для оракула.
- Потребители: [[Engine/EngineRuntime]] и [[Engine/RveEditor]]. Палитры уходят в [[Engine/EngineRender]] через `Frame::joint_palettes` и `SkinBinding`.
- Крейт не зависит от renderer, окна и script runtime.

## Ограничения

- Blend tree только одномерный, не более 16 точек; дерева из деревьев нет.
- Нет IK, масок костей, слоёв, state machine и событий, привязанных ко времени внутри клипа.
- Клипы читаются только как TRS-ключи узлов. Морф-таргеты не поддерживаются.
- Аниматор несовместим с `lod_group`: skinning применяется только к уровню 0 (см. [[Engine/EngineRender]]).
- Состояние проигрывания не сохраняется между запусками; превью живёт только в редакторе.

## Решения и основания

- Отдельный крейт, а не часть `engine-scene` или `engine-assets`: математика (`glam`) и состояние проигрывания отделены от схемы сцены и от данных FBX. Решение пользователя (план, этап 2).
- Состояние аниматора не хранится в `World`, как и состояние звука: сцена остаётся схемой, а запросы скриптов применяются мостом. Поэтому Undo/Redo сцены не откатывает проигрывание.
- Клипы из модели и из отдельных FBX сопоставляются по имени узла. Так работает процесс Mixamo: клип в своём файле, скелет в модели. Непопавшие треки становятся предупреждением, а не ошибкой.
- Палитра строится по joints, а не по узлам: один узел может быть joint с разными `inverse_bind` в разных частях меша.
- Оракул `ufbx::evaluate_scene` проверяет позы, skinned-вершины и inherit modes в нескольких моментах времени; ручная математика без него не считается проверенной.

## Замена сцены

`AnimationBridge::replace_objects(world, removed, added, load_model, load_clips)` снимает аниматоры удалённых объектов и собирает аниматоры добавленных по их хендлам; состояние остальных аниматоров (клипы, время, палитры) не меняется. Используется заменой сцены в runtime ([[Engine/EngineRuntime]]).
