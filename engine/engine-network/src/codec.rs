//! Бинарный кодек значений сообщений скриптов и читатель буфера.
//!
//! Глубина, число элементов и суммарный размер строк проверяются до выделения памяти и до
//! рекурсивного спуска. Поэтому вражеские данные не вызывают переполнения стека или нехватки памяти.

use engine_files::VirtualPath;
use engine_scene::ParamValue;
use engine_scene::network::{MAX_MESSAGE_DEPTH, MAX_MESSAGE_ELEMENTS, MAX_MESSAGE_STRING_BYTES};

use crate::NetError;

const TAG_BOOL: u8 = 0;
const TAG_INT: u8 = 1;
const TAG_FLOAT: u8 = 2;
const TAG_STRING: u8 = 3;
const TAG_ARRAY: u8 = 4;
const TAG_OBJECT: u8 = 5;
const TAG_ASSET: u8 = 6;

/// Кодирует значение в конец `out`. Значение, нарушающее лимиты, отклоняется целиком.
pub fn encode_value(value: &ParamValue, out: &mut Vec<u8>) -> Result<(), NetError> {
    encode_node(value, 0, &mut Budget::default(), out)
}

/// Декодирует значение, которое занимает весь буфер.
pub fn decode_value(data: &[u8]) -> Result<ParamValue, NetError> {
    let mut reader = Reader::new(data);
    let value = decode_node(&mut reader, 0, &mut Budget::default())?;
    if reader.remaining() != 0 {
        return Err(invalid("лишние байты после значения"));
    }
    Ok(value)
}

/// Общие счётчики значения: элементы и байты строк.
#[derive(Default)]
struct Budget {
    elements: usize,
    string_bytes: usize,
}

impl Budget {
    fn element(&mut self) -> Result<(), NetError> {
        self.elements += 1;
        if self.elements > MAX_MESSAGE_ELEMENTS {
            return Err(invalid(format!(
                "значение содержит больше {MAX_MESSAGE_ELEMENTS} элементов"
            )));
        }
        Ok(())
    }

    fn string(&mut self, len: usize) -> Result<(), NetError> {
        self.string_bytes += len;
        if self.string_bytes > MAX_MESSAGE_STRING_BYTES {
            return Err(invalid(format!(
                "строки значения длиннее {MAX_MESSAGE_STRING_BYTES} байт"
            )));
        }
        Ok(())
    }
}

fn encode_node(
    value: &ParamValue,
    depth: usize,
    budget: &mut Budget,
    out: &mut Vec<u8>,
) -> Result<(), NetError> {
    budget.element()?;
    match value {
        ParamValue::Bool(flag) => {
            out.push(TAG_BOOL);
            out.push(u8::from(*flag));
        }
        ParamValue::Int(number) => {
            out.push(TAG_INT);
            out.extend_from_slice(&number.to_be_bytes());
        }
        ParamValue::Float(number) => {
            if !number.is_finite() {
                return Err(invalid("число должно быть конечным"));
            }
            out.push(TAG_FLOAT);
            out.extend_from_slice(&number.to_be_bytes());
        }
        ParamValue::String(text) => {
            budget.string(text.len())?;
            out.push(TAG_STRING);
            push_len(out, text.len())?;
            out.extend_from_slice(text.as_bytes());
        }
        ParamValue::Object(id) => {
            budget.string(id.len())?;
            out.push(TAG_OBJECT);
            push_len(out, id.len())?;
            out.extend_from_slice(id.as_bytes());
        }
        ParamValue::Asset(path) => {
            let path = path.as_str();
            budget.string(path.len())?;
            out.push(TAG_ASSET);
            push_len(out, path.len())?;
            out.extend_from_slice(path.as_bytes());
        }
        ParamValue::Array(items) => {
            if depth >= MAX_MESSAGE_DEPTH {
                return Err(invalid(format!(
                    "вложенность массивов больше {MAX_MESSAGE_DEPTH}"
                )));
            }
            out.push(TAG_ARRAY);
            push_len(out, items.len())?;
            for item in items {
                encode_node(item, depth + 1, budget, out)?;
            }
        }
    }
    Ok(())
}

