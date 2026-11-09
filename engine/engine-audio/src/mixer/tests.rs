use super::*;

const RATE: u32 = 48_000;

fn clip(rate: u32, channels: u16, samples: Vec<f32>) -> Arc<Clip> {
    Arc::new(Clip {
        sample_rate: rate,
        channels,
        samples,
    })
}

fn request(looping: bool) -> PlayRequest {
    PlayRequest {
        group: AudioGroup::Effects,
        volume: 1.0,
        looping,
        spatial: None,
        spatial_blend: 0.0,
        rolloff: Rolloff::Inverse,
        pitch: 1.0,
        force_mono: false,
    }
}

fn spatial_request(looping: bool) -> PlayRequest {
    PlayRequest {
        spatial: Some(Distances {
            min: 1.0,
            max: 30.0,
        }),
        spatial_blend: 1.0,
        ..request(looping)
    }
}

fn voice(id: u64) -> VoiceId {
    VoiceId::from_raw(id).expect("тестовый id ненулевой")
}

fn mixer() -> Mixer {
    Mixer::new(RATE, Arc::new(Spatial::new()))
}

fn play(mixer: &mut Mixer, id: u64, lane: usize, clip: Arc<Clip>, request: PlayRequest) {
    mixer.apply(MixerCommand::Play {
        voice: voice(id),
        lane,
        clip,
        request,
    });
}

fn drain(mixer: &mut Mixer) -> Vec<VoiceId> {
    let mut ids = Vec::new();
    mixer.drain_ended(|id| {
        ids.push(id);
        true
    });
    ids
}

fn render_mono(mixer: &mut Mixer, frames: usize) -> Vec<f32> {
    let mut out = vec![0.0; frames];
    mixer.render(&mut out, 1);
    out
}

fn render_stereo(mixer: &mut Mixer, frames: usize) -> Vec<f32> {
    let mut out = vec![0.0; frames * 2];
    mixer.render(&mut out, 2);
    out
}

#[test]
fn one_shot_finishes_and_reports_its_voice() {
    let mut mixer = mixer();
    play(
        &mut mixer,
        1,
        0,
        clip(RATE, 1, vec![0.5; 300]),
        request(false),
    );
    let out = render_mono(&mut mixer, 320);
    assert!(
        out[256..300]
            .iter()
            .all(|value| (*value - 0.5).abs() < 1e-6)
    );
    assert!(out[300..].iter().all(|value| *value == 0.0));
    assert_eq!(mixer.active_voices(), 0);
    assert_eq!(drain(&mut mixer), vec![voice(1)]);
    assert!(drain(&mut mixer).is_empty());
}

#[test]
fn looping_voice_repeats_and_keeps_playing() {
    let mut mixer = mixer();
    play(
        &mut mixer,
        2,
        3,
        clip(RATE, 1, vec![0.25, -0.25]),
        request(true),
    );
    let out = render_mono(&mut mixer, 9);
    assert!(out[0] > 0.0 && out[1] < 0.0 && out[2] > 0.0, "{out:?}");
    assert_eq!(mixer.active_voices(), 1);
    assert!(drain(&mut mixer).is_empty());
}

#[test]
fn group_and_master_volumes_multiply_voice_volume() {
    let mut mixer = mixer();
    mixer.apply(MixerCommand::SetGroupVolume {
        group: AudioGroup::Effects,
        volume: 0.5,
    });
    mixer.apply(MixerCommand::SetGroupVolume {
        group: AudioGroup::Master,
        volume: 0.5,
    });
    assert_eq!(mixer.group_gain(AudioGroup::Effects), 0.5);
    play(
        &mut mixer,
        1,
        0,
        clip(RATE, 1, vec![1.0; 2048]),
        request(false),
    );
    let out = render_mono(&mut mixer, 1024);
    assert!((out[1000] - 0.25).abs() < 1e-5, "{}", out[1000]);

    mixer.apply(MixerCommand::SetGroupVolume {
        group: AudioGroup::Master,
        volume: 0.0,
    });
    render_mono(&mut mixer, 256);
    let silent = render_mono(&mut mixer, 256);
    assert!(silent.iter().all(|value| value.abs() < 1e-6));
}

