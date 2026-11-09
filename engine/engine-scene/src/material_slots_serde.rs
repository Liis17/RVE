//! TOML encoding for per-part material slots. Empty slots are stable empty strings.

use engine_files::VirtualPath;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

fn encode_slots(slots: &[Option<VirtualPath>]) -> Vec<String> {
    slots
        .iter()
        .map(|slot| {
            slot.as_ref()
                .map(|path| format!("/{path}"))
                .unwrap_or_default()
        })
        .collect()
}

fn decode_slots<E>(slots: Vec<String>) -> Result<Vec<Option<VirtualPath>>, E>
where
    E: serde::de::Error,
{
    slots
        .into_iter()
        .map(|path| {
            if path.is_empty() {
                Ok(None)
            } else {
                VirtualPath::parse(&path)
                    .map(Some)
                    .map_err(|error| E::custom(error.to_string()))
            }
        })
        .collect()
}

pub(crate) fn serialize<S>(slots: &[Option<VirtualPath>], serializer: S) -> Result<S::Ok, S::Error>
where
    S: Serializer,
{
    encode_slots(slots).serialize(serializer)
}

pub(crate) fn deserialize<'de, D>(deserializer: D) -> Result<Vec<Option<VirtualPath>>, D::Error>
where
    D: Deserializer<'de>,
{
    decode_slots(Vec::<String>::deserialize(deserializer)?)
}

pub(crate) mod optional {
    use super::{decode_slots, encode_slots};
    use engine_files::VirtualPath;
    use serde::{Deserialize, Deserializer, Serializer};

    pub(crate) fn serialize<S>(
        slots: &Option<Vec<Option<VirtualPath>>>,
        serializer: S,
    ) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        match slots {
            Some(slots) => serializer.collect_seq(encode_slots(slots)),
            None => serializer.serialize_none(),
        }
    }

    pub(crate) fn deserialize<'de, D>(
        deserializer: D,
    ) -> Result<Option<Vec<Option<VirtualPath>>>, D::Error>
    where
        D: Deserializer<'de>,
    {
        Option::<Vec<String>>::deserialize(deserializer)?
            .map(decode_slots)
            .transpose()
    }
}
