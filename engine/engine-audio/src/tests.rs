use super::*;
use engine_scene::AudioGroup;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering as AtomicOrdering};

fn voice(id: u64) -> VoiceId {
    VoiceId::from_raw(id).expect("тестовый id ненулевой")
}

fn spatial_request(looping: bool) -> PlayRequest {
    PlayRequest {
        spatial: Some(Distances {
            min: 1.0,
            max: 30.0,
        }),
        ..request(looping)
    }
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

/// Минимальный 16-битный моно WAV нужной длины в кадрах (22050 Гц).
fn evicted_voice(
    result: Result<(usize, Option<LaneSlot>), AudioError>,
) -> Result<(usize, Option<VoiceId>), AudioError> {
    result.map(|(lane, evicted)| (lane, evicted.map(|slot| slot.voice)))
}

fn wav_bytes(frames: usize) -> Vec<u8> {
    let data_len = (frames * 2) as u32;
    let mut out = Vec::new();
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&(36 + data_len).to_le_bytes());
    out.extend_from_slice(b"WAVEfmt ");
    out.extend_from_slice(&16u32.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&22_050u32.to_le_bytes());
    out.extend_from_slice(&(22_050u32 * 2).to_le_bytes());
    out.extend_from_slice(&2u16.to_le_bytes());
    out.extend_from_slice(&16u16.to_le_bytes());
    out.extend_from_slice(b"data");
    out.extend_from_slice(&data_len.to_le_bytes());
    out.resize(out.len() + data_len as usize, 0);
    out
}

#[test]
fn lanes_fill_then_evict_the_oldest_one_shot() {
    let mut table = Lanes::default();
    for id in 1..=VOICE_SLOTS as u64 {
        assert_eq!(
            table.allocate(voice(id), false),
            Ok(((id - 1) as usize, None))
        );
    }
    assert_eq!(
        evicted_voice(table.allocate(voice(100), false)),
        Ok((0, Some(voice(1))))
    );
    assert_eq!(table.lane_of(voice(100)), Some(0));
    assert_eq!(table.lane_of(voice(1)), None);
}

#[test]
fn eviction_skips_loops_and_takes_the_oldest_one_shot() {
    let mut table = Lanes::default();
    table.allocate(voice(1), true).unwrap();
    for id in 2..=VOICE_SLOTS as u64 {
        table.allocate(voice(id), false).unwrap();
    }
    assert_eq!(
        evicted_voice(table.allocate(voice(200), false)),
        Ok((1, Some(voice(2))))
    );
    assert_eq!(table.lane_of(voice(1)), Some(0));
}

#[test]
fn looping_voices_are_never_evicted() {
    let mut table = Lanes::default();
    for id in 1..=VOICE_SLOTS as u64 {
        table.allocate(voice(id), true).unwrap();
    }
    assert_eq!(
        table.allocate(voice(200), false),
        Err(AudioError::NoFreeVoice)
    );
    assert_eq!(table.lane_of(voice(1)), Some(0));
}

#[test]
fn release_frees_only_the_lane_owned_by_that_voice() {
    let mut table = Lanes::default();
    table.allocate(voice(1), false).unwrap();
    assert!(!table.release(voice(2)));
    assert_eq!(table.lane_of(voice(1)), Some(0));
    assert!(table.release(voice(1)));
    assert_eq!(table.lane_of(voice(1)), None);
}

#[test]
fn play_reserves_a_lane_and_sends_it_with_the_command() {
    let (handle, commands, _events) = AudioHandle::detached();
    handle
        .play(voice(1), "audio/hum.wav", request(false))
        .unwrap();
    let lane = handle.lane_of(voice(1)).expect("слот выделен");
    match commands.try_recv() {
        Ok(Command::Play {
            voice: sent,
            lane: sent_lane,
            path,
            ..
        }) => {
            assert_eq!(sent, voice(1));
            assert_eq!(sent_lane, lane);
            assert_eq!(path, "audio/hum.wav");
        }
        other => panic!("ожидалась команда Play: {other:?}"),
    }
}

#[test]
fn full_pool_sends_stop_for_the_evicted_voice_before_the_new_play() {
    let (handle, commands, _events) = AudioHandle::detached();
    for id in 1..=VOICE_SLOTS as u64 {
        handle
            .play(voice(id), "audio/hum.wav", request(false))
            .unwrap();
    }
    let _: Vec<Command> = commands.try_iter().collect();
    handle
        .play(voice(100), "audio/hum.wav", request(false))
        .unwrap();
    let sent: Vec<Command> = commands.try_iter().collect();
    assert!(
        matches!(
            sent.as_slice(),
            [Command::Stop(stopped), Command::Play { voice: new, .. }]
                if *stopped == voice(1) && *new == voice(100)
        ),
        "{sent:?}"
    );
}

