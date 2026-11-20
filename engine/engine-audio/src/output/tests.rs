use super::*;
use cpal::{SampleFormat, SupportedBufferSize};
use std::time::{Duration, Instant};

fn range(
    channels: u16,
    min_rate: u32,
    max_rate: u32,
    format: SampleFormat,
) -> SupportedStreamConfigRange {
    SupportedStreamConfigRange::new(
        channels,
        min_rate,
        max_rate,
        SupportedBufferSize::Unknown,
        format,
    )
}

fn config(channels: u16, rate: u32, format: SampleFormat) -> SupportedStreamConfig {
    SupportedStreamConfig::new(channels, rate, SupportedBufferSize::Unknown, format)
}

/// Конфигурация по умолчанию, которую микшер не понимает (24-битные сэмплы).
fn unsupported_default(channels: u16, rate: u32) -> Result<SupportedStreamConfig, String> {
    Ok(config(channels, rate, SampleFormat::I24))
}

#[test]
fn default_with_a_supported_format_is_used_as_is() {
    let default = config(6, 96_000, SampleFormat::I16);
    let ranges = vec![range(2, 8_000, 48_000, SampleFormat::F32)];
    assert_eq!(choose_config(Ok(default), Ok(ranges)), Ok(default));
}

#[test]
fn default_with_a_supported_format_does_not_need_the_range_list() {
    let default = config(2, 44_100, SampleFormat::U16);
    assert_eq!(choose_config(Ok(default), Err("нет".into())), Ok(default));
}

#[test]
fn unsupported_default_format_keeps_its_channels_and_rate_in_an_f32_range() {
    let ranges = vec![
        range(2, 8_000, 96_000, SampleFormat::I16),
        range(2, 8_000, 96_000, SampleFormat::F32),
    ];
    assert_eq!(
        choose_config(unsupported_default(2, 44_100), Ok(ranges)),
        Ok(config(2, 44_100, SampleFormat::F32)),
    );
}

#[test]
fn formats_are_preferred_in_f32_i32_i16_u16_order() {
    let pick = |formats: &[SampleFormat]| {
        let ranges = formats
            .iter()
            .map(|format| range(2, 48_000, 48_000, *format))
            .collect();
        choose_config(Err("нет".into()), Ok(ranges)).map(|config| config.sample_format())
    };
    assert_eq!(
        pick(&[SampleFormat::U16, SampleFormat::I16]),
        Ok(SampleFormat::I16)
    );
    assert_eq!(
        pick(&[SampleFormat::U16, SampleFormat::I32]),
        Ok(SampleFormat::I32)
    );
    assert_eq!(
        pick(&[
            SampleFormat::U16,
            SampleFormat::I16,
            SampleFormat::I32,
            SampleFormat::F32
        ]),
        Ok(SampleFormat::F32),
    );
}

#[test]
fn channels_prefer_the_default_then_stereo_then_mono_then_the_rest_ascending() {
    let channels = |default: Option<u16>, counts: &[u16]| {
        let ranges = counts
            .iter()
            .map(|count| range(*count, 48_000, 48_000, SampleFormat::F32))
            .collect();
        let default = default.map_or(Err("нет".into()), |channels| {
            unsupported_default(channels, 48_000)
        });
        choose_config(default, Ok(ranges)).map(|config| config.channels())
    };
    assert_eq!(channels(Some(6), &[4, 1, 8]), Ok(1), "моно раньше прочих");
    assert_eq!(channels(None, &[4, 8]), Ok(4), "прочие по возрастанию");
    assert_eq!(channels(None, &[1, 2]), Ok(2), "стерео раньше моно");
    assert_eq!(
        channels(Some(6), &[2, 6]),
        Ok(6),
        "каналы устройства раньше стерео"
    );
}

#[test]
fn rate_prefers_the_default_then_48000_then_44100_then_the_maximum() {
    let rate = |default: Option<u32>, min: u32, max: u32| {
        let ranges = vec![range(2, min, max, SampleFormat::F32)];
        let default = default.map_or(Err("нет".into()), |rate| unsupported_default(2, rate));
        choose_config(default, Ok(ranges)).map(|config| config.sample_rate())
    };
    assert_eq!(
        rate(Some(22_050), 8_000, 96_000),
        Ok(22_050),
        "частота устройства в диапазоне"
    );
    assert_eq!(
        rate(Some(192_000), 8_000, 96_000),
        Ok(48_000),
        "вне диапазона — 48000"
    );
    assert_eq!(rate(None, 8_000, 96_000), Ok(48_000));
    assert_eq!(
        rate(None, 8_000, 44_100),
        Ok(44_100),
        "48000 вне диапазона — 44100"
    );
    assert_eq!(
        rate(None, 8_000, 32_000),
        Ok(32_000),
        "ни одной предпочитаемой — максимум"
    );
}

