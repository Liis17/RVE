//! Сохранение комментариев и оформления TOML при перезаписи файла, который редактор
//! сериализует заново из модели.
//!
//! Новый текст накладывается на прежний: ключи и таблицы с прежним значением остаются
//! нетронутыми вместе с комментариями, изменённые значения получают новое значение со старым
//! оформлением вокруг, удалённые исчезают, новые добавляются в конец своей таблицы. Элементы
//! массивов таблиц (`[[objects]]`, `[[assets]]`) сопоставляются по `id`, затем по `path`, иначе по
//! индексу; порядок элементов берётся из нового текста. Ключи, которые сериализатор дописал в
//! прежние таблицы, но которые не меняют документ (значения по умолчанию), не добавляются.

use toml_edit::{ArrayOfTables, DocumentMut, Item, Table, Value};

/// Текст `generated` с комментариями и оформлением из `previous`.
///
/// `same(text)` сообщает, описывает ли `text` тот же документ, что `generated`; им проверяется
/// каждый кандидат. `None` — прежний текст не разбирается или подходящего кандидата нет; тогда
/// записывается `generated`.
pub fn preserve_formatting(
    previous: &str,
    generated: &str,
    same: impl Fn(&str) -> bool,
) -> Option<String> {
    let mut merged = previous.parse::<DocumentMut>().ok()?;
    let generated_document = generated.parse::<DocumentMut>().ok()?;
    let mut added = Vec::new();
    merge_table(
        merged.as_table_mut(),
        generated_document.as_table(),
        &mut Vec::new(),
        &mut added,
    );
    // Сначала таблицы остаются на прежних местах файла, новые встают за предыдущей таблицей.
    // Если элементы переставлены, порядок заголовков задаётся заново по структуре.
    let mut renumbered = merged.clone();
    let mut position = 0;
    renumber(renumbered.as_table_mut(), &mut position);
    [merged, renumbered]
        .into_iter()
        .find_map(|candidate| without_redundant_keys(candidate, &added, &same))
}

/// Предел поштучных проверок добавленных ключей: каждая разбирает весь документ, а оставшиеся
/// ключи лишь повторяют значения по умолчанию.
const MAX_KEY_TRIALS: usize = 256;

/// Убирает добавленные ключи, без которых документ не меняется; `None` — кандидат не подходит.
fn without_redundant_keys(
    candidate: DocumentMut,
    added: &[Vec<Step>],
    same: &impl Fn(&str) -> bool,
) -> Option<String> {
    let mut all_removed = candidate.clone();
    for path in added {
        remove_key(all_removed.as_table_mut(), path);
    }
    let text = all_removed.to_string();
    if same(&text) {
        return Some(text);
    }
    let mut candidate = candidate;
    let mut text = candidate.to_string();
    if !same(&text) {
        return None;
    }
    for path in added.iter().take(MAX_KEY_TRIALS) {
        let mut trial = candidate.clone();
        remove_key(trial.as_table_mut(), path);
        let trial_text = trial.to_string();
        if same(&trial_text) {
            candidate = trial;
            text = trial_text;
        }
    }
    Some(text)
}

/// Шаг пути к ключу: ключ таблицы или элемент массива таблиц.
#[derive(Clone, Debug)]
enum Step {
    Key(String),
    Index(usize),
}

fn remove_key(table: &mut Table, path: &[Step]) {
    let Some((Step::Key(last), parents)) = path.split_last() else {
        return;
    };
    let mut current = table;
    let mut steps = parents.iter();
    while let Some(step) = steps.next() {
        let Step::Key(key) = step else {
            return;
        };
        current = match current.get_mut(key) {
            Some(Item::Table(table)) => table,
            Some(Item::ArrayOfTables(array)) => match steps.next() {
                Some(Step::Index(index)) => match array.get_mut(*index) {
                    Some(table) => table,
                    None => return,
                },
                _ => return,
            },
            _ => return,
        };
    }
    current.remove(last);
}