#[test]
fn full_queue_rejects_commands_and_a_rejected_play_gives_back_its_lane() {
    let (handle, commands, _events) = AudioHandle::detached();
    for _ in 0..COMMAND_CAPACITY {
        handle.preload(Vec::new()).unwrap();
    }
    assert_eq!(
        handle.play(voice(1), "audio/hum.wav", request(false)),
        Err(AudioError::QueueFull)
    );
    assert_eq!(handle.lane_of(voice(1)), None);
    assert_eq!(handle.stop(voice(2)), Err(AudioError::QueueFull));
    assert_eq!(commands.try_iter().count(), COMMAND_CAPACITY);
    handle.stop(voice(2)).unwrap();
    assert!(matches!(commands.try_recv(), Ok(Command::Stop(id)) if id == voice(2)));
}

#[test]
fn positions_are_written_without_the_command_queue() {
    let (handle, commands, _events) = AudioHandle::detached();
    handle
        .play(voice(1), "audio/hum.wav", request(true))
        .unwrap();
    for _ in 0..COMMAND_CAPACITY - 1 {
        handle.preload(Vec::new()).unwrap();
    }
    assert_eq!(handle.preload(Vec::new()), Err(AudioError::QueueFull));
    let lane = handle.lane_of(voice(1)).unwrap();
    for step in 0..10_000 {
        handle.set_emitter(voice(1), [step as f32, 0.0, 0.0]);
    }
    assert_eq!(handle.spatial().emitter(lane), Some([9_999.0, 0.0, 0.0]));
    handle.set_emitter(voice(42), [1.0, 1.0, 1.0]);
    assert_eq!(commands.try_iter().count(), COMMAND_CAPACITY);
}

#[test]
fn play_at_writes_the_emitter_before_the_voice_can_render() {
    let (handle, _commands, _events) = AudioHandle::detached();
    handle
        .play_at(
            voice(1),
            "audio/hum.wav",
            spatial_request(true),
            [4.0, 0.0, 0.0],
        )
        .unwrap();
    let lane = handle.lane_of(voice(1)).unwrap();
    assert_eq!(handle.spatial().emitter(lane), Some([4.0, 0.0, 0.0]));
}

#[test]
fn run_returns_on_shutdown_without_an_output_device() {
    let (engine, handle) = AudioEngine::new(AudioConfig::default());
    let worker = thread::spawn(move || engine.run());
    handle.shutdown();
    worker.join().unwrap();
}

#[test]
fn run_returns_when_every_handle_is_dropped() {
    let (engine, handle) = AudioEngine::new(AudioConfig::default());
    let worker = thread::spawn(move || engine.run());
    drop(handle);
    worker.join().unwrap();
}

/// Поток `rve-audio` без запуска: команды подаются из теста, чтения и декодирование — синхронно.
struct Harness {
    handle: AudioHandle,
    commands: Receiver<Command>,
    core: Core,
    mixer: rtrb::Consumer<MixerCommand>,
    decode_jobs: Receiver<DecodeJob>,
    reads: flume::Sender<Result<Vec<u8>, String>>,
    reads_started: Arc<AtomicUsize>,
}

impl Harness {
    fn new() -> Self {
        let (engine, handle) = AudioEngine::new(AudioConfig::default());
        let AudioEngine {
            commands,
            events_tx,
            lanes,
            ..
        } = engine;
        let (reads, script) = flume::unbounded::<Result<Vec<u8>, String>>();
        let reads_started = Arc::new(AtomicUsize::new(0));
        let started = Arc::clone(&reads_started);
        let read: ReadSource = Box::new(move |_path: &str| {
            started.fetch_add(1, AtomicOrdering::Relaxed);
            let script = script.clone();
            Ok(Box::new(move || match script.try_recv() {
                Ok(result) => Some(result),
                Err(flume::TryRecvError::Empty) => None,
                Err(flume::TryRecvError::Disconnected) => Some(Err("канал закрыт".to_owned())),
            }) as ReadPoll)
        });
        let (mixer_tx, mixer) = RingBuffer::new(MIXER_CAPACITY);
        let (decode_tx, decode_jobs) = flume::unbounded();
        let core = Core {
            lanes,
            read,
            loading: HashMap::new(),
            decoding: HashSet::new(),
            cache: HashMap::new(),
            cache_bytes: 0,
            failed: HashSet::new(),
            pending: Vec::new(),
            mixer: Some(mixer_tx),
            outbox: VecDeque::new(),
            events: events_tx,
            decode_tx,
        };
        Self {
            handle,
            commands,
            core,
            mixer,
            decode_jobs,
            reads,
            reads_started,
        }
    }

