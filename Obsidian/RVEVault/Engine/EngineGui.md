# EngineGui

Parent: [[Index]]

## Назначение и границы

`engine-gui` — renderer- и window-независимое ядро игрового экранного UI. Оно разбирает TOML `.rveui`, вычисляет layout, хранит состояние виджетов, обрабатывает нормализованный ввод и строит premultiplied-sRGB draw list. `engine-script` загружает документы/assets, связывает их со скриптами и передаёт ввод; `engine-ui::game::GameUi` управляет UI GPU-ресурсами и формирует primitives для `engine-render`. Ядро не зависит от egui, winit, Vulkan или renderer.

## Документ и layout

Документ содержит `[canvas]`, объявленные `[[assets]]` и `[[elements]]`. Схема строго отклоняет неизвестные поля, требует ровно один виджет на элемент, проверяет id, ссылки и циклы, конечность чисел, диапазоны и допустимые сочетания. Пути изображений и шрифтов должны быть перечислены в assets самого `.rveui`; пути канонизируются через `VirtualPath`, поддержаны PNG, TTF и OTF. Виджеты: panel, image, text, button, toggle, slider, progress bar, input field, scroll view, grid, vertical/horizontal layout group и dropdown.

`CanvasSettings` задаёт слой, constant либо screen scaling, reference resolution, вес подгонки по ширине/высоте и default focus. Constant scale задаёт физические пиксели на единицу канваса; screen scale вычисляется относительно reference resolution логарифмическим смешиванием width/height ratios. Координаты канваса имеют начало у левого верхнего угла, Y направлена вниз.

`RectSpec` задаёт для каждой границы пару `anchor + offset` относительно соответствующей границы родителя: `min = parent.min + anchor_min * parent.size + offset_min`, `max = parent.min + anchor_max * parent.size + offset_max`. Растянутые и точечные якоря используют ту же формулу. `pivot` не участвует в разрешении layout; он определяет точку, относительно которой инспектор преобразует позицию и размер в offsets при перемещении/resize. Порядок элементов задаёт отрисовку: родитель до потомков, соседи в document order. ScrollView пересекает clip области с ограничениями предков.

## Runtime-состояние, ввод и события

`UiSystem` владеет открытыми канвасами, generation-checked handles, layout, focus/hover, popup, изменяемыми значениями и состояниями видимости/interactability. Канвасы с большим layer рисуются поверх меньших и первыми получают pointer; при одинаковом layer позже открытый канвас выше. Сценовый `ui_canvas.layer`, если задан, заменяет layer документа; его `visible` задаёт начальную видимость.

`UiInput` принимает указатель в физических пикселях, колесо, текстовые события и абстрактные навигационные actions. `UiClaims` сообщает, обработал ли UI primary pointer, wheel, actions, держит ли focus или текстовый focus. `InputCapture` скрывает поглощённые нажатия от игровой логики до соответствующего release; нажатие, начатое игрой, остаётся видимым и при последующем захвате, а колесо скрывается только в кадре, когда UI его обработал. Текстовый focus скрывает новые клавиатурные нажатия на время редактирования; любой UI focus захватывает навигационные действия и левый стик пада. Нормализацию платформенных клавиш и lifetime capture выполняет `engine-script`, не `engine-gui`.

Событие содержит canvas/element handles, id элемента, kind и optional value. Runtime-связка `engine-script` регистрирует заранее загруженные документы; компонентные canvas открываются до `on_start`, а Lua `rve.ui.open` принимает только зарегистрированный документ. События UI ставятся в очередь после шага интерфейса и доставляются на следующем update только активным экземплярам скриптов объекта-владельца. Lite Lua имеет `on_ui_event`; ABI Rust — nullable callback с v16. Модули ABI v3–v15 загружаются без него; для UI events их нужно пересобрать с ABI v16 или новее.

API скрипта доступен только внутри вызова скрипта. Чтение UI допустимо и в `create`/top-level Lua chunk; запись запрещена в этих фазах. Записи UI транзакционны: при ошибке состояние существующих canvas и focus восстанавливаются, canvas, открытые неуспешным вызовом, закрываются, а созданные им события отбрасываются. Закрытие применяется на commit; в оставшейся части вызова закрываемый canvas уже недоступен. Ошибка callback обрабатывается общим правилом host и отключает упавший экземпляр скрипта.

## Paint и renderer

Paint собирается в batches по текстуре и clip rect; изображения используют заявленные PNG, текст — общий glyph atlas. Вершины имеют экранные координаты в физических пикселях. Пока UI не менялся, `paint()` возвращает те же `Arc` draw list и atlas без повторной тесселяции. Atlas имеет версию, возрастающую при изменении пикселей; GPU-адаптер загружает только новую версию и повторно конвертирует вершины только для нового draw list. UI рисуется в output pass конкретного вида после scene/post-process вывода, вне HDR scene target и temporal обработки. Он виден только в тех кадрах, которым передали его primitives. Подробности прохода — [[Engine/EngineRender]].

Картинки UI живут в `GameUi` по пути с числом ссылок. Владельцы (префабы и сессии Play) держат картинки через `attach`/`detach`. Сцена через `adopt_scene_images` закрепляет свои пути до `release_all`: повторная картинка того же пути освобождает только свой новый id, а уже загруженная текстура остаётся. `release_all` вызывается при выходе runtime (`RenderBackend::release_scene`) и при остановке сессии Play в редакторе; каждая текстура уничтожается ровно один раз.

## Источники

Схема/layout/state/input/paint: `engine/engine-gui/src/{document,layout,system,input,capture,events,paint}.rs`. Загрузка, транзакции и Lua/ABI bridge: `engine/engine-script/src/{ui_assets,ui,ui_input,ui_lua,abi_host,library}.rs`. GPU-кэш: `engine/engine-ui/src/game.rs`; runtime кадр: `engine/engine-runtime/src/{game_ui,pipeline}.rs`. Сценовая ссылка — [[Engine/EngineScene]], редактор — [[RveEditor]], ABI — [[RveSdk]].