/// Равенство данных TOML. Числа сцены хранятся в `f32`, поэтому `1.15` из файла и его
/// сериализация `1.149999976158142` считаются одним значением, и автор сохраняет свою запись.
pub fn same_data(left: &toml::Value, right: &toml::Value) -> bool {
    use toml::Value;
    match (left, right) {
        (Value::Float(left), Value::Float(right)) => *left as f32 == *right as f32,
        (Value::Integer(left), Value::Float(right))
        | (Value::Float(right), Value::Integer(left)) => *left as f32 == *right as f32,
        (Value::Array(left), Value::Array(right)) => {
            left.len() == right.len()
                && left
                    .iter()
                    .zip(right)
                    .all(|(left, right)| same_data(left, right))
        }
        (Value::Table(left), Value::Table(right)) => {
            left.len() == right.len()
                && left
                    .iter()
                    .all(|(key, left)| right.get(key).is_some_and(|right| same_data(left, right)))
        }
        _ => left == right,
    }
}

fn merge_table(old: &mut Table, new: &Table, path: &mut Vec<Step>, added: &mut Vec<Vec<Step>>) {
    let removed = old
        .iter()
        .map(|(key, _)| key.to_owned())
        .filter(|key| !new.contains_key(key))
        .collect::<Vec<_>>();
    for key in removed {
        old.remove(&key);
    }
    for (key, new_item) in new.iter() {
        path.push(Step::Key(key.to_owned()));
        match old.get_mut(key) {
            Some(old_item) => merge_item(old_item, new_item, path, added),
            None => {
                let name = new.key(key).expect("key from iteration").clone();
                old.insert_formatted(&name, detached(new_item.clone()));
                added.push(path.clone());
                record_keys(new_item, path, added);
            }
        }
        path.pop();
    }
}

fn merge_item(old: &mut Item, new: &Item, path: &mut Vec<Step>, added: &mut Vec<Vec<Step>>) {
    match (&mut *old, new) {
        (Item::Table(old), Item::Table(new)) => merge_table(old, new, path, added),
        (Item::ArrayOfTables(old), Item::ArrayOfTables(new)) => {
            merge_tables_array(old, new, path, added);
        }
        (Item::Value(old), Item::Value(new)) => merge_value(old, new.clone()),
        // Автор записал таблицу или массив таблиц в строку: сохраняется строчная форма.
        (Item::Value(old @ Value::InlineTable(_)), Item::Table(new)) => {
            merge_value(old, Value::InlineTable(new.clone().into_inline_table()));
        }
        (Item::Value(old @ Value::Array(_)), Item::ArrayOfTables(new)) => {
            merge_value(old, Value::Array(new.clone().into_array()));
        }
        _ => *old = detached(new.clone()),
    }
}

/// Прежнее значение остаётся, если данные совпадают; иначе заменяется с прежним оформлением.
fn merge_value(old: &mut Value, mut new: Value) {
    if same_value(old, &new) {
        return;
    }
    let mut written = old.clone();
    written.decor_mut().clear();
    if !written.to_string().contains('\n') {
        compact(&mut new);
    }
    *new.decor_mut() = old.decor().clone();
    *old = new;
}

/// Однострочная запись массива или строчной таблицы, как в файлах, написанных вручную.
fn compact(value: &mut Value) {
    match value {
        Value::Array(array) => {
            array.iter_mut().for_each(compact);
            array.fmt();
        }
        Value::InlineTable(table) => {
            table.iter_mut().for_each(|(_, value)| compact(value));
            table.fmt();
        }
        _ => {}
    }
}

/// Ключи вставленной таблицы — кандидаты на удаление, если они повторяют значения по умолчанию.
fn record_keys(item: &Item, path: &mut Vec<Step>, added: &mut Vec<Vec<Step>>) {
    match item {
        Item::Table(table) => record_table_keys(table, path, added),
        Item::ArrayOfTables(array) => {
            for (index, table) in array.iter().enumerate() {
                path.push(Step::Index(index));
                record_table_keys(table, path, added);
                path.pop();
            }
        }
        _ => {}
    }
}

fn record_table_keys(table: &Table, path: &mut Vec<Step>, added: &mut Vec<Vec<Step>>) {
    for (key, item) in table.iter() {
        path.push(Step::Key(key.to_owned()));
        added.push(path.clone());
        record_keys(item, path, added);
        path.pop();
    }
}

