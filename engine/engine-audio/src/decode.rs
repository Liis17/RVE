//! Декодирование клипов в interleaved f32: свой разбор WAV и Ogg Vorbis через `lewton`.
//!
//! Поддерживаются моно и стерео. Декодированный клип ограничен [`MAX_CLIP_SAMPLES`]:
//! для WAV размер проверяется по заголовку до аллокации, для Ogg — по мере декодирования.
//! Сэмплы пишутся сразу в итоговый `Vec`; клип в кеше хранится за `Arc`, поэтому копии нет.

use lewton::inside_ogg::OggStreamReader;
use std::fmt;
use std::io::Cursor;
use std::sync::atomic::{AtomicBool, Ordering};

/// Наибольший декодированный клип в сэмплах f32 (256 МБ).
pub const MAX_CLIP_SAMPLES: usize = 64 * 1024 * 1024;

/// Наибольший файл клипа, который читается целиком. Сэмплы занимают 4 байта, поэтому файл больше
/// этого размера не уложится в [`MAX_CLIP_SAMPLES`] даже как 32-битный float.
pub const MAX_CLIP_FILE_BYTES: u64 = (MAX_CLIP_SAMPLES * std::mem::size_of::<f32>()) as u64;

/// Декодированный клип; `samples` — interleaved, `channels` — 1 или 2.
#[derive(Clone, Debug, PartialEq)]
pub struct Clip {
    pub sample_rate: u32,
    pub channels: u16,
    pub samples: Vec<f32>,
}

impl Clip {
    /// Число кадров (сэмплов на канал).
    pub fn frames(&self) -> usize {
        self.samples.len() / usize::from(self.channels)
    }

    /// Размер сэмплов в памяти; используется для бюджета кеша.
    pub fn bytes(&self) -> usize {
        self.samples.len() * std::mem::size_of::<f32>()
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DecodeError {
    Unsupported(String),
    Corrupt(String),
    TooLarge { samples: u64 },
    /// Декодирование прервано: приложение завершает звуковой поток.
    Cancelled,
}

impl fmt::Display for DecodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unsupported(message) => write!(f, "неподдерживаемый аудиоформат: {message}"),
            Self::Corrupt(message) => write!(f, "повреждённый аудиофайл: {message}"),
            Self::TooLarge { samples } => write!(
                f,
                "клип из {samples} сэмплов больше лимита {MAX_CLIP_SAMPLES}"
            ),
            Self::Cancelled => write!(f, "декодирование отменено"),
        }
    }
}

impl std::error::Error for DecodeError {}

/// Декодирует клип по расширению файла (`wav` или `ogg`, без учёта регистра).
/// `cancel` проверяется между пакетами Ogg; WAV разбирается за один проход.
pub fn decode(extension: &str, bytes: &[u8], cancel: &AtomicBool) -> Result<Clip, DecodeError> {
    match extension.to_ascii_lowercase().as_str() {
        "wav" => decode_wav(bytes),
        "ogg" => decode_ogg(bytes, cancel),
        other => Err(DecodeError::Unsupported(format!(
            "расширение .{other} не поддерживается"
        ))),
    }
}

#[derive(Clone, Copy)]
enum WavSamples {
    Pcm { bits: u16 },
    Float,
}

#[derive(Clone, Copy)]
struct WavFormat {
    samples: WavSamples,
    channels: u16,
    sample_rate: u32,
    bytes_per_sample: usize,
}

/// Разбирает RIFF/WAVE: PCM 8/16/24/32 бит и float32, включая WAVE_FORMAT_EXTENSIBLE.
pub fn decode_wav(bytes: &[u8]) -> Result<Clip, DecodeError> {
    if bytes.len() < 12 || &bytes[0..4] != b"RIFF" || &bytes[8..12] != b"WAVE" {
        return Err(corrupt("нет заголовка RIFF/WAVE"));
    }
    let mut format = None;
    let mut position = 12;
    while position + 8 <= bytes.len() {
        let id = &bytes[position..position + 4];
        let size = u32_le(bytes, position + 4) as usize;
        let body = position + 8;
        let available = bytes.len() - body;
        if id == b"fmt " {
            if size > available || size < 16 {
                return Err(corrupt("обрезанный chunk fmt"));
            }
            format = Some(parse_fmt(&bytes[body..body + size])?);
        } else if id == b"data" {
            let format = format.ok_or_else(|| corrupt("chunk data раньше fmt"))?;
            if size > available {
                return Err(corrupt(&format!(
                    "data заявлено {size} байт, в файле {available}"
                )));
            }
            return read_data(&bytes[body..body + size], format);
        }
        position = body
            .checked_add(size)
            .and_then(|end| end.checked_add(size & 1))
            .ok_or_else(|| corrupt("размер chunk переполняет файл"))?;
    }
    Err(corrupt("нет chunk data"))
}

