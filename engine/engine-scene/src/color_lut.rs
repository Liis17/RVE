//! 3D LUT цветокоррекции в формате `.cube` (Adobe Cube LUT, экспорт DaVinci Resolve, Photoshop).
//!
//! Поддерживается только 3D LUT с доменом `[0, 1]`: ключевые слова `TITLE`, `LUT_3D_SIZE`,
//! `DOMAIN_MIN 0 0 0`, `DOMAIN_MAX 1 1 1` и `LUT_3D_INPUT_RANGE 0 1`, затем `size³` строк
//! `r g b`, где красный меняется быстрее всего. Пустые строки и строки с `#` пропускаются.
//! 1D LUT, другой домен, неизвестные ключевые слова и неконечные числа отклоняются с номером строки.

use std::error::Error;
use std::fmt;

/// Наименьшая сторона LUT.
pub const MIN_CUBE_LUT_SIZE: u32 = 2;
/// Наибольшая сторона LUT; совпадает с пределом renderer.
pub const MAX_CUBE_LUT_SIZE: u32 = 128;

/// Разобранная 3D LUT: сторона и `size³` узлов RGB в порядке файла.
#[derive(Clone, Debug, PartialEq)]
pub struct CubeLut {
    size: u32,
    texels: Vec<[f32; 3]>,
}

impl CubeLut {
    /// Разбирает текст `.cube`; `label` добавляется к ошибкам.
    pub fn parse(label: impl Into<String>, source: &str) -> Result<Self, CubeLutError> {
        let label = label.into();
        let error = |line: usize, message: String| CubeLutError {
            label: label.clone(),
            line: Some(line),
            message,
        };
        let mut size = None;
        let mut title_seen = false;
        let mut texels = Vec::new();
        let source = source.strip_prefix('\u{feff}').unwrap_or(source);
        for (index, raw) in source.lines().enumerate() {
            let number = index + 1;
            let line = raw.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let mut tokens = line.split_whitespace();
            let first = tokens.next().unwrap_or_default();
            if first.starts_with(|c: char| c.is_ascii_alphabetic()) {
                if !texels.is_empty() {
                    return Err(error(
                        number,
                        format!("ключевое слово {first} после данных LUT"),
                    ));
                }
                let values: Vec<&str> = tokens.collect();
                match first {
                    "TITLE" => {
                        if title_seen {
                            return Err(error(number, "TITLE указан повторно".to_owned()));
                        }
                        title_seen = true;
                    }
                    "LUT_3D_SIZE" => {
                        if size.is_some() {
                            return Err(error(number, "LUT_3D_SIZE указан повторно".to_owned()));
                        }
                        let parsed = match values.as_slice() {
                            [value] => value.parse::<u32>().ok(),
                            _ => None,
                        }
                        .filter(|value| (MIN_CUBE_LUT_SIZE..=MAX_CUBE_LUT_SIZE).contains(value))
                        .ok_or_else(|| {
                            error(
                                number,
                                format!(
                                    "LUT_3D_SIZE должен быть целым числом в [{MIN_CUBE_LUT_SIZE}, {MAX_CUBE_LUT_SIZE}]"
                                ),
                            )
                        })?;
                        size = Some(parsed);
                        texels.reserve_exact((parsed as usize).pow(3));
                    }
                    "LUT_1D_SIZE" | "LUT_1D_INPUT_RANGE" => {
                        return Err(error(
                            number,
                            "1D LUT не поддерживается: нужна 3D LUT (LUT_3D_SIZE)".to_owned(),
                        ));
                    }
                    "DOMAIN_MIN" => require_values(&values, &[0.0; 3])
                        .map_err(|()| error(number, "DOMAIN_MIN должен быть 0 0 0".to_owned()))?,
                    "DOMAIN_MAX" => require_values(&values, &[1.0; 3])
                        .map_err(|()| error(number, "DOMAIN_MAX должен быть 1 1 1".to_owned()))?,
                    "LUT_3D_INPUT_RANGE" => require_values(&values, &[0.0, 1.0]).map_err(|()| {
                        error(number, "LUT_3D_INPUT_RANGE должен быть 0 1".to_owned())
                    })?,
                    other => {
                        return Err(error(number, format!("неизвестное ключевое слово {other}")));
                    }
                }
                continue;
            }
            let Some(size) = size else {
                return Err(error(
                    number,
                    "LUT_3D_SIZE должен предшествовать данным".to_owned(),
                ));
            };
            if texels.len() == (size as usize).pow(3) {
                return Err(error(
                    number,
                    format!("больше {} узлов LUT", (size as usize).pow(3)),
                ));
            }
            let texel = parse_texel(line)
                .ok_or_else(|| error(number, "узел LUT — три конечных числа r g b".to_owned()))?;
            texels.push(texel);
        }
        let Some(size) = size else {
            return Err(CubeLutError {
                label,
                line: None,
                message: "нет LUT_3D_SIZE".to_owned(),
            });
        };
        let expected = (size as usize).pow(3);
        if texels.len() != expected {
            return Err(CubeLutError {
                label,
                line: None,
                message: format!(
                    "LUT {size}³ требует {expected} узлов, найдено {}",
                    texels.len()
                ),
            });
        }
        Ok(Self { size, texels })
    }

