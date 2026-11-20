//! Вывод на устройство по умолчанию через `cpal`. Тонкая обёртка: вся логика в [`Mixer`].
//!
//! Callback не выделяет память, не освобождает память и не берёт блокировок: буфер для
//! конвертации создаётся до запуска потока, команды и завершённые голоса идут через
//! SPSC-кольца `rtrb`; кольца завершённых стрим-голосов возвращаются потоку `rve-audio`.
//! Команды читаются пачками по [`COMMANDS_PER_CALLBACK`]; завершённые
//! голоса, которые не влезли в кольцо, остаются в микшере до следующего вызова.
//!

use crate::mixer::{Mixer, MixerCommand, Spatial};
use crate::stream::StreamReader;
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{
    FromSample, SampleFormat, SizedSample, StreamConfig, SupportedStreamConfig,
    SupportedStreamConfigRange,
};
use engine_scene::VoiceId;
use rtrb::{Consumer, Producer, PushError};
use std::collections::BTreeSet;
use std::sync::Arc;

/// Кадров, которые callback конвертирует за один проход через scratch-буфер.
const SCRATCH_FRAMES: usize = 8192;
/// Команд, которые callback забирает из очереди за один вызов.
const COMMANDS_PER_CALLBACK: usize = 256;
/// Форматы сэмплов, которые понимает микшер, от предпочтительного.
const FORMATS: [SampleFormat; 4] = [
    SampleFormat::F32,
    SampleFormat::I32,
    SampleFormat::I16,
    SampleFormat::U16,
];
/// Частоты, которые предпочитаются, если у устройства по умолчанию другая.
const PREFERRED_RATES: [u32; 2] = [48_000, 44_100];

/// Открытый выходной поток. Дроп останавливает устройство.
pub struct Output {
    _stream: cpal::Stream,
}

/// Открывает устройство вывода по умолчанию и запускает поток. Ошибка не паникует:
/// вызывающий код выводит предупреждение и работает без звука.
pub fn open(
    commands: Consumer<MixerCommand>,
    ended: Producer<VoiceId>,
    retired: Producer<StreamReader>,
    spatial: Arc<Spatial>,
) -> Result<Output, String> {
    let host = cpal::default_host();
    let device = host
        .default_output_device()
        .ok_or_else(|| "устройство вывода не найдено".to_owned())?;
    let default = device
        .default_output_config()
        .map_err(|error| error.to_string());
    let ranges = device
        .supported_output_configs()
        .map(|ranges| ranges.collect::<Vec<_>>())
        .map_err(|error| error.to_string());
    let config = choose_config(default, ranges)?;
    let stream_config = config.config();
    let mixer = Mixer::new(stream_config.sample_rate, spatial);
    let state = CallbackState {
        mixer,
        commands,
        ended,
        retired,
    };
    let stream = match config.sample_format() {
        SampleFormat::F32 => build::<f32>(&device, stream_config, state),
        SampleFormat::I32 => build::<i32>(&device, stream_config, state),
        SampleFormat::I16 => build::<i16>(&device, stream_config, state),
        SampleFormat::U16 => build::<u16>(&device, stream_config, state),
        // `choose_config` возвращает только форматы из `FORMATS`; ветка нужна для `non_exhaustive`.
        other => return Err(format!("формат сэмплов {other:?} не поддерживается")),
    }
    .map_err(|error| format!("поток вывода: {error}"))?;
    stream
        .play()
        .map_err(|error| format!("запуск потока: {error}"))?;
    Ok(Output { _stream: stream })
}