    /// Выполняет накопленные команды, запускает готовые чтения и декодирует полученные байты.
    fn step(&mut self) {
        while let Ok(command) = self.commands.try_recv() {
            assert!(self.core.command(command));
        }
        self.core.poll_reads();
        while let Ok(job) = self.decode_jobs.try_recv() {
            let result = decode("wav", &job.bytes, &AtomicBool::new(false));
            self.core.decoded(Decoded {
                path: job.path,
                result,
            });
        }
    }

    fn reads(&self) -> usize {
        self.reads_started.load(AtomicOrdering::Relaxed)
    }

    /// Команды, дошедшие до кольца микшера, в порядке отправки.
    fn sent(&mut self) -> Vec<MixerCommand> {
        std::iter::from_fn(|| self.mixer.pop().ok()).collect()
    }
}

#[test]
fn stop_before_the_clip_loads_cancels_the_play() {
    let mut h = Harness::new();
    h.handle
        .play(voice(1), "audio/hum.wav", request(false))
        .unwrap();
    h.handle.stop(voice(1)).unwrap();
    h.reads.send(Ok(wav_bytes(2205))).unwrap();
    h.step();
    let sent = h.sent();
    assert!(
        sent.iter()
            .all(|command| matches!(command, MixerCommand::Stop { .. })),
        "{sent:?}"
    );
    assert_eq!(h.reads(), 1);
    assert_eq!(h.handle.lane_of(voice(1)), None);
    assert_eq!(
        h.handle.poll_events(),
        vec![AudioEvent::VoiceEnded(voice(1))]
    );
}

#[test]
fn ready_clip_is_sent_to_the_mixer_on_its_lane_and_cached_for_later_plays() {
    let mut h = Harness::new();
    h.handle
        .play(voice(1), "audio/hum.wav", request(false))
        .unwrap();
    h.reads.send(Ok(wav_bytes(2205))).unwrap();
    h.step();
    match h.sent().as_slice() {
        [
            MixerCommand::Play {
                voice: id,
                lane,
                clip,
                ..
            },
        ] => {
            assert_eq!(*id, voice(1));
            assert_eq!(*lane, 0);
            assert_eq!(clip.frames(), 2205);
        }
        other => panic!("ожидалась команда Play: {other:?}"),
    }

    h.handle
        .play(voice(2), "audio/hum.wav", request(false))
        .unwrap();
    h.step();
    assert!(matches!(
        h.sent().as_slice(),
        [MixerCommand::Play { voice: id, .. }] if *id == voice(2)
    ));
    assert_eq!(h.reads(), 1, "второй запуск берёт клип из кеша");
}

#[test]
fn a_clip_that_cannot_be_read_releases_its_lane_and_plays_nothing() {
    let mut h = Harness::new();
    h.handle
        .play(voice(1), "audio/missing.wav", request(false))
        .unwrap();
    h.reads.send(Err("не найден".to_owned())).unwrap();
    h.step();
    assert_eq!(h.handle.lane_of(voice(1)), None);
    assert!(h.sent().is_empty());
    let events = h.handle.poll_events();
    assert!(
        matches!(
            events.as_slice(),
            [AudioEvent::Failed(message), AudioEvent::VoiceEnded(ended)]
                if message.contains("audio/missing.wav") && *ended == voice(1)
        ),
        "{events:?}"
    );

    h.handle
        .play(voice(2), "audio/missing.wav", request(false))
        .unwrap();
    h.step();
    assert_eq!(h.handle.lane_of(voice(2)), None);
    assert_eq!(h.reads(), 1, "отказавший путь не читается повторно");
}

#[test]
fn corrupt_bytes_fail_the_clip_and_release_its_lane() {
    let mut h = Harness::new();
    h.handle
        .play(voice(1), "audio/hum.wav", request(false))
        .unwrap();
    h.reads.send(Ok(b"not a wav".to_vec())).unwrap();
    h.step();
    assert_eq!(h.handle.lane_of(voice(1)), None);
    assert!(h.sent().is_empty());
}

