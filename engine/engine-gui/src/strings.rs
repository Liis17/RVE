//! Локализация игрового UI: таблицы строк по языкам и выбор строки по ключу.
//!
//! Таблица строк — TOML-файл одного языка: ключ → строка. Вложенные таблицы задают ключи
//! через точку (`[menu] start = "…"` и `menu.start = "…"` дают ключ `menu.start`).
//! Строка ищется в текущем языке, затем в языке по умолчанию, иначе показывается сам ключ.

use crate::document::UiError;
use std::collections::BTreeMap;

/// Строки одного языка по ключу.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct StringTable {
    strings: BTreeMap<String, String>,
}

impl StringTable {
    /// Разбирает TOML; значение, не являющееся строкой или таблицей, — ошибка с ключом.
    pub fn parse(source_label: &str, input: &str) -> Result<Self, UiError> {
        let root: toml::Table = toml::from_str(input)
            .map_err(|error| UiError::new(source_label, None, error.to_string()))?;
        let mut strings = BTreeMap::new();
        flatten(source_label, "", root, &mut strings)?;
        Ok(Self { strings })
    }

    pub fn get(&self, key: &str) -> Option<&str> {
        self.strings.get(key).map(String::as_str)
    }

    pub fn len(&self) -> usize {
        self.strings.len()
    }

    pub fn is_empty(&self) -> bool {
        self.strings.is_empty()
    }
}

impl FromIterator<(String, String)> for StringTable {
    fn from_iter<I: IntoIterator<Item = (String, String)>>(iter: I) -> Self {
        Self {
            strings: iter.into_iter().collect(),
        }
    }
}

fn flatten(
    source: &str,
    prefix: &str,
    table: toml::Table,
    out: &mut BTreeMap<String, String>,
) -> Result<(), UiError> {
    for (name, value) in table {
        let key = if prefix.is_empty() {
            name
        } else {
            format!("{prefix}.{name}")
        };
        match value {
            toml::Value::String(text) => {
                // Ключ `a.b` и таблица `a` с полем `b` дают один ключ; TOML сам запрещает повтор.
                out.insert(key, text);
            }
            toml::Value::Table(nested) => flatten(source, &key, nested, out)?,
            other => {
                return Err(UiError::new(
                    source,
                    None,
                    format!(
                        "ключ {key:?}: ожидается строка, получено {}",
                        other.type_str()
                    ),
                ));
            }
        }
    }
    Ok(())
}

/// Таблицы строк проекта и текущий язык.
#[derive(Clone, Debug, PartialEq)]
pub struct Localization {
    default_language: String,
    language: String,
    tables: BTreeMap<String, StringTable>,
}

impl Localization {
    /// Текущим становится язык по умолчанию; у него обязана быть таблица.
    pub fn new(
        default_language: impl Into<String>,
        tables: BTreeMap<String, StringTable>,
    ) -> Result<Self, String> {
        let default_language = default_language.into();
        if !tables.contains_key(&default_language) {
            return Err(format!(
                "нет таблицы строк для языка по умолчанию {default_language:?}"
            ));
        }
        Ok(Self {
            language: default_language.clone(),
            default_language,
            tables,
        })
    }

    pub fn language(&self) -> &str {
        &self.language
    }

    pub fn default_language(&self) -> &str {
        &self.default_language
    }

    /// Языки с таблицами, по возрастанию кода.
    pub fn languages(&self) -> impl Iterator<Item = &str> {
        self.tables.keys().map(String::as_str)
    }

    /// Меняет текущий язык; неизвестный язык — ошибка, текущий не меняется.
    pub fn set_language(&mut self, language: &str) -> Result<(), String> {
        if !self.tables.contains_key(language) {
            return Err(format!("нет таблицы строк для языка {language:?}"));
        }
        self.language = language.to_owned();
        Ok(())
    }

    /// Строка ключа: текущий язык, затем язык по умолчанию, иначе сам ключ.
    pub fn resolve<'a>(&'a self, key: &'a str) -> &'a str {
        [&self.language, &self.default_language]
            .into_iter()
            .find_map(|language| self.tables.get(language.as_str())?.get(key))
            .unwrap_or(key)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn table(input: &str) -> StringTable {
        StringTable::parse("test.toml", input).unwrap()
    }

    fn sample() -> Localization {
        Localization::new(
            "en",
            BTreeMap::from([
                (
                    "en".to_owned(),
                    table("title = \"Game\"\n[menu]\nstart = \"Start\"\nquit = \"Quit\"\n"),
                ),
                ("ru".to_owned(), table("menu.start = \"Начать\"\n")),
            ]),
        )
        .unwrap()
    }

    #[test]
    fn nested_tables_and_dotted_keys_flatten_to_one_key_space() {
        let strings = table("a = \"1\"\nb.c = \"2\"\n[d]\ne = \"3\"\n[d.f]\ng = \"\"\n");
        assert_eq!(strings.len(), 4);
        assert_eq!(strings.get("a"), Some("1"));
        assert_eq!(strings.get("b.c"), Some("2"));
        assert_eq!(strings.get("d.e"), Some("3"));
        assert_eq!(strings.get("d.f.g"), Some(""), "пустая строка допустима");
        assert_eq!(strings.get("d"), None);
        assert!(table("").is_empty());
    }

    #[test]
    fn non_string_values_and_bad_toml_are_errors() {
        let number = StringTable::parse("ru.toml", "menu.count = 3\n").unwrap_err();
        assert!(number.to_string().contains("menu.count"), "{number}");
        assert!(number.to_string().contains("ru.toml"), "{number}");
        assert!(StringTable::parse("ru.toml", "list = [\"a\"]\n").is_err());
        assert!(StringTable::parse("ru.toml", "a = \"1\"\na = \"2\"\n").is_err());
        assert!(StringTable::parse("ru.toml", "a = ").is_err());
    }

    #[test]
    fn resolve_falls_back_to_default_language_then_key() {
        let mut localization = sample();
        assert_eq!(localization.resolve("menu.start"), "Start");
        localization.set_language("ru").unwrap();
        assert_eq!(localization.language(), "ru");
        assert_eq!(localization.resolve("menu.start"), "Начать");
        assert_eq!(
            localization.resolve("menu.quit"),
            "Quit",
            "нет в ru — из en"
        );
        assert_eq!(localization.resolve("missing.key"), "missing.key");
    }

    #[test]
    fn unknown_language_is_rejected_and_keeps_current() {
        let mut localization = sample();
        localization.set_language("ru").unwrap();
        assert!(localization.set_language("de").is_err());
        assert_eq!(localization.language(), "ru");
        assert_eq!(localization.languages().collect::<Vec<_>>(), ["en", "ru"]);
        assert_eq!(localization.default_language(), "en");
    }

    #[test]
    fn default_language_needs_a_table() {
        let error = Localization::new("de", BTreeMap::new()).unwrap_err();
        assert!(error.contains("de"), "{error}");
    }
}