fn decode_node(
    reader: &mut Reader<'_>,
    depth: usize,
    budget: &mut Budget,
) -> Result<ParamValue, NetError> {
    budget.element()?;
    let tag = reader.u8().ok_or_else(truncated)?;
    match tag {
        TAG_BOOL => match reader.u8().ok_or_else(truncated)? {
            0 => Ok(ParamValue::Bool(false)),
            1 => Ok(ParamValue::Bool(true)),
            _ => Err(invalid("неверное логическое значение")),
        },
        TAG_INT => {
            let bytes = reader.array().ok_or_else(truncated)?;
            Ok(ParamValue::Int(i64::from_be_bytes(bytes)))
        }
        TAG_FLOAT => {
            let bytes = reader.array().ok_or_else(truncated)?;
            let number = f64::from_be_bytes(bytes);
            if !number.is_finite() {
                return Err(invalid("число должно быть конечным"));
            }
            Ok(ParamValue::Float(number))
        }
        TAG_STRING => {
            let len = read_len(reader)?;
            budget.string(len)?;
            let bytes = reader.take(len).ok_or_else(truncated)?;
            let text = std::str::from_utf8(bytes).map_err(|_| invalid("строка не в UTF-8"))?;
            Ok(ParamValue::String(text.to_owned()))
        }
        TAG_OBJECT | TAG_ASSET => {
            let len = read_len(reader)?;
            budget.string(len)?;
            let bytes = reader.take(len).ok_or_else(truncated)?;
            let text = std::str::from_utf8(bytes).map_err(|_| invalid("строка не в UTF-8"))?;
            if tag == TAG_OBJECT {
                Ok(ParamValue::Object(text.to_owned()))
            } else {
                VirtualPath::parse(text)
                    .map(ParamValue::Asset)
                    .map_err(|_| invalid("некорректный путь ресурса"))
            }
        }
        TAG_ARRAY => {
            if depth >= MAX_MESSAGE_DEPTH {
                return Err(invalid(format!(
                    "вложенность массивов больше {MAX_MESSAGE_DEPTH}"
                )));
            }
            let count = read_len(reader)?;
            if count > MAX_MESSAGE_ELEMENTS {
                return Err(invalid(format!(
                    "массив содержит больше {MAX_MESSAGE_ELEMENTS} элементов"
                )));
            }
            let mut items = Vec::with_capacity(count);
            for _ in 0..count {
                items.push(decode_node(reader, depth + 1, budget)?);
            }
            Ok(ParamValue::Array(items))
        }
        _ => Err(invalid("неизвестный тип значения")),
    }
}

/// Длина, которая не может превышать оставшиеся байты: каждый элемент занимает хотя бы байт.
fn read_len(reader: &mut Reader<'_>) -> Result<usize, NetError> {
    let len = reader.u32().ok_or_else(truncated)?;
    let len = usize::try_from(len).map_err(|_| truncated())?;
    if len > reader.remaining() {
        return Err(truncated());
    }
    Ok(len)
}

fn push_len(out: &mut Vec<u8>, len: usize) -> Result<(), NetError> {
    let len = u32::try_from(len).map_err(|_| invalid("слишком длинное значение"))?;
    out.extend_from_slice(&len.to_be_bytes());
    Ok(())
}

fn invalid(reason: impl Into<String>) -> NetError {
    NetError::InvalidMessage(reason.into())
}

fn truncated() -> NetError {
    invalid("значение обрезано")
}

/// Читатель буфера: любое чтение за пределами данных возвращает `None`.
pub(crate) struct Reader<'a> {
    rest: &'a [u8],
}

impl<'a> Reader<'a> {
    pub(crate) fn new(data: &'a [u8]) -> Self {
        Self { rest: data }
    }

