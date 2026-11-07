use super::*;

const OGG_FIXTURE: &[u8] = include_bytes!("../../tests/fixtures/tone_mono_22050.ogg");

fn live() -> AtomicBool {
    AtomicBool::new(false)
}

/// Собирает WAV: `format_tag` — тег формата, `data` — сырые байты данных.
fn wav(format_tag: u16, channels: u16, rate: u32, bits: u16, data: &[u8]) -> Vec<u8> {
    let block_align = channels * bits / 8;
    let mut out = Vec::new();
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&(36 + data.len() as u32).to_le_bytes());
    out.extend_from_slice(b"WAVE");
    out.extend_from_slice(b"fmt ");
    out.extend_from_slice(&16u32.to_le_bytes());
    out.extend_from_slice(&format_tag.to_le_bytes());
    out.extend_from_slice(&channels.to_le_bytes());
    out.extend_from_slice(&rate.to_le_bytes());
    out.extend_from_slice(&(rate * u32::from(block_align)).to_le_bytes());
    out.extend_from_slice(&block_align.to_le_bytes());
    out.extend_from_slice(&bits.to_le_bytes());
    out.extend_from_slice(b"data");
    out.extend_from_slice(&(data.len() as u32).to_le_bytes());
    out.extend_from_slice(data);
    out
}

#[test]
fn pcm16_mono_decodes_to_normalised_samples() {
    let data: Vec<u8> = [0i16, 16384, -32768, 32767]
        .iter()
        .flat_map(|value| value.to_le_bytes())
        .collect();
    let clip = decode_wav(&wav(1, 1, 22_050, 16, &data)).unwrap();
    assert_eq!(clip.sample_rate, 22_050);
    assert_eq!(clip.channels, 1);
    assert_eq!(clip.frames(), 4);
    assert_eq!(&clip.samples[..], &[0.0, 0.5, -1.0, 32767.0 / 32768.0]);
}

#[test]
fn pcm8_is_unsigned_centred_on_128() {
    let clip = decode_wav(&wav(1, 1, 8_000, 8, &[128, 255, 0])).unwrap();
    assert_eq!(clip.samples[0], 0.0);
    assert!((clip.samples[1] - 127.0 / 128.0).abs() < 1e-6);
    assert_eq!(clip.samples[2], -1.0);
}

#[test]
fn pcm24_sign_extends_three_byte_samples() {
    let data = [0x00, 0x00, 0x80, 0xFF, 0xFF, 0x7F];
    let clip = decode_wav(&wav(1, 1, 8_000, 24, &data)).unwrap();
    assert_eq!(clip.samples[0], -1.0);
    assert!((clip.samples[1] - 8_388_607.0 / 8_388_608.0).abs() < 1e-7);
}

#[test]
fn pcm32_and_float32_stereo_keep_interleaving() {
    let pcm: Vec<u8> = [i32::MIN, 0, i32::MAX, 0]
        .iter()
        .flat_map(|value| value.to_le_bytes())
        .collect();
    let clip = decode_wav(&wav(1, 2, 44_100, 32, &pcm)).unwrap();
    assert_eq!(clip.channels, 2);
    assert_eq!(clip.frames(), 2);
    assert_eq!(clip.samples[0], -1.0);
    assert!((clip.samples[2] - 1.0).abs() < 1e-6);

    let float: Vec<u8> = [0.25f32, -0.5]
        .iter()
        .flat_map(|value| value.to_le_bytes())
        .collect();
    let clip = decode_wav(&wav(3, 1, 48_000, 32, &float)).unwrap();
    assert_eq!(&clip.samples[..], &[0.25, -0.5]);
}

#[test]
fn extensible_header_uses_the_subformat_tag() {
    let mut fmt = Vec::new();
    fmt.extend_from_slice(&0xFFFEu16.to_le_bytes());
    fmt.extend_from_slice(&1u16.to_le_bytes());
    fmt.extend_from_slice(&22_050u32.to_le_bytes());
    fmt.extend_from_slice(&(22_050u32 * 2).to_le_bytes());
    fmt.extend_from_slice(&2u16.to_le_bytes());
    fmt.extend_from_slice(&16u16.to_le_bytes());
    fmt.extend_from_slice(&22u16.to_le_bytes());
    fmt.extend_from_slice(&16u16.to_le_bytes());
    fmt.extend_from_slice(&4u32.to_le_bytes());
    fmt.extend_from_slice(&1u16.to_le_bytes());
    fmt.extend_from_slice(&[0u8; 14]);
    let mut out = Vec::new();
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&0u32.to_le_bytes());
    out.extend_from_slice(b"WAVE");
    out.extend_from_slice(b"fmt ");
    out.extend_from_slice(&(fmt.len() as u32).to_le_bytes());
    out.extend_from_slice(&fmt);
    out.extend_from_slice(b"data");
    out.extend_from_slice(&4u32.to_le_bytes());
    out.extend_from_slice(&[0, 0x40, 0, 0x80]);
    let clip = decode_wav(&out).unwrap();
    assert_eq!(clip.channels, 1);
    assert_eq!(clip.frames(), 2);
    assert!((clip.samples[0] - 0.5).abs() < 1e-6);
    assert_eq!(clip.samples[1], -1.0);
}