#[test]
fn attenuation_is_full_inside_min_zero_at_max_and_beyond() {
    let distances = Distances {
        min: 2.0,
        max: 10.0,
    };
    assert_eq!(attenuation(0.0, distances), 1.0);
    assert_eq!(attenuation(2.0, distances), 1.0);
    assert!((attenuation(4.0, distances) - 0.375).abs() < 1e-6);
    assert_eq!(attenuation(10.0, distances), 0.0);
    assert_eq!(attenuation(50.0, distances), 0.0);
}

#[test]
fn spatial_voice_pans_toward_its_side_and_is_silent_past_max() {
    let spatial = Arc::new(Spatial::new());
    spatial.set_listener([0.0; 3], [1.0, 0.0, 0.0], [0.0, 0.0, -1.0]);
    let mut mixer = Mixer::new(RATE, Arc::clone(&spatial));

    spatial.set_emitter(0, [-5.0, 0.0, 0.0]);
    play(
        &mut mixer,
        1,
        0,
        clip(RATE, 1, vec![1.0; 4]),
        spatial_request(true),
    );
    let left = render_stereo(&mut mixer, 64);
    let (l, r) = (left[120], left[121]);
    assert!(l > r * 2.0, "слева: l={l} r={r}");

    spatial.set_emitter(0, [5.0, 0.0, 0.0]);
    let right = render_stereo(&mut mixer, 64);
    let (l, r) = (right[120], right[121]);
    assert!(r > l * 2.0, "справа: l={l} r={r}");

    spatial.set_emitter(0, [100.0, 0.0, 0.0]);
    let far = render_stereo(&mut mixer, 512);
    assert!(far[1000..].iter().all(|value| value.abs() < 1e-5));
    assert_eq!(
        mixer.active_voices(),
        1,
        "голос дальше max остаётся активным"
    );
}

#[test]
fn spatial_emitter_at_listener_is_centred() {
    let spatial = Arc::new(Spatial::new());
    spatial.set_listener([1.0, 2.0, 3.0], [1.0, 0.0, 0.0], [0.0, 0.0, -1.0]);
    spatial.set_emitter(0, [1.0, 2.0, 3.0]);
    let mut mixer = Mixer::new(RATE, spatial);
    play(
        &mut mixer,
        1,
        0,
        clip(RATE, 1, vec![1.0; 4]),
        spatial_request(true),
    );
    let out = render_stereo(&mut mixer, 64);
    assert!((out[100] - out[101]).abs() < 1e-6);
}

#[test]
fn resampling_22050_to_48000_produces_expected_frame_count() {
    let mut mixer = mixer();
    play(
        &mut mixer,
        1,
        0,
        clip(22_050, 1, vec![0.5; 22_050]),
        request(false),
    );
    let mut frames = 0;
    let mut block = vec![0.0; 256];
    while mixer.active_voices() > 0 && frames < 100_000 {
        mixer.render(&mut block, 1);
        frames += 256;
    }
    let expected: i64 = 48_000;
    assert!(
        (frames as i64 - expected).abs() < 256,
        "{frames} кадров, ожидалось около {expected}"
    );
}

#[test]
fn playing_into_an_occupied_lane_replaces_and_reports_the_old_voice() {
    let mut mixer = mixer();
    play(
        &mut mixer,
        1,
        5,
        clip(RATE, 1, vec![0.5; 100_000]),
        request(true),
    );
    play(
        &mut mixer,
        2,
        5,
        clip(RATE, 1, vec![0.5; 100_000]),
        request(false),
    );
    assert_eq!(drain(&mut mixer), vec![voice(1)]);
    assert_eq!(mixer.active_voices(), 1);
}

#[test]
fn stop_fades_out_then_removes_the_voice() {
    let mut mixer = mixer();
    play(
        &mut mixer,
        1,
        0,
        clip(RATE, 1, vec![1.0; 100_000]),
        request(true),
    );
    render_mono(&mut mixer, 512);
    mixer.apply(MixerCommand::Stop { voice: voice(1) });
    let fade = render_mono(&mut mixer, 512);
    assert!(
        fade[0] > 0.9,
        "fade начинается с текущей громкости: {}",
        fade[0]
    );
    assert!(fade[fade.len() - 1] < fade[0]);
    assert_eq!(mixer.active_voices(), 0);
    assert_eq!(drain(&mut mixer), vec![voice(1)]);
}