    pub(crate) fn take(&mut self, len: usize) -> Option<&'a [u8]> {
        if len > self.rest.len() {
            return None;
        }
        let (head, tail) = self.rest.split_at(len);
        self.rest = tail;
        Some(head)
    }

    pub(crate) fn array<const N: usize>(&mut self) -> Option<[u8; N]> {
        self.take(N)?.try_into().ok()
    }

    pub(crate) fn u8(&mut self) -> Option<u8> {
        Some(self.take(1)?[0])
    }

    pub(crate) fn u16(&mut self) -> Option<u16> {
        self.array().map(u16::from_be_bytes)
    }

    pub(crate) fn u32(&mut self) -> Option<u32> {
        self.array().map(u32::from_be_bytes)
    }

    pub(crate) fn u64(&mut self) -> Option<u64> {
        self.array().map(u64::from_be_bytes)
    }

    pub(crate) fn remaining(&self) -> usize {
        self.rest.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn round_trip(value: &ParamValue) -> ParamValue {
        let mut bytes = Vec::new();
        encode_value(value, &mut bytes).unwrap();
        decode_value(&bytes).unwrap()
    }

    fn nested(depth: usize) -> ParamValue {
        (0..depth).fold(ParamValue::Bool(true), |inner, _| {
            ParamValue::Array(vec![inner])
        })
    }

    #[test]
    fn every_scalar_and_array_round_trips() {
        let value = ParamValue::Array(vec![
            ParamValue::Bool(false),
            ParamValue::Int(-42),
            ParamValue::Float(1.5),
            ParamValue::String("привет".into()),
            ParamValue::Object("target".into()),
            ParamValue::Asset(VirtualPath::parse("models/target.fbx").unwrap()),
            ParamValue::Array(vec![ParamValue::Int(7)]),
        ]);
        assert_eq!(round_trip(&value), value);
    }

    #[test]
    fn nesting_at_the_limit_is_accepted_and_one_more_is_rejected() {
        assert_eq!(
            round_trip(&nested(MAX_MESSAGE_DEPTH)),
            nested(MAX_MESSAGE_DEPTH)
        );

        let mut bytes = Vec::new();
        assert!(encode_value(&nested(MAX_MESSAGE_DEPTH + 1), &mut bytes).is_err());
    }

    #[test]
    fn deeply_nested_wire_data_is_rejected_without_recursion_overflow() {
        // 100 000 открывающих тегов массива без конца: отказ до спуска глубже лимита.
        let mut bytes = Vec::new();
        for _ in 0..100_000 {
            bytes.push(TAG_ARRAY);
            bytes.extend_from_slice(&1u32.to_be_bytes());
        }
        assert!(decode_value(&bytes).is_err());
    }

    #[test]
    fn huge_declared_lengths_are_rejected_before_allocation() {
        let array = [TAG_ARRAY, 0xff, 0xff, 0xff, 0xff];
        assert!(decode_value(&array).is_err());

        let string = [TAG_STRING, 0x7f, 0xff, 0xff, 0xff, b'a'];
        assert!(decode_value(&string).is_err());
    }

    #[test]
    fn too_many_elements_are_rejected() {
        let items = vec![ParamValue::Bool(true); MAX_MESSAGE_ELEMENTS];
        let value = ParamValue::Array(items);
        let mut bytes = Vec::new();
        assert!(encode_value(&value, &mut bytes).is_err());
    }

    #[test]
    fn oversized_strings_are_rejected() {
        let text = "a".repeat(MAX_MESSAGE_STRING_BYTES + 1);
        let mut bytes = Vec::new();
        assert!(encode_value(&ParamValue::String(text), &mut bytes).is_err());
    }

    #[test]
    fn malformed_scalars_are_rejected() {
        assert!(decode_value(&[TAG_BOOL, 2]).is_err());
        assert!(decode_value(&[TAG_INT, 0, 0]).is_err());
        assert!(decode_value(&[99]).is_err());
        assert!(decode_value(&[TAG_STRING, 0, 0, 0, 1, 0xff]).is_err());
        assert!(decode_value(&[TAG_BOOL, 1, 0]).is_err(), "лишние байты");
    }

    #[test]
    fn non_finite_floats_are_rejected_both_ways() {
        let mut bytes = Vec::new();
        assert!(encode_value(&ParamValue::Float(f64::NAN), &mut bytes).is_err());
        let inf = [&[TAG_FLOAT][..], &f64::INFINITY.to_be_bytes()].concat();
        assert!(decode_value(&inf).is_err());
    }

    #[test]
    fn reader_never_reads_past_the_end() {
        let mut reader = Reader::new(&[1, 2, 3]);
        assert_eq!(reader.u16(), Some(0x0102));
        assert_eq!(reader.u32(), None);
        assert_eq!(reader.remaining(), 1);
    }
}