#[test]
fn clip_over_the_cache_budget_fails_without_panicking() {
    let mut h = Harness::new();
    h.core.cache_bytes = CACHE_LIMIT_BYTES;
    h.handle
        .play(voice(1), "audio/big.wav", request(false))
        .unwrap();
    h.reads.send(Ok(wav_bytes(2205))).unwrap();
    h.step();
    assert_eq!(h.handle.lane_of(voice(1)), None);
    assert!(h.sent().is_empty());
}

#[test]
fn stop_all_drops_every_waiting_play() {
    let mut h = Harness::new();
    h.handle
        .play(voice(1), "audio/hum.wav", request(false))
        .unwrap();
    h.handle
        .play(voice(2), "audio/hum.wav", request(false))
        .unwrap();
    h.handle.stop_all().unwrap();
    h.reads.send(Ok(wav_bytes(2205))).unwrap();
    h.step();
    let sent = h.sent();
    assert!(
        sent.iter()
            .all(|command| matches!(command, MixerCommand::StopAll)),
        "{sent:?}"
    );
}

#[test]
fn ended_voice_frees_its_lane_and_reports_the_event() {
    let mut h = Harness::new();
    h.handle
        .play(voice(1), "audio/hum.wav", request(false))
        .unwrap();
    h.reads.send(Ok(wav_bytes(2205))).unwrap();
    h.step();
    assert!(matches!(h.sent().as_slice(), [MixerCommand::Play { .. }]));
    h.core.voice_ended(voice(1));
    assert_eq!(h.handle.lane_of(voice(1)), None);
    assert_eq!(
        h.handle.poll_events(),
        vec![AudioEvent::VoiceEnded(voice(1))]
    );
}

#[test]
fn oversized_file_is_rejected_before_its_bytes_are_read() {
    let started = Arc::new(AtomicBool::new(false));
    let flag = Arc::clone(&started);
    let mut poll = guarded_read(
        || Some(Ok(MAX_CLIP_FILE_BYTES + 1)),
        move || {
            flag.store(true, AtomicOrdering::Relaxed);
            Ok(Box::new(|| None) as ReadPoll)
        },
    );
    assert!(matches!(poll(), Some(Err(message)) if message.contains("лимита")));
    assert!(!started.load(AtomicOrdering::Relaxed));
}

#[test]
fn file_within_the_limit_is_read_once_its_size_is_known() {
    let mut size_pending = true;
    let mut poll = guarded_read(
        move || {
            if size_pending {
                size_pending = false;
                None
            } else {
                Some(Ok(4))
            }
        },
        move || Ok(Box::new(|| Some(Ok(vec![1u8, 2, 3, 4]))) as ReadPoll),
    );
    assert!(poll().is_none(), "размер ещё не получен");
    assert!(poll().is_none(), "чтение начато, байт пока нет");
    assert_eq!(poll(), Some(Ok(vec![1u8, 2, 3, 4])));
}

#[test]
fn preload_starts_one_read_per_path() {
    let mut h = Harness::new();
    h.handle
        .preload(vec!["audio/hum.wav".to_owned(), "audio/hum.wav".to_owned()])
        .unwrap();
    h.step();
    assert_eq!(h.reads(), 1);
}

#[test]
fn commands_beyond_the_mixer_ring_wait_in_order_and_none_are_lost() {
    let mut h = Harness::new();
    let total = MIXER_CAPACITY + 16;
    for index in 0..total {
        h.core.send_mixer(MixerCommand::SetGroupVolume {
            group: AudioGroup::Music,
            volume: index as f32,
        });
    }
    assert_eq!(
        h.core.outbox.len(),
        16,
        "кольцо вмещает MIXER_CAPACITY команд"
    );
    let mut received = h.sent();
    h.core.flush_mixer();
    received.extend(h.sent());
    let volumes: Vec<f32> = received
        .iter()
        .map(|command| match command {
            MixerCommand::SetGroupVolume { volume, .. } => *volume,
            other => panic!("неожиданная команда: {other:?}"),
        })
        .collect();
    assert_eq!(volumes, (0..total).map(|i| i as f32).collect::<Vec<_>>());
}

#[test]
fn shutdown_ends_run_even_when_the_command_queue_is_full() {
    let (engine, handle) = AudioEngine::new(AudioConfig::default());
    for _ in 0..COMMAND_CAPACITY {
        handle.preload(Vec::new()).unwrap();
    }
    assert_eq!(handle.preload(Vec::new()), Err(AudioError::QueueFull));
    handle.shutdown();
    // Таймаут — только страховка от зависания: без флага run не вернётся.
    let (done_tx, done_rx) = flume::bounded(1);
    thread::spawn(move || {
        engine.run();
        let _ = done_tx.send(());
    });
    assert!(done_rx.recv_timeout(Duration::from_secs(10)).is_ok());
}