#[test]
fn volume_change_is_ramped_without_jumps() {
    let mut mixer = mixer();
    play(
        &mut mixer,
        1,
        0,
        clip(RATE, 1, vec![1.0; 100_000]),
        request(true),
    );
    let before = render_mono(&mut mixer, 512);
    mixer.apply(MixerCommand::SetVoiceVolume {
        voice: voice(1),
        volume: 0.0,
    });
    let after = render_mono(&mut mixer, 512);
    let jump = (after[0] - before[511]).abs();
    assert!(jump < 0.05, "скачок на границе блоков: {jump}");
    let settled = render_mono(&mut mixer, 512);
    assert!(settled.iter().all(|value| value.abs() < 1e-6));
}

#[test]
fn stereo_clip_downmixes_to_mono_and_extra_channels_stay_silent() {
    let mut mixer = mixer();
    play(
        &mut mixer,
        1,
        0,
        clip(RATE, 2, vec![1.0, 0.0, 1.0, 0.0]),
        request(false),
    );
    let mono = render_mono(&mut mixer, 2);
    assert!((mono[1] - 0.5).abs() < 1e-6, "{mono:?}");

    let mut quad_mixer = Mixer::new(RATE, Arc::new(Spatial::new()));
    play(
        &mut quad_mixer,
        1,
        0,
        clip(RATE, 2, [1.0, 0.0].repeat(8)),
        request(true),
    );
    let mut quad = vec![9.0_f32; 4 * 512];
    quad_mixer.render(&mut quad, 4);
    let frame = &quad[400 * 4..401 * 4];
    assert!((frame[0] - 1.0).abs() < 1e-6, "{frame:?}");
    assert!(frame[1].abs() < 1e-6, "{frame:?}");
    assert_eq!(frame[2], 0.0);
    assert_eq!(frame[3], 0.0);
}

#[test]
fn paused_mixer_is_silent_and_does_not_advance() {
    let ramp: Vec<f32> = (0..100_000).map(|index| index as f32 / 1000.0).collect();
    let mut mixer = mixer();
    play(&mut mixer, 1, 0, clip(RATE, 1, ramp), request(false));
    render_mono(&mut mixer, 256);
    mixer.apply(MixerCommand::SetPaused(true));
    assert!(
        render_mono(&mut mixer, 16)
            .iter()
            .all(|value| *value == 0.0)
    );
    mixer.apply(MixerCommand::SetPaused(false));
    let out = render_mono(&mut mixer, 4);
    assert!(
        (out[0] - 0.256).abs() < 1e-4,
        "пауза не должна двигать позицию: {out:?}"
    );
}

#[test]
fn ended_queue_keeps_events_until_the_sink_accepts_them() {
    let mut mixer = mixer();
    for id in 1..=3 {
        play(
            &mut mixer,
            id,
            id as usize,
            clip(RATE, 1, vec![0.1; 1]),
            request(false),
        );
    }
    render_mono(&mut mixer, 8);
    let mut attempts = Vec::new();
    mixer.drain_ended(|id| {
        attempts.push(id);
        attempts.len() < 2
    });
    assert_eq!(attempts, vec![voice(1), voice(2)]);
    assert_eq!(drain(&mut mixer), vec![voice(2), voice(3)]);
}

#[test]
fn seqlock_read_gives_up_after_bounded_attempts_while_a_write_is_open() {
    let slot = SeqSlot::<3>::new();
    slot.store([1.0, 2.0, 3.0]);
    assert_eq!(slot.try_load(), Some([1.0, 2.0, 3.0]));
    slot.seq
        .store(slot.seq.load(Ordering::Relaxed) + 1, Ordering::Relaxed);
    assert_eq!(slot.try_load(), None);
}

#[test]
fn an_open_emitter_write_keeps_the_voice_at_its_previous_position() {
    let spatial = Arc::new(Spatial::new());
    spatial.set_emitter(0, [0.0, 0.0, 5.0]);
    let mut mixer = Mixer::new(RATE, Arc::clone(&spatial));
    play(
        &mut mixer,
        1,
        0,
        clip(RATE, 1, vec![1.0; 48_000]),
        spatial_request(true),
    );
    // Первый блок ещё нарастает от нуля; установившийся уровень — последний кадр.
    let before = render_mono(&mut mixer, 256);

    let seq = spatial.emitters[0].seq.load(Ordering::Relaxed);
    spatial.emitters[0].seq.store(seq + 1, Ordering::Relaxed);
    assert_eq!(spatial.emitter(0), None);
    let during = render_mono(&mut mixer, 256);

    assert!(
        during
            .iter()
            .all(|value| (value - before[255]).abs() < 1e-6),
        "громкость и панорама прежние: {during:?}"
    );
}