/// Выбирает конфигурацию вывода. Конфигурация по умолчанию берётся как есть, если её формат
/// понимает микшер. Иначе из `ranges` выбирается по порядку: формат ([`FORMATS`]), каналы
/// (как у default, затем 2, 1, остальные по возрастанию), частота (как у default, затем
/// [`PREFERRED_RATES`], затем максимальная). При равенстве побеждает первый в `ranges`.
/// Ошибки `default` и перечисления попадают в текст ошибки, если подходящий формат не найден.
pub(crate) fn choose_config(
    default: Result<SupportedStreamConfig, String>,
    ranges: Result<Vec<SupportedStreamConfigRange>, String>,
) -> Result<SupportedStreamConfig, String> {
    let mut problems = Vec::new();
    let default = match default {
        Ok(config) if supported(config.sample_format()) => return Ok(config),
        Ok(config) => Some(config),
        Err(error) => {
            problems.push(format!("конфигурация устройства: {error}"));
            None
        }
    };
    let ranges = ranges.unwrap_or_else(|error| {
        problems.push(format!("перечисление форматов: {error}"));
        Vec::new()
    });
    let default_channels = default.map(|config| config.channels());
    let candidate = ranges
        .iter()
        .filter(|range| supported(range.sample_format()))
        .min_by_key(|range| {
            (
                format_rank(range.sample_format()),
                channel_rank(range.channels(), default_channels),
            )
        });
    let Some(range) = candidate else {
        let offered: BTreeSet<SampleFormat> =
            ranges.iter().map(|range| range.sample_format()).collect();
        problems.push(if offered.is_empty() {
            "нет конфигураций вывода".to_owned()
        } else {
            let offered = offered
                .iter()
                .map(|format| format!("{format:?}"))
                .collect::<Vec<_>>()
                .join(", ");
            format!("ни один формат не понимает микшер; устройство предлагает: {offered}")
        });
        return Err(problems.join("; "));
    };
    let rate = pick_rate(range, default.map(|config| config.sample_rate()));
    Ok(SupportedStreamConfig::new(
        range.channels(),
        rate,
        *range.buffer_size(),
        range.sample_format(),
    ))
}

fn supported(format: SampleFormat) -> bool {
    FORMATS.contains(&format)
}

fn format_rank(format: SampleFormat) -> usize {
    FORMATS
        .iter()
        .position(|candidate| *candidate == format)
        .unwrap_or(FORMATS.len())
}

/// Сначала каналы устройства по умолчанию, затем стерео, моно и остальные по возрастанию.
fn channel_rank(channels: u16, default: Option<u16>) -> (u8, u16) {
    let class = if Some(channels) == default {
        0
    } else {
        match channels {
            2 => 1,
            1 => 2,
            _ => 3,
        }
    };
    (class, channels)
}

fn pick_rate(range: &SupportedStreamConfigRange, default: Option<u32>) -> u32 {
    let (min, max) = (range.min_sample_rate(), range.max_sample_rate());
    let within = |rate: &u32| (min..=max).contains(rate);
    default
        .filter(within)
        .or_else(|| PREFERRED_RATES.into_iter().find(within))
        .unwrap_or(max)
}

/// Состояние, которое владеет callback-ом: микшер и концы колец.
struct CallbackState {
    mixer: Mixer,
    commands: Consumer<MixerCommand>,
    ended: Producer<VoiceId>,
    retired: Producer<StreamReader>,
}

fn build<T>(
    device: &cpal::Device,
    config: StreamConfig,
    mut state: CallbackState,
) -> Result<cpal::Stream, cpal::Error>
where
    T: SizedSample + FromSample<f32>,
{
    let channels = usize::from(config.channels);
    let mut scratch = vec![0.0f32; SCRATCH_FRAMES * channels];
    device.build_output_stream::<T, _, _>(
        config,
        move |data: &mut [T], _info: &cpal::OutputCallbackInfo| {
            for _ in 0..COMMANDS_PER_CALLBACK {
                match state.commands.pop() {
                    Ok(command) => state.mixer.apply(command),
                    Err(_) => break,
                }
            }
            for chunk in data.chunks_mut(SCRATCH_FRAMES * channels) {
                let buffer = &mut scratch[..chunk.len()];
                state.mixer.render(buffer, channels);
                for (sample, value) in chunk.iter_mut().zip(buffer.iter()) {
                    *sample = T::from_sample(*value);
                }
            }
            let CallbackState {
                mixer,
                ended,
                retired,
                ..
            } = &mut state;
            mixer.drain_ended(|voice| ended.push(voice).is_ok());
            mixer.drain_retired(|reader| {
                retired
                    .push(reader)
                    .map_err(|PushError::Full(reader)| reader)
            });
        },
        |error| log::warn!("ошибка аудиопотока: {error}"),
        None,
    )
}

#[cfg(test)]
mod tests;
