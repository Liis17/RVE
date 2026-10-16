# EngineFiles

Parent: [[Index]]

## Назначение

`engine-files` задаёт виртуальные пути игрового namespace и общий контракт позиционного чтения. Он не знает о форматах ресурсов, файлах архивов или renderer; его первый потребитель — [[Engine/EnginePak]].

## Источники

| Файл | Значимый символ | Роль |
|---|---|---|
| `engine/engine-files/src/virtual_path.rs` | `VirtualPath` | Канонический относительный UTF-8 путь |
| `engine/engine-files/src/read_at.rs` | `ReadAt`, `FileSource` | Checked positional reads с 64-битными смещениями |
| `engine/engine-files/src/path.rs` | `strip_windows_verbatim_prefix` | Снятие Windows verbatim-префикса с дискового или UNC-пути |

## Публичные контракты

- `VirtualPath::parse` принимает один необязательный ведущий `/`, сохраняет canonical value без него, использует только `/` как разделитель и сохраняет регистр. Пустой путь или component, `.`, `..`, `\`, `:`, NUL запрещены. Unicode не нормализуется.
- `ReadAt` сообщает длину источника и читает по абсолютному `u64` offset. `read_exact_at` повторяет короткие чтения и `Interrupted`, проверяет арифметику и возвращает EOF ошибкой, если диапазон невозможно заполнить.
- `FileSource` закрепляет открытый file handle. Unix использует `FileExt::read_at`; Windows `FileExt::seek_read` меняет cursor, поэтому один источник сериализует операции на своём handle. Файловые workers создают отдельные источники, а не делят один Windows cursor.
- `strip_windows_verbatim_prefix` на Windows преобразует `\\?\C:\...` в обычный дисковый путь, а `\\?\UNC\server\share\...` — в UNC-путь и приводит разделители к `\`. Другие формы namespace `\\?\` возвращают `InvalidInput`; на остальных ОС путь возвращается без изменений. Разбор Windows-форм тестируется из строк на всех платформах.
- Реализация `ReadAt` может быть injected как recording/fake reader для тестирования выборочного доступа и смещений выше 4 GiB без больших файлов.

## Зависимости и ограничения

Крейт не имеет внешних зависимостей. Для targets без positional file API `FileSource` возвращает `Unsupported`; интерфейс trait остаётся доступен для собственных источников.
