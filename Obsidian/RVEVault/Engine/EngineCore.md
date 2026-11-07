# EngineCore

Parent: [[Index]]

## Назначение

`engine-core` предоставляет счётчик времени для игрового цикла. Он использует `std::time` и не зависит от других крейтов workspace.

## Источники

| Файл | Значимый символ | Роль |
|---|---|---|
| `engine/engine-core/src/lib.rs` | `Time` | Время с запуска, delta между тиками и номер тика |
| `engine/engine-core/Cargo.toml` | `engine-core` | Manifest библиотеки без зависимостей |

## Публичные контракты

| Контракт | Поведение и условия |
|---|---|
| `Time::new() -> Time` | Создаёт счётчик с нулевым номером кадра |
| `Time::tick(&mut self) -> Duration` | Возвращает время с прошлого тика и увеличивает номер тика |
| `Time::elapsed(&self) -> Duration` | Возвращает длительность с момента создания |
| `Time::frame(&self) -> u64` | Возвращает число выполненных тиков |
| `Default for Time` | Создаёт новое значение через `Time::new()` |
| `profile::RollingStat::new(capacity)`, `push`, `summary() -> StatSummary` | Кольцевое окно замеров фиксированной ёмкости; память выделяется в `new`, `push` её не расширяет. Пустое окно даёт нулевой итог с `samples == 0` |
| `profile::ProfileReport`, `ProfileGroup`, `ProfileRow`, `ProfileValue` | Отчёт для показа: группы строк, метка и значение строки. Значение — итоги окна (`Timing`), одна длительность (`Single`), счётчик (`Count`) или отсутствие значения (`Missing`, показывается как `n/a`). `Display` даёт текст для консоли и лога |
| `profile::ProfileBoard::new(sources)`, `publish(source, report)`, `reports()` | Доска отчётов по фиксированному набору источников. Публикация меняет `Arc` под коротким lock; чтение возвращает отчёты в порядке регистрации |

## Зависимости и взаимодействия

Используется в [[Engine/Sandbox]] для времени кадров и меняющегося clear color. Модуль `profile` используют [[Engine/EngineRuntime]] (отчёты потоков, старт и оверлей F3), [[Engine/RveEditor]] (вкладка Profiler) и `engine-ui` (таблица `profiler`). `engine-resources` зависит от `engine-core` ради `ResourceSampler`: группа `Resources` строится там же, где лежит `ResourceStats`, и её используют и runtime, и редактор. `engine-render` и `engine-script` пишут длительности через `std::time::Duration` и на `engine-core` не опираются.

## Ограничения и важные детали

- Время измеряется через `std::time::Instant`.
- Unit-тесты проверяют увеличение счётчика тиков и монотонность `elapsed`.
- Модуль `profile` использует только `std`. Доска не хранит отчёты дольше последней публикации и не ведёт историю.
