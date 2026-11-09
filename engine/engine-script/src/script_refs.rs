//! Справочник экземпляров скриптов для обращения из других скриптов.
//!
//! Экземпляр адресуется серийным номером, который никогда не повторяется в процессе. Поэтому
//! ссылка, сохранённая после удаления объекта или смены сцены, не может попасть на другой
//! экземпляр: её поиск даёт `Dead`. Справочник меняет только `ScriptHost` при создании,
//! удалении и коммите; транзакции читают его и ставят заявки.

use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};
use std::rc::Rc;

use engine_files::VirtualPath;
use engine_scene::{ObjectHandle, ParamSchema, ParamValue};

/// Одна запись справочника: объявленные свойства и флаг включения экземпляра.
pub(crate) struct ScriptRefEntry {
    pub(crate) serial: u64,
    pub(crate) object: ObjectHandle,
    /// Ключ поиска: виртуальный путь lite-скрипта или имя heavy-скрипта.
    pub(crate) key: String,
    pub(crate) schema: Option<Rc<ParamSchema>>,
    /// Текущие значения объявленных свойств; источник для чтения через `get`.
    pub(crate) properties: BTreeMap<String, ParamValue>,
    pub(crate) enabled: bool,
    /// Имена свойств, записанных другим скриптом после последнего вызова: только они попадают в Lua.
    pub(crate) dirty_properties: BTreeSet<String>,
}

pub(crate) type ScriptRefs = Rc<RefCell<Vec<ScriptRefEntry>>>;

/// Причина отказа при обращении к чужому экземпляру; текст попадает в ошибку вызова.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum ScriptRefError {
    Dead,
    Undeclared(String),
    Invalid(String),
}

impl ScriptRefError {
    pub(crate) fn message(&self) -> String {
        match self {
            Self::Dead => "script instance is no longer alive".to_owned(),
            Self::Undeclared(name) => format!("property {name:?} is not declared by the script"),
            Self::Invalid(reason) => reason.clone(),
        }
    }
}

/// Находит экземпляр объекта по ключу; при нескольких совпадениях берётся первый в порядке
/// объявления на объекте.
pub(crate) fn find_serial(
    entries: &[ScriptRefEntry],
    object: ObjectHandle,
    key: &str,
) -> Option<u64> {
    entries
        .iter()
        .find(|entry| entry.object == object && entry.key == key)
        .map(|entry| entry.serial)
}

pub(crate) fn find_entry(entries: &[ScriptRefEntry], serial: u64) -> Option<&ScriptRefEntry> {
    entries.iter().find(|entry| entry.serial == serial)
}

/// Проверяет запись свойства по схеме экземпляра и приводит значение.
pub(crate) fn coerce_property(
    entry: &ScriptRefEntry,
    name: &str,
    value: &ParamValue,
) -> Result<ParamValue, ScriptRefError> {
    let spec = entry
        .schema
        .as_ref()
        .and_then(|schema| schema.get(name))
        .ok_or_else(|| ScriptRefError::Undeclared(name.to_owned()))?;
    spec.coerce(value).map_err(ScriptRefError::Invalid)
}

/// Канонический ключ поиска: пути принимаются с ведущим слешем и без него, как в сценах.
pub(crate) fn normalize_script_key(key: &str) -> String {
    VirtualPath::parse(key).map_or_else(|_| key.to_owned(), |path| path.as_str().to_owned())
}
