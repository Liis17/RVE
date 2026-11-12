use super::*;

const RATE: u32 = 48_000;

fn sine(frequency: f32, frames: usize) -> Vec<f32> {
    (0..frames)
        .flat_map(|index| {
            let value = (2.0 * PI * frequency * index as f32 / RATE as f32).sin();
            [value, value]
        })
        .collect()
}

/// Амплитуда второй половины буфера, когда фильтр уже вышел на установившийся режим.
fn steady_peak(stereo: &[f32]) -> f32 {
    stereo[stereo.len() / 2..]
        .iter()
        .fold(0.0f32, |peak, value| peak.max(value.abs()))
}

fn processed(effects: AudioGroupEffects, mut stereo: Vec<f32>) -> Vec<f32> {
    let mut bus = Bus::new(RATE);
    bus.set(effects);
    bus.process(&mut stereo);
    stereo
}

#[test]
fn bus_without_effects_passes_the_signal_unchanged() {
    let input = sine(440.0, 1024);
    let mut bus = Bus::new(RATE);
    assert!(bus.is_bypassed());
    let mut output = input.clone();
    bus.process(&mut output);
    assert_eq!(output, input);
}

#[test]
fn lowpass_keeps_low_frequencies_and_cuts_high_ones() {
    let effects = AudioGroupEffects {
        lowpass_hz: 500.0,
        ..AudioGroupEffects::default()
    };
    let low = steady_peak(&processed(effects, sine(100.0, 9600)));
    let high = steady_peak(&processed(effects, sine(8000.0, 9600)));
    assert!(low > 0.9, "низкая частота проходит: {low}");
    assert!(high < 0.05, "высокая частота подавлена: {high}");
}

#[test]
fn highpass_keeps_high_frequencies_and_cuts_low_ones() {
    let effects = AudioGroupEffects {
        highpass_hz: 2000.0,
        ..AudioGroupEffects::default()
    };
    let low = steady_peak(&processed(effects, sine(60.0, 9600)));
    let high = steady_peak(&processed(effects, sine(10_000.0, 9600)));
    assert!(low < 0.05, "низкая частота подавлена: {low}");
    assert!(high > 0.9, "высокая частота проходит: {high}");
}

#[test]
fn reverb_leaves_a_tail_after_an_impulse_and_turning_it_off_removes_it() {
    let effects = AudioGroupEffects {
        reverb: Some(AudioReverb {
            room_size: 0.8,
            damping: 0.3,
            wet: 1.0,
        }),
        ..AudioGroupEffects::default()
    };
    let mut bus = Bus::new(RATE);
    bus.set(effects);
    let mut impulse = vec![0.0; RATE as usize];
    impulse[0] = 1.0;
    impulse[1] = 1.0;
    bus.process(&mut impulse);
    let tail = steady_peak(&impulse[..RATE as usize / 2]);
    assert!(tail > 0.0, "реверберация звучит после импульса");
    assert!(impulse.iter().all(|value| value.is_finite()));

    bus.set(AudioGroupEffects::default());
    let mut silence = vec![0.0; 512];
    bus.process(&mut silence);
    assert!(silence.iter().all(|&value| value == 0.0));
}

#[test]
fn filter_recovers_from_non_finite_input() {
    let effects = AudioGroupEffects {
        lowpass_hz: 1000.0,
        ..AudioGroupEffects::default()
    };
    let mut bus = Bus::new(RATE);
    bus.set(effects);
    let mut poisoned = vec![f32::NAN, f32::NAN];
    bus.process(&mut poisoned);
    let mut after = sine(100.0, 4800);
    bus.process(&mut after);
    assert!(after.iter().all(|value| value.is_finite()));
}