    /// Сторона LUT.
    pub fn size(&self) -> u32 {
        self.size
    }

    /// Узлы RGB: красный меняется быстрее всего, затем зелёный и синий.
    pub fn texels(&self) -> &[[f32; 3]] {
        &self.texels
    }
}

fn parse_numbers<const N: usize>(tokens: &[&str]) -> Option<[f32; N]> {
    if tokens.len() != N {
        return None;
    }
    let mut values = [0.0; N];
    for (value, token) in values.iter_mut().zip(tokens) {
        *value = token
            .parse::<f32>()
            .ok()
            .filter(|value| value.is_finite())?;
    }
    Some(values)
}

fn parse_texel(line: &str) -> Option<[f32; 3]> {
    parse_numbers::<3>(&line.split_whitespace().collect::<Vec<_>>())
}

fn require_values(tokens: &[&str], expected: &[f32]) -> Result<(), ()> {
    let matches = tokens.len() == expected.len()
        && tokens
            .iter()
            .zip(expected)
            .all(|(token, expected)| token.parse::<f32>().is_ok_and(|value| value == *expected));
    if matches { Ok(()) } else { Err(()) }
}

/// Ошибка разбора `.cube`: файл, строка (если относится к строке) и описание.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CubeLutError {
    pub label: String,
    pub line: Option<usize>,
    pub message: String,
}

impl fmt::Display for CubeLutError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.line {
            Some(line) => write!(f, "{}:{line}: {}", self.label, self.message),
            None => write!(f, "{}: {}", self.label, self.message),
        }
    }
}

impl Error for CubeLutError {}

#[cfg(test)]
mod tests {
    use super::*;

    fn identity_source(size: u32, header: &str) -> String {
        let mut source = format!("{header}LUT_3D_SIZE {size}\n");
        let scale = (size - 1) as f32;
        for blue in 0..size {
            for green in 0..size {
                for red in 0..size {
                    source.push_str(&format!(
                        "{} {} {}\n",
                        red as f32 / scale,
                        green as f32 / scale,
                        blue as f32 / scale
                    ));
                }
            }
        }
        source
    }

    #[test]
    fn parses_identity_lut_with_red_varying_fastest() {
        let lut = CubeLut::parse("id.cube", &identity_source(3, "")).unwrap();
        assert_eq!(lut.size(), 3);
        assert_eq!(lut.texels().len(), 27);
        assert_eq!(lut.texels()[1], [0.5, 0.0, 0.0]);
        assert_eq!(lut.texels()[3], [0.0, 0.5, 0.0]);
        assert_eq!(lut.texels()[9], [0.0, 0.0, 0.5]);
    }