fn same_value(left: &Value, right: &Value) -> bool {
    let data = |value: &Value| {
        let mut value = value.clone();
        value.decor_mut().clear();
        toml::from_str::<toml::Value>(&format!("v = {value}")).ok()
    };
    matches!((data(left), data(right)), (Some(left), Some(right)) if same_data(&left, &right))
}

fn merge_tables_array(
    old: &mut ArrayOfTables,
    new: &ArrayOfTables,
    path: &mut Vec<Step>,
    added: &mut Vec<Vec<Step>>,
) {
    // Пустые строки перед заголовком относятся к месту в файле, а не к элементу: при
    // перестановке они остаются на своей позиции. Комментарии переезжают вместе с элементом.
    let slot_spacing = old.iter().map(blank_prefix).collect::<Vec<_>>();
    let mut previous = old.iter().cloned().map(Some).collect::<Vec<_>>();
    let mut merged = ArrayOfTables::new();
    for (index, new_table) in new.iter().enumerate() {
        let identity = identity(new_table);
        let found = match &identity {
            Some(identity) => previous.iter().position(|table| {
                table
                    .as_ref()
                    .is_some_and(|table| self::identity(table).as_ref() == Some(identity))
            }),
            None => previous
                .get(index)
                .and_then(Option::as_ref)
                .is_some_and(|table| self::identity(table).is_none())
                .then_some(index),
        };
        let table = match found.and_then(|index| previous[index].take()) {
            Some(mut table) => {
                path.push(Step::Index(index));
                merge_table(&mut table, new_table, path, added);
                path.pop();
                if blank_prefix(&table).is_some()
                    && let Some(Some(spacing)) = slot_spacing.get(index)
                {
                    table.decor_mut().set_prefix(spacing.clone());
                }
                table
            }
            None => {
                let mut table = new_table.clone();
                clear_positions(&mut table);
                path.push(Step::Index(index));
                record_table_keys(&table, path, added);
                path.pop();
                table
            }
        };
        merged.push(table);
    }
    *old = merged;
}

/// Элемент из нового текста без позиций таблиц (в выводе он встаёт за предыдущей таблицей)
/// и с однострочными массивами.
fn detached(mut item: Item) -> Item {
    match &mut item {
        Item::Table(table) => clear_positions(table),
        Item::ArrayOfTables(array) => array.iter_mut().for_each(clear_positions),
        Item::Value(value) => {
            let decor = value.decor().clone();
            compact(value);
            *value.decor_mut() = decor;
        }
        Item::None => {}
    }
    item
}

fn clear_positions(table: &mut Table) {
    // toml_edit не сбрасывает позицию, поэтому таблица собирается заново.
    let mut fresh = Table::new();
    fresh.set_implicit(table.is_implicit());
    fresh.set_dotted(table.is_dotted());
    *fresh.decor_mut() = table.decor().clone();
    for (key, item) in table.iter() {
        let name = table.key(key).expect("key from iteration").clone();
        fresh.insert_formatted(&name, detached(item.clone()));
    }
    *table = fresh;
}

/// Префикс заголовка таблицы, если он состоит только из пробелов и переводов строк.
fn blank_prefix(table: &Table) -> Option<String> {
    let prefix = table
        .decor()
        .prefix()
        .map_or(Some(""), |prefix| prefix.as_str())?;
    prefix
        .chars()
        .all(char::is_whitespace)
        .then(|| prefix.to_owned())
}

/// Ключ сопоставления элемента массива таблиц.
fn identity(table: &Table) -> Option<(&'static str, String)> {
    ["id", "path"].into_iter().find_map(|key| {
        table
            .get(key)
            .and_then(Item::as_str)
            .map(|value| (key, value.to_owned()))
    })
}

