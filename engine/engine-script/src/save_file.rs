use crate::transaction::validate_save_values;
use engine_scene::ParamValue;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

const MAX_SAVE_FILE_BYTES: usize = 1_048_576;

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct SaveFile {
    #[serde(default)]
    values: BTreeMap<String, ParamValue>,
}

/// Decodes a bounded save TOML document into validated script values.
pub fn decode_save_data(source: &str) -> Result<BTreeMap<String, ParamValue>, String> {
    if source.len() > MAX_SAVE_FILE_BYTES {
        return Err("save file exceeds 1 MiB".to_owned());
    }
    let file: SaveFile = toml::from_str(source).map_err(|error| error.to_string())?;
    validate_save_values(&file.values)?;
    Ok(file.values)
}

/// Encodes validated script values into deterministic TOML.
pub fn encode_save_data(values: &BTreeMap<String, ParamValue>) -> Result<String, String> {
    validate_save_values(values)?;
    let source = toml::to_string_pretty(&SaveFile {
        values: values.clone(),
    })
    .map_err(|error| error.to_string())?;
    if source.len() > MAX_SAVE_FILE_BYTES {
        return Err("save file exceeds 1 MiB".to_owned());
    }
    Ok(source)
}

#[cfg(test)]
mod tests {
    use super::{decode_save_data, encode_save_data};
    use engine_scene::ParamValue;
    use std::collections::BTreeMap;

    #[test]
    fn save_toml_roundtrips_param_values_and_is_deterministic() {
        let mut values = BTreeMap::new();
        values.insert("enabled".to_owned(), ParamValue::Bool(true));
        values.insert("count".to_owned(), ParamValue::Int(42));
        values.insert("target".to_owned(), ParamValue::Object("player".to_owned()));
        values.insert(
            "asset".to_owned(),
            ParamValue::Asset(engine_files::VirtualPath::parse("textures/hero.png").unwrap()),
        );
        values.insert(
            "values".to_owned(),
            ParamValue::Array(vec![
                ParamValue::Float(0.25),
                ParamValue::String("ok".to_owned()),
            ]),
        );
        let text = encode_save_data(&values).unwrap();
        assert_eq!(encode_save_data(&values).unwrap(), text);
        assert_eq!(decode_save_data(&text).unwrap(), values);
    }

    #[test]
    fn malformed_and_oversized_save_documents_are_rejected() {
        assert!(decode_save_data("values = 3\n").is_err());
        assert!(decode_save_data(&" ".repeat(1_048_577)).is_err());
        let values = BTreeMap::from([("bad".to_owned(), ParamValue::Float(f64::NAN))]);
        assert!(encode_save_data(&values).is_err());
    }
}