    #[test]
    fn accepts_resolve_header_comments_bom_and_crlf() {
        let header = "\u{feff}# Created by Resolve\r\nTITLE \"Film\"\r\nDOMAIN_MIN 0 0 0\r\nDOMAIN_MAX 1.0 1.0 1.0\r\nLUT_3D_INPUT_RANGE 0 1\r\n\r\n";
        let source = identity_source(2, header).replace('\n', "\r\n");
        let lut = CubeLut::parse("film.cube", &source).unwrap();
        assert_eq!(lut.size(), 2);
        assert_eq!(lut.texels()[7], [1.0, 1.0, 1.0]);
    }

    #[test]
    fn accepts_values_outside_unit_range() {
        let source = "LUT_3D_SIZE 2\n-0.1 0 0\n1 0 0\n0 1 0\n1 1 0\n0 0 1\n1 0 1\n0 1 1\n1.2 1 1\n";
        assert_eq!(
            CubeLut::parse("hdr.cube", source).unwrap().texels()[7][0],
            1.2
        );
    }

    #[test]
    fn rejects_one_dimensional_luts() {
        let error = CubeLut::parse("curve.cube", "LUT_1D_SIZE 4\n0 0 0\n").unwrap_err();
        assert_eq!(error.line, Some(1));
        assert!(error.message.contains("1D"), "{error}");
    }

    #[test]
    fn rejects_non_unit_domain_and_input_range() {
        for header in [
            "DOMAIN_MIN -0.1 0 0\n",
            "DOMAIN_MAX 1 1 2\n",
            "LUT_3D_INPUT_RANGE 0 4\n",
        ] {
            let error = CubeLut::parse("domain.cube", &identity_source(2, header)).unwrap_err();
            assert_eq!(error.line, Some(1), "{header}");
        }
    }

    #[test]
    fn rejects_sizes_outside_limits_and_duplicates() {
        for size in ["1", "129", "abc", "2 2"] {
            let source = format!("LUT_3D_SIZE {size}\n");
            assert!(CubeLut::parse("size.cube", &source).is_err(), "{size}");
        }
        let error = CubeLut::parse("dup.cube", &identity_source(2, "LUT_3D_SIZE 2\n")).unwrap_err();
        assert_eq!(error.line, Some(2));
    }

    #[test]
    fn rejects_wrong_texel_count_with_file_label() {
        let mut source = identity_source(2, "");
        source.push_str("0 0 0\n");
        let extra = CubeLut::parse("extra.cube", &source).unwrap_err();
        assert_eq!(extra.line, Some(10));
        let missing = CubeLut::parse("missing.cube", "LUT_3D_SIZE 2\n0 0 0\n").unwrap_err();
        assert_eq!(missing.line, None);
        assert!(
            missing.to_string().starts_with("missing.cube: "),
            "{missing}"
        );
    }

    #[test]
    fn rejects_malformed_and_non_finite_texels() {
        for texel in ["0 0", "0 0 0 0", "0 x 0", "nan 0 0", "inf 0 0"] {
            let source = format!("LUT_3D_SIZE 2\n{texel}\n");
            let error = CubeLut::parse("bad.cube", &source).unwrap_err();
            assert_eq!(error.line, Some(2), "{texel}");
        }
    }

    #[test]
    fn rejects_data_before_size_keywords_after_data_and_unknown_keywords() {
        assert_eq!(
            CubeLut::parse("order.cube", "0 0 0\nLUT_3D_SIZE 2\n")
                .unwrap_err()
                .line,
            Some(1)
        );
        let late = format!(
            "{}TITLE \"late\"\n",
            identity_source(2, "").replace("1 1 1\n", "")
        );
        assert!(CubeLut::parse("late.cube", &late).is_err());
        assert_eq!(
            CubeLut::parse("unknown.cube", "LUT_FOO 1\n")
                .unwrap_err()
                .line,
            Some(1)
        );
        assert!(CubeLut::parse("empty.cube", "").is_err());
    }
}