#[test]
fn stopped_voice_fades_out_without_a_jump_while_a_new_voice_starts_on_another_lane() {
    let mut mixer = mixer();
    play(
        &mut mixer,
        1,
        0,
        clip(RATE, 1, vec![0.5; 4800]),
        request(false),
    );
    let _ = render_mono(&mut mixer, 256);
    mixer.apply(MixerCommand::Stop { voice: voice(1) });
    play(
        &mut mixer,
        2,
        1,
        clip(RATE, 1, vec![0.0; 4800]),
        request(false),
    );

    let out = render_mono(&mut mixer, 512);
    let fade = (FADE_SECONDS * RATE as f32).ceil() as usize;
    assert!(
        (out[0] - 0.5).abs() < 1e-6,
        "первый кадр без скачка: {}",
        out[0]
    );
    let largest_step = out
        .windows(2)
        .map(|pair| (pair[0] - pair[1]).abs())
        .fold(0.0f32, f32::max);
    assert!(
        largest_step <= 0.5 / fade as f32 + 1e-6,
        "шаг {largest_step}"
    );
    assert!(out[fade..].iter().all(|value| *value == 0.0));
    assert_eq!(drain(&mut mixer), vec![voice(1)]);
}

#[test]
fn device_output_stays_within_unit_range_and_non_finite_samples_become_silence() {
    let mut mixer = mixer();
    play(
        &mut mixer,
        1,
        0,
        clip(RATE, 1, vec![1.0; 48_000]),
        request(true),
    );
    mixer.apply(MixerCommand::SetVoiceVolume {
        voice: voice(1),
        volume: 4.0,
    });
    let loud = render_mono(&mut mixer, 512);
    assert!(
        loud.iter().all(|value| (-1.0..=1.0).contains(value)),
        "{loud:?}"
    );
    assert_eq!(loud.last().copied(), Some(1.0));

    mixer.apply(MixerCommand::SetVoiceVolume {
        voice: voice(1),
        volume: f32::NAN,
    });
    let broken = render_mono(&mut mixer, 256);
    assert!(broken.iter().all(|value| *value == 0.0), "{broken:?}");
}

/// Пространственный голос с постоянным сигналом (или заданным клипом) и передним слушателем.
fn spatial_mixer(emitter: [f32; 3], blend: f32, rolloff: Rolloff, samples: Vec<f32>) -> Mixer {
    let spatial = Arc::new(Spatial::new());
    spatial.set_listener([0.0; 3], [1.0, 0.0, 0.0], [0.0, 0.0, -1.0]);
    spatial.set_emitter(0, emitter);
    let mut mixer = Mixer::new(RATE, spatial);
    let request = PlayRequest {
        spatial: Some(Distances {
            min: 1.0,
            max: 30.0,
        }),
        spatial_blend: blend,
        rolloff,
        ..request(true)
    };
    play(&mut mixer, 1, 0, clip(RATE, 1, samples), request);
    mixer
}

#[test]
fn zero_blend_matches_a_plain_2d_voice_wherever_the_emitter_is() {
    let mut flat = mixer();
    play(&mut flat, 1, 0, clip(RATE, 1, vec![1.0; 4]), request(true));
    let mut blended = spatial_mixer([-5.0, 0.0, 0.0], 0.0, Rolloff::Inverse, vec![1.0; 4]);
    let a = render_stereo(&mut flat, 256);
    let b = render_stereo(&mut blended, 256);
    assert!(a.iter().zip(&b).all(|(x, y)| (x - y).abs() < 1e-6));
}

#[test]
fn half_blend_pans_less_than_full_blend() {
    let mut full = spatial_mixer([-5.0, 0.0, 0.0], 1.0, Rolloff::Inverse, vec![1.0; 4]);
    let mut half = spatial_mixer([-5.0, 0.0, 0.0], 0.5, Rolloff::Inverse, vec![1.0; 4]);
    let f = render_stereo(&mut full, 128);
    let h = render_stereo(&mut half, 128);
    // Полная панорама: правый канал молчит. Смесь 0.5: правый канал звучит, но тише левого.
    assert!(f[241].abs() < 1e-6, "полная панорама: правый {}", f[241]);
    let ratio = h[241] / h[240];
    assert!(
        ratio > 0.2 && ratio < 0.8,
        "смесь 0.5 даёт соотношение {ratio}"
    );
}