#[test]
fn unknown_chunks_before_data_are_skipped() {
    let data = [128u8, 128];
    let mut bytes = wav(1, 1, 8_000, 8, &data);
    let list = b"LIST\x03\x00\x00\x00abc\x00";
    bytes.splice(36..36, list.iter().copied());
    let clip = decode_wav(&bytes).unwrap();
    assert_eq!(clip.frames(), 2);
}

#[test]
fn data_header_larger_than_the_file_is_rejected_before_allocation() {
    let mut bytes = wav(1, 1, 8_000, 16, &[0u8; 8]);
    // Поле размера chunk data стоит перед 8 байтами данных.
    let size_field = bytes.len() - 8 - 4;
    bytes[size_field..size_field + 4].copy_from_slice(&0xFFFF_FFFFu32.to_le_bytes());
    let error = decode_wav(&bytes).unwrap_err();
    assert!(matches!(error, DecodeError::Corrupt(_)), "{error}");
}

#[test]
fn truncated_and_malformed_wav_files_fail_without_panicking() {
    let full = wav(1, 1, 8_000, 16, &[0u8; 16]);
    for cut in [0, 8, 20, 30, 40, full.len() - 1] {
        assert!(decode_wav(&full[..cut]).is_err(), "cut at {cut}");
    }
    assert!(matches!(
        decode_wav(&wav(1, 1, 8_000, 16, &[0u8; 3])),
        Err(DecodeError::Corrupt(_))
    ));
    assert!(matches!(
        decode_wav(b"RIFX\0\0\0\0WAVE"),
        Err(DecodeError::Corrupt(_))
    ));
    assert!(matches!(
        decode_wav(&wav(1, 3, 8_000, 16, &[0u8; 6])),
        Err(DecodeError::Unsupported(_))
    ));
    assert!(matches!(
        decode_wav(&wav(2, 1, 8_000, 16, &[0u8; 4])),
        Err(DecodeError::Unsupported(_))
    ));
}

#[test]
fn oversized_sample_count_is_rejected_by_limit() {
    let samples = MAX_CLIP_SAMPLES + 1;
    let header = wav(1, 1, 8_000, 8, &[]);
    let mut bytes = header;
    let size = samples as u32;
    let len = bytes.len();
    bytes[len - 4..].copy_from_slice(&size.to_le_bytes());
    bytes.resize(len + samples, 0);
    let error = decode_wav(&bytes).unwrap_err();
    assert!(matches!(error, DecodeError::TooLarge { .. }), "{error}");
}

#[test]
fn ogg_fixture_decodes_with_its_declared_format() {
    let clip = decode_ogg(OGG_FIXTURE, &live()).unwrap();
    assert_eq!(clip.sample_rate, 22_050);
    assert_eq!(clip.channels, 1);
    let seconds = clip.frames() as f32 / 22_050.0;
    assert!((seconds - 0.5).abs() < 0.05, "длительность {seconds} с");
    let peak = clip
        .samples
        .iter()
        .fold(0.0f32, |peak, value| peak.max(value.abs()));
    // ffmpeg `sine` по умолчанию имеет амплитуду 1/8.
    assert!((0.05..=0.2).contains(&peak), "амплитуда синуса: {peak}");
}

#[test]
fn truncated_ogg_fixture_fails_instead_of_panicking() {
    for cut in [0, 16, 100, OGG_FIXTURE.len() / 2] {
        let result = decode_ogg(&OGG_FIXTURE[..cut], &live());
        assert!(
            result.is_err() || result.unwrap().frames() < 22_050,
            "cut at {cut}"
        );
    }
}

#[test]
fn extension_selects_the_decoder_and_unknown_ones_are_unsupported() {
    assert!(decode("WAV", &wav(1, 1, 8_000, 8, &[128]), &live()).is_ok());
    assert!(decode("ogg", OGG_FIXTURE, &live()).is_ok());
    assert!(matches!(
        decode("mp3", &[], &live()),
        Err(DecodeError::Unsupported(_))
    ));
}

#[test]
fn cancelled_ogg_decode_stops_before_the_first_packet() {
    let cancel = AtomicBool::new(true);
    assert!(matches!(
        decode_ogg(OGG_FIXTURE, &cancel),
        Err(DecodeError::Cancelled)
    ));
    cancel.store(false, Ordering::Relaxed);
    assert!(
        decode_ogg(OGG_FIXTURE, &cancel).is_ok(),
        "без отмены декодирование проходит"
    );
}