#[test]
fn among_equal_candidates_the_first_one_in_the_list_wins() {
    let first = range(2, 8_000, 44_100, SampleFormat::F32);
    let second = range(2, 8_000, 48_000, SampleFormat::F32);
    let rate = |ranges: Vec<SupportedStreamConfigRange>| {
        choose_config(Err("нет".into()), Ok(ranges)).map(|config| config.sample_rate())
    };
    assert_eq!(rate(vec![first, second]), Ok(44_100));
    assert_eq!(rate(vec![second, first]), Ok(48_000));
}

#[test]
fn a_failed_default_query_does_not_block_a_supported_range() {
    let ranges = vec![range(2, 48_000, 48_000, SampleFormat::F32)];
    assert_eq!(
        choose_config(Err("сбой по умолчанию".into()), Ok(ranges)),
        Ok(config(2, 48_000, SampleFormat::F32)),
    );
}

#[test]
fn failed_queries_are_named_in_the_error_when_nothing_fits() {
    let error = choose_config(Err("сбой A".into()), Err("сбой B".into())).unwrap_err();
    assert!(error.contains("сбой A"), "{error}");
    assert!(error.contains("сбой B"), "{error}");
}

#[test]
fn unsupported_formats_are_listed_in_the_error() {
    let ranges = vec![
        range(2, 48_000, 48_000, SampleFormat::F64),
        range(2, 48_000, 48_000, SampleFormat::I8),
    ];
    let error = choose_config(unsupported_default(2, 48_000), Ok(ranges)).unwrap_err();
    assert!(error.contains("I8"), "{error}");
    assert!(error.contains("F64"), "{error}");
    assert!(!error.contains("F32"), "{error}");
}

#[test]
fn a_device_without_any_configuration_is_an_error() {
    let error = choose_config(Err("сбой".into()), Ok(Vec::new())).unwrap_err();
    assert!(error.contains("нет конфигураций вывода"), "{error}");
}

/// Задержки, которые `Reconnect` ставит после каждой подряд идущей неудачи.
fn waits_after_failures(count: usize) -> Vec<Duration> {
    let now = Instant::now();
    let mut reconnect = Reconnect::default();
    reconnect.schedule(now);
    (0..count)
        .map(|_| {
            reconnect.failed(now);
            reconnect.wait(now).expect("попытка запланирована")
        })
        .collect()
}

#[test]
fn the_first_attempt_after_a_loss_is_immediate_then_the_delays_double_up_to_five_seconds() {
    let ms = Duration::from_millis;
    assert_eq!(
        waits_after_failures(8),
        [
            ms(250),
            ms(500),
            ms(1_000),
            ms(2_000),
            ms(4_000),
            ms(5_000),
            ms(5_000),
            ms(5_000),
        ]
    );
    let now = Instant::now();
    let mut reconnect = Reconnect::default();
    reconnect.schedule(now);
    assert_eq!(reconnect.wait(now), Some(Duration::ZERO));
    assert!(reconnect.due(now));
}

#[test]
fn an_attempt_is_due_only_once_its_delay_has_passed() {
    let now = Instant::now();
    let mut reconnect = Reconnect::default();
    reconnect.failed(now);
    let later = now + Duration::from_millis(249);
    assert!(!reconnect.due(later));
    assert!(reconnect.due(now + Duration::from_millis(250)));
}

#[test]
fn a_running_stream_resets_the_delays_after_a_loss() {
    let now = Instant::now();
    let mut reconnect = Reconnect::default();
    for _ in 0..3 {
        reconnect.failed(now);
    }
    reconnect.opened();
    reconnect.alive();
    reconnect.schedule(now);
    assert_eq!(reconnect.wait(now), Some(Duration::ZERO));
}

#[test]
fn a_stream_that_dies_before_its_first_callback_keeps_the_delays_growing() {
    let now = Instant::now();
    let mut reconnect = Reconnect::default();
    reconnect.schedule(now);
    reconnect.opened();
    assert_eq!(
        reconnect.wait(now),
        None,
        "после открытия попытка не запланирована"
    );
    reconnect.schedule(now);
    assert_eq!(reconnect.wait(now), Some(Duration::from_millis(250)));
}