/// Нумерует заголовки заново в порядке структуры.
fn renumber(table: &mut Table, position: &mut usize) {
    for (_, item) in table.iter_mut() {
        match item {
            Item::Table(table) => {
                *position += 1;
                table.set_position(*position);
                renumber(table, position);
            }
            Item::ArrayOfTables(array) => {
                for table in array.iter_mut() {
                    *position += 1;
                    table.set_position(*position);
                    renumber(table, position);
                }
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn same_as(generated: &str) -> impl Fn(&str) -> bool + '_ {
        move |text| {
            let parse = |text: &str| toml::from_str::<toml::Value>(text).ok();
            matches!((parse(text), parse(generated)), (Some(a), Some(b)) if same_data(&a, &b))
        }
    }

    const PREVIOUS: &str = "\
# Главная сцена демо.
[[assets]]
path = \"/models/a.fbx\" # общий куб

[[assets]]
path = \"/scripts/rotate.lua\"

# Камера игрока
[[objects]]
id = \"camera\"
name = \"Camera\"
[objects.transform]
position = [0.0, 1.0,   8.0] # над полом
[objects.camera]

[[objects]]
id = \"cube\"
name = \"Cube\"
[objects.transform]
position = [1.0, 0.0, 0.0]
[objects.mesh_renderer]
model = \"/models/a.fbx\"
[[objects.lite_scripts]]
path = \"/scripts/rotate.lua\"
params = { speed = 24.0 }
";

    #[test]
    fn unchanged_document_is_written_back_byte_for_byte() {
        let generated = "\
[[assets]]
path = \"/models/a.fbx\"

[[assets]]
path = \"/scripts/rotate.lua\"

[[objects]]
id = \"camera\"
name = \"Camera\"

[objects.transform]
position = [0.0, 1.0, 8.0]

[objects.camera]

[[objects]]
id = \"cube\"
name = \"Cube\"

[objects.transform]
position = [1.0, 0.0, 0.0]

[objects.mesh_renderer]
model = \"/models/a.fbx\"

[[objects.lite_scripts]]
path = \"/scripts/rotate.lua\"

[objects.lite_scripts.params]
speed = 24.0
";
        assert_eq!(
            preserve_formatting(PREVIOUS, generated, same_as(generated)).as_deref(),
            Some(PREVIOUS)
        );
    }

    #[test]
    fn edits_keep_comments_of_untouched_parts_and_follow_the_new_order() {
        // Куб удалён, камера сдвинута и переименована, добавлены свет и новый ресурс.
        let generated = "\
[[assets]]
path = \"/models/a.fbx\"

[[assets]]
path = \"/scripts/rotate.lua\"

[[assets]]
path = \"/models/b.fbx\"

[[objects]]
id = \"camera\"
name = \"Main Camera\"

[objects.transform]
position = [0.0, 2.0, 8.0]

[objects.camera]

[[objects]]
id = \"sun\"

[objects.directional_light]
shadows = true
";
        let merged = preserve_formatting(PREVIOUS, generated, same_as(generated)).unwrap();
        assert_eq!(
            merged,
            "\
# Главная сцена демо.
[[assets]]
path = \"/models/a.fbx\" # общий куб

[[assets]]
path = \"/scripts/rotate.lua\"

[[assets]]
path = \"/models/b.fbx\"

# Камера игрока
[[objects]]
id = \"camera\"
name = \"Main Camera\"
[objects.transform]
position = [0.0, 2.0, 8.0] # над полом
[objects.camera]

[[objects]]
id = \"sun\"

[objects.directional_light]
shadows = true
"
        );
    }

    #[test]
    fn reordered_objects_and_inline_tables_keep_their_form() {
        let previous = "\
[[objects]]
id = \"a\" # первый
transform = { position = [1.0, 0.0, 0.0] }

[[objects]]
id = \"b\" # второй
";
        let generated = "\
[[objects]]
id = \"b\"

[[objects]]
id = \"a\"

[objects.transform]
position = [2.0, 0.0, 0.0]
";
        assert_eq!(
            preserve_formatting(previous, generated, same_as(generated)).unwrap(),
            "\
[[objects]]
id = \"b\" # второй

[[objects]]
id = \"a\" # первый
transform = { position = [2.0, 0.0, 0.0] }
"
        );
    }

    #[test]
    fn unparsable_previous_text_falls_back_to_the_generated_text() {
        assert_eq!(
            preserve_formatting("objects = [", "a = 1\n", same_as("a = 1\n")),
            None
        );
    }
}
