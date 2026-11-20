//! Вывод на устройство по умолчанию через `cpal`. Тонкая обёртка: вся логика в [`Mixer`].
//!
//! Callback не выделяет память, не освобождает память и не берёт блокировок: буфер для
//! конвертации создаётся до запуска потока, команды и завершённые голоса идут через
//! SPSC-кольца `rtrb`; кольца завершённых стрим-голосов возвращаются потоку `rve-audio`.
//! Команды читаются пачками по [`COMMANDS_PER_CALLBACK`]; завершённые
//! голоса, которые не влезли в кольцо, остаются в микшере до следующего вызова.
//!
//! Callback сообщает потоку `rve-audio` двумя атомарными флагами в [`Signals`]: потеря устройства
//! и первый отработавший блок. По ним [`Reconnect`] решает, когда открыть устройство снова.

use crate::mixer::{Mixer, MixerCommand, Spatial};
use crate::stream::StreamReader;
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{
    ErrorKind, FromSample, SampleFormat, SizedSample, StreamConfig, SupportedStreamConfig,
    SupportedStreamConfigRange,
};
use engine_scene::VoiceId;
use rtrb::{Consumer, Producer, PushError};
use std::collections::BTreeSet;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

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
/// Задержка перед второй попыткой открытия; дальше она удваивается до [`RETRY_CAP`].
const RETRY_BASE: Duration = Duration::from_millis(250);
const RETRY_CAP: Duration = Duration::from_secs(5);

/// Открытый выходной поток. Дроп останавливает устройство.
pub struct Output {
    _stream: cpal::Stream,
}

/// Концы колец одного открытия: callback читает `commands`, пишет `ended` и `retired`.
pub(crate) struct Channels {
    pub(crate) commands: Consumer<MixerCommand>,
    pub(crate) ended: Producer<VoiceId>,
    pub(crate) retired: Producer<StreamReader>,
}

/// Флаги, которые callback выставляет для потока `rve-audio`. Оба пишутся только через `store`.
#[derive(Clone, Default)]
pub(crate) struct Signals {
    /// Устройство пропало или поток остановлен системой: открыть заново.
    pub(crate) lost: Arc<AtomicBool>,
    /// Callback выполнился хотя бы раз: поток работает.
    pub(crate) alive: Arc<AtomicBool>,
}

/// Открывает устройство вывода по умолчанию и запускает поток. Ошибка не паникует:
/// вызывающий код выводит предупреждение и повторяет попытку через [`Reconnect`].
pub fn open(
    commands: Consumer<MixerCommand>,
    ended: Producer<VoiceId>,
    retired: Producer<StreamReader>,
    spatial: Arc<Spatial>,
) -> Result<Output, String> {
    let channels = Channels {
        commands,
        ended,
        retired,
    };
    open_device(channels, spatial, Signals::default())
}

/// Как [`open`], но сигналы callback-а задаёт вызывающий: поток `rve-audio` по ним видит потерю.
pub(crate) fn open_device(
    channels: Channels,
    spatial: Arc<Spatial>,
    signals: Signals,
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
        commands: channels.commands,
        ended: channels.ended,
        retired: channels.retired,
    };
    let stream = match config.sample_format() {
        SampleFormat::F32 => build::<f32>(&device, stream_config, state, signals),
        SampleFormat::I32 => build::<i32>(&device, stream_config, state, signals),
        SampleFormat::I16 => build::<i16>(&device, stream_config, state, signals),
        SampleFormat::U16 => build::<u16>(&device, stream_config, state, signals),
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

/// Расписание повторных открытий. Время передаётся аргументами, поэтому тесты не ждут.
///
/// Попытки считаются с последнего отработавшего callback-а. После потери первая попытка сразу,
/// затем задержки 0.25, 0.5, 1, 2, 4, 5, 5… с. Открытие без отработавшего callback-а не сбрасывает
/// счёт: поток, который сразу падает, не даёт частых попыток.
#[derive(Debug, Default)]
pub(crate) struct Reconnect {
    /// Попытки открытия с последнего отработавшего callback-а.
    attempts: u32,
    /// Момент следующей попытки; `None` — попытка не запланирована.
    next: Option<Instant>,
}

impl Reconnect {
    /// Планирует попытку через задержку, которая положена по числу неудач.
    pub(crate) fn schedule(&mut self, now: Instant) {
        self.next = Some(now + retry_delay(self.attempts));
    }

    /// Открытие не удалось: счёт растёт, следующая попытка планируется заново.
    pub(crate) fn failed(&mut self, now: Instant) {
        self.attempts = self.attempts.saturating_add(1);
        self.schedule(now);
    }

    /// Открытие удалось, но поток ещё не доказал работу: счёт растёт, попытка не запланирована.
    pub(crate) fn opened(&mut self) {
        self.attempts = self.attempts.saturating_add(1);
        self.next = None;
    }

    /// Callback отработал: счёт сбрасывается.
    pub(crate) fn alive(&mut self) {
        self.attempts = 0;
    }

    pub(crate) fn due(&self, now: Instant) -> bool {
        self.next.is_some_and(|next| now >= next)
    }

    /// Сколько ждать до следующей попытки; `None`, если попытка не запланирована.
    pub(crate) fn wait(&self, now: Instant) -> Option<Duration> {
        self.next.map(|next| next.saturating_duration_since(now))
    }
}

/// Задержка перед попыткой после `attempts` неудач: 0, затем 0.25 с, удваиваясь до [`RETRY_CAP`].
fn retry_delay(attempts: u32) -> Duration {
    if attempts == 0 {
        return Duration::ZERO;
    }
    let shift = (attempts - 1).min(16);
    RETRY_BASE.saturating_mul(1 << shift).min(RETRY_CAP)
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
    signals: Signals,
) -> Result<cpal::Stream, cpal::Error>
where
    T: SizedSample + FromSample<f32>,
{
    let channels = usize::from(config.channels);
    let mut scratch = vec![0.0f32; SCRATCH_FRAMES * channels];
    let alive = signals.alive;
    let lost = signals.lost;
    device.build_output_stream::<T, _, _>(
        config,
        move |data: &mut [T], _info: &cpal::OutputCallbackInfo| {
            alive.store(true, Ordering::Release);
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
        move |error: cpal::Error| report_error(&error, &lost),
        None,
    )
}

/// Ошибка потока из callback-а: только флаг потери и журнал, без ожиданий и очередей.
fn report_error(error: &cpal::Error, lost: &AtomicBool) {
    match error.kind() {
        ErrorKind::DeviceNotAvailable | ErrorKind::StreamInvalidated => {
            lost.store(true, Ordering::Release);
            log::warn!("аудиоустройство потеряно: {error}");
        }
        // macOS перенаправляет поток на новое устройство по умолчанию сам: открывать заново не нужно.
        ErrorKind::DeviceChanged => log::info!("устройство вывода по умолчанию сменилось: {error}"),
        _ => log::warn!("ошибка аудиопотока: {error}"),
    }
}

#[cfg(test)]
mod tests;