#[test]
fn cancelled_decoder_skips_the_queued_jobs() {
    let jobs = |count: usize| {
        let (tx, rx) = flume::unbounded();
        for index in 0..count {
            tx.send(DecodeJob {
                path: format!("audio/{index}.wav"),
                bytes: wav_bytes(2205),
            })
            .unwrap();
        }
        drop(tx);
        rx
    };
    let (results_tx, results_rx) = flume::unbounded();
    decode_loop(jobs(3), results_tx, &AtomicBool::new(true));
    assert!(results_rx.try_iter().next().is_none());

    let (results_tx, results_rx) = flume::unbounded();
    decode_loop(jobs(3), results_tx, &AtomicBool::new(false));
    assert_eq!(results_rx.try_iter().count(), 3);
}

#[test]
fn stopped_voice_keeps_its_lane_until_the_mixer_reports_it_ended() {
    let mut h = Harness::new();
    h.handle
        .play(voice(1), "audio/hum.wav", request(false))
        .unwrap();
    h.reads.send(Ok(wav_bytes(2205))).unwrap();
    h.step();
    let _ = h.sent();

    h.handle.stop(voice(1)).unwrap();
    h.handle
        .play(voice(2), "audio/hum.wav", request(false))
        .unwrap();
    assert_eq!(
        h.handle.lane_of(voice(1)),
        Some(0),
        "затухающий голос держит слот"
    );
    assert_eq!(
        h.handle.lane_of(voice(2)),
        Some(1),
        "новый запуск получает другой слот"
    );
    h.step();
    assert!(
        matches!(
            h.sent().as_slice(),
            [
                MixerCommand::Stop { voice: stopped },
                MixerCommand::Play { voice: started, lane: 1, .. },
            ] if *stopped == voice(1) && *started == voice(2)
        ),
        "порядок: Stop старого, затем Play нового"
    );

    h.core.voice_ended(voice(1));
    assert_eq!(h.handle.lane_of(voice(1)), None);
    assert_eq!(h.handle.lane_of(voice(2)), Some(1));
}

#[test]
fn voices_that_never_reach_the_mixer_end_at_once_so_the_source_can_play_again() {
    let mut h = Harness::new();
    h.core.mixer = None;
    h.handle
        .play(voice(1), "audio/hum.wav", request(true))
        .unwrap();
    h.reads.send(Ok(wav_bytes(2205))).unwrap();
    h.step();
    assert_eq!(h.handle.lane_of(voice(1)), None);
    assert_eq!(
        h.handle.poll_events(),
        vec![AudioEvent::VoiceEnded(voice(1))]
    );

    h.handle
        .play(voice(2), "audio/hum.wav", request(true))
        .unwrap();
    h.step();
    assert_eq!(h.handle.lane_of(voice(2)), None);
    assert_eq!(
        h.handle.poll_events(),
        vec![AudioEvent::VoiceEnded(voice(2))]
    );
}

#[test]
fn stop_on_a_full_queue_keeps_the_voice_and_its_lane() {
    let (handle, commands, _events) = AudioHandle::detached();
    handle
        .play(voice(1), "audio/hum.wav", request(true))
        .unwrap();
    for _ in 0..COMMAND_CAPACITY - 1 {
        handle.preload(Vec::new()).unwrap();
    }
    assert_eq!(handle.stop(voice(1)), Err(AudioError::QueueFull));
    assert_eq!(handle.lane_of(voice(1)), Some(0));
    let _: Vec<Command> = commands.try_iter().collect();
    handle.stop(voice(1)).unwrap();
    assert!(matches!(commands.try_recv(), Ok(Command::Stop(id)) if id == voice(1)));
    assert_eq!(
        handle.lane_of(voice(1)),
        Some(0),
        "слот освобождает только VoiceEnded"
    );
}

#[test]
fn barrier_returns_after_earlier_commands_have_reported_their_failures() {
    let (engine, handle) = AudioEngine::new(AudioConfig::default());
    let worker = thread::spawn(move || engine.run());
    handle
        .preload(vec!["audio/missing.wav".to_owned()])
        .unwrap();
    handle.barrier().unwrap();
    let events = handle.poll_events();
    assert!(
        events.iter().any(|event| matches!(
            event,
            AudioEvent::Failed(message) if message.contains("audio/missing.wav")
        )),
        "{events:?}"
    );
    handle.shutdown();
    worker.join().unwrap();
}