fn parse_fmt(body: &[u8]) -> Result<WavFormat, DecodeError> {
    let mut tag = u16::from_le_bytes([body[0], body[1]]);
    let channels = u16::from_le_bytes([body[2], body[3]]);
    let sample_rate = u32_le(body, 4);
    let bits = u16::from_le_bytes([body[14], body[15]]);
    if tag == 0xFFFE {
        // WAVE_FORMAT_EXTENSIBLE: настоящий формат — первые два байта GUID подформата.
        if body.len() < 26 {
            return Err(corrupt("обрезанный extensible fmt"));
        }
        tag = u16::from_le_bytes([body[24], body[25]]);
    }
    if !(1..=2).contains(&channels) {
        return Err(DecodeError::Unsupported(format!(
            "{channels} каналов; поддерживаются моно и стерео"
        )));
    }
    if sample_rate == 0 {
        return Err(corrupt("частота дискретизации 0"));
    }
    let samples = match (tag, bits) {
        (1, 8 | 16 | 24 | 32) => WavSamples::Pcm { bits },
        (3, 32) => WavSamples::Float,
        _ => {
            return Err(DecodeError::Unsupported(format!(
                "формат тега {tag:#x} с {bits} битами"
            )));
        }
    };
    Ok(WavFormat {
        samples,
        channels,
        sample_rate,
        bytes_per_sample: usize::from(bits / 8),
    })
}

fn read_data(data: &[u8], format: WavFormat) -> Result<Clip, DecodeError> {
    let frame_bytes = format.bytes_per_sample * usize::from(format.channels);
    if !data.len().is_multiple_of(frame_bytes) {
        return Err(corrupt("data не делится на целое число кадров"));
    }
    let samples = data.len() / format.bytes_per_sample;
    if samples > MAX_CLIP_SAMPLES {
        return Err(DecodeError::TooLarge {
            samples: samples as u64,
        });
    }
    let mut out = Vec::with_capacity(samples);
    for chunk in data.chunks_exact(format.bytes_per_sample) {
        out.push(convert(chunk, format.samples)?);
    }
    Ok(Clip {
        sample_rate: format.sample_rate,
        channels: format.channels,
        samples: out,
    })
}

fn convert(chunk: &[u8], kind: WavSamples) -> Result<f32, DecodeError> {
    Ok(match kind {
        WavSamples::Pcm { bits: 8 } => (f32::from(chunk[0]) - 128.0) / 128.0,
        WavSamples::Pcm { bits: 16 } => {
            f32::from(i16::from_le_bytes([chunk[0], chunk[1]])) / 32768.0
        }
        WavSamples::Pcm { bits: 24 } => {
            let raw =
                i32::from(chunk[0]) | (i32::from(chunk[1]) << 8) | (i32::from(chunk[2]) << 16);
            let signed = (raw << 8) >> 8;
            signed as f32 / 8_388_608.0
        }
        WavSamples::Pcm { .. } => {
            i32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]) as f32 / 2_147_483_648.0
        }
        WavSamples::Float => {
            let value = f32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]);
            if !value.is_finite() {
                return Err(corrupt("нечисловой float-сэмпл"));
            }
            value
        }
    })
}

/// Декодирует Ogg Vorbis; пакеты добавляются по одному, лимит проверяется до роста буфера,
/// отмена — между пакетами.
pub fn decode_ogg(bytes: &[u8], cancel: &AtomicBool) -> Result<Clip, DecodeError> {
    let mut reader = OggStreamReader::new(Cursor::new(bytes))
        .map_err(|error| corrupt(&format!("ogg: {error}")))?;
    let channels = u16::from(reader.ident_hdr.audio_channels);
    let sample_rate = reader.ident_hdr.audio_sample_rate;
    if !(1..=2).contains(&channels) {
        return Err(DecodeError::Unsupported(format!(
            "{channels} каналов; поддерживаются моно и стерео"
        )));
    }
    let mut out: Vec<f32> = Vec::new();
    loop {
        if cancel.load(Ordering::Relaxed) {
            return Err(DecodeError::Cancelled);
        }
        let packet = match reader.read_dec_packet_generic::<Vec<Vec<f32>>>() {
            Ok(Some(packet)) => packet,
            Ok(None) => break,
            Err(error) => return Err(corrupt(&format!("ogg: {error}"))),
        };
        if packet.len() != usize::from(channels) {
            return Err(corrupt("число каналов пакета не совпадает с заголовком"));
        }
        let frames = packet.first().map_or(0, Vec::len);
        let added = frames * usize::from(channels);
        if out.len() + added > MAX_CLIP_SAMPLES {
            return Err(DecodeError::TooLarge {
                samples: (out.len() + added) as u64,
            });
        }
        out.reserve(added);
        for frame in 0..frames {
            for channel in &packet {
                out.push(channel[frame]);
            }
        }
    }
    // Буфер рос по пакетам и мог получить запас ёмкости; клип в кеше держит его весь век.
    out.shrink_to_fit();
    Ok(Clip {
        sample_rate,
        channels,
        samples: out,
    })
}

fn u32_le(bytes: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]])
}

fn corrupt(message: &str) -> DecodeError {
    DecodeError::Corrupt(message.to_owned())
}

#[cfg(test)]
mod tests;
