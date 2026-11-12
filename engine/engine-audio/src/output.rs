//! Вывод на устройство по умолчанию через `cpal`. Тонкая обёртка: вся логика в [`Mixer`].
//!
//! Callback не выделяет память, не освобождает память и не берёт блокировок: буфер для
//! конвертации создаётся до запуска потока, команды и завершённые голоса идут через
//! SPSC-кольца `rtrb`; кольца завершённых стрим-голосов возвращаются потоку `rve-audio`.
//! Команды читаются пачками по [`COMMANDS_PER_CALLBACK`]; завершённые
//! голоса, которые не влезли в кольцо, остаются в микшере до следующего вызова.

use crate::mixer::{Mixer, MixerCommand, Spatial};
use crate::stream::StreamReader;
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{FromSample, SampleFormat, SizedSample, StreamConfig};
use engine_scene::VoiceId;
use rtrb::{Consumer, Producer};
use std::sync::Arc;

/// Кадров, которые callback конвертирует за один проход через scratch-буфер.
const SCRATCH_FRAMES: usize = 8192;
/// Команд, которые callback забирает из очереди за один вызов.
const COMMANDS_PER_CALLBACK: usize = 256;

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
    let supported = device
        .default_output_config()
        .map_err(|error| format!("конфигурация устройства: {error}"))?;
    let config = supported.config();
    let mixer = Mixer::new(config.sample_rate, spatial);
    let state = CallbackState {
        mixer,
        commands,
        ended,
        retired,
    };
    let stream = match supported.sample_format() {
        SampleFormat::F32 => build::<f32>(&device, config, state),
        SampleFormat::I16 => build::<i16>(&device, config, state),
        SampleFormat::U16 => build::<u16>(&device, config, state),
        SampleFormat::I32 => build::<i32>(&device, config, state),
        other => return Err(format!("формат сэмплов {other:?} не поддерживается")),
    }
    .map_err(|error| format!("поток вывода: {error}"))?;
    stream
        .play()
        .map_err(|error| format!("запуск потока: {error}"))?;
    Ok(Output { _stream: stream })
}

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
                    .map_err(|rtrb::PushError::Full(reader)| reader)
            });
        },
        |error| log::warn!("ошибка аудиопотока: {error}"),
        None,
    )
}