#[test]
fn sound_behind_the_listener_is_quieter_and_darker() {
    // Слушатель смотрит в -Z: излучатель на +Z находится за спиной на том же расстоянии.
    let rms = |emitter: [f32; 3], samples: Vec<f32>| {
        let mut mixer = spatial_mixer(emitter, 1.0, Rolloff::Inverse, samples);
        let out = render_stereo(&mut mixer, 2048);
        let tail = &out[1024..];
        (tail.iter().map(|value| value * value).sum::<f32>() / tail.len() as f32).sqrt()
    };
    let front = [0.0, 0.0, -5.0];
    let behind = [0.0, 0.0, 5.0];
    let steady_front = rms(front, vec![1.0; 4]);
    let steady_behind = rms(behind, vec![1.0; 4]);
    assert!(
        steady_behind < steady_front * 0.8,
        "за спиной тише: {steady_behind} >= {steady_front}"
    );

    // Высокая частота (Найквист) гасится фильтром за спиной сильнее, чем постоянный сигнал.
    let high = vec![1.0, -1.0];
    let front_high = rms(front, high.clone());
    let behind_high = rms(behind, high);
    assert!(
        behind_high < front_high * 0.5,
        "за спиной не приглушено: {behind_high} >= {front_high}"
    );
}

#[test]
fn mono_output_mixes_both_channels_to_the_same_value() {
    let stereo = || clip(RATE, 2, [1.0f32, 0.0].repeat(8));
    let mut normal = mixer();
    play(&mut normal, 1, 0, stereo(), request(true));
    let apart = render_stereo(&mut normal, 64);
    assert!(
        apart
            .chunks(2)
            .any(|frame| (frame[0] - frame[1]).abs() > 1e-3)
    );

    let mut mono = mixer();
    mono.apply(MixerCommand::SetMonoOutput(true));
    play(&mut mono, 1, 0, stereo(), request(true));
    let together = render_stereo(&mut mono, 64);
    assert!(
        together
            .chunks(2)
            .all(|frame| (frame[0] - frame[1]).abs() < 1e-6)
    );
    assert!(together[100] > 0.0, "звук не должен пропасть при сведении");
}

#[test]
fn force_mono_collapses_one_stereo_voice_only() {
    let stereo = clip(RATE, 2, [1.0f32, 0.0].repeat(8));
    let mut mixer = mixer();
    play(
        &mut mixer,
        1,
        0,
        stereo,
        PlayRequest {
            force_mono: true,
            ..request(true)
        },
    );
    let out = render_stereo(&mut mixer, 64);
    assert!(
        out.chunks(2)
            .all(|frame| (frame[0] - frame[1]).abs() < 1e-6)
    );
}

#[test]
fn pitch_scales_playback_speed_and_invalid_pitch_plays_at_normal_speed() {
    let ten = || clip(RATE, 1, vec![0.5; 10]);
    let pitched = |pitch: f32| PlayRequest {
        pitch,
        ..request(false)
    };

    let mut fast = mixer();
    play(&mut fast, 1, 0, ten(), pitched(2.0));
    render_mono(&mut fast, 6);
    assert_eq!(
        drain(&mut fast),
        vec![voice(1)],
        "вдвое быстрее: 10 кадров за 5"
    );

    let mut normal = mixer();
    play(&mut normal, 1, 0, ten(), pitched(1.0));
    render_mono(&mut normal, 6);
    assert!(drain(&mut normal).is_empty());

    // NaN не должен сделать шаг бесконечным: голос завершается за длину клипа.
    let mut broken = mixer();
    play(&mut broken, 1, 0, ten(), pitched(f32::NAN));
    render_mono(&mut broken, 12);
    assert_eq!(drain(&mut broken), vec![voice(1)]);
}

#[test]
fn linear_rolloff_reaches_silence_at_max_and_differs_from_inverse() {
    let distances = Distances {
        min: 1.0,
        max: 30.0,
    };
    assert_eq!(linear_attenuation(1.0, distances), 1.0);
    assert!((linear_attenuation(15.5, distances) - 0.5).abs() < 1e-6);
    assert_eq!(linear_attenuation(30.0, distances), 0.0);
    assert!((attenuation(15.5, distances) - linear_attenuation(15.5, distances)).abs() > 0.1);
}
