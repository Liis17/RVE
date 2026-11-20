use super::*;
use engine_scene::AudioGroup;
use std::cell::RefCell;
use std::rc::Rc;
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
        fade_in: 0.0,
        stream: false,
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
    stream_jobs: Receiver<StreamJob>,
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
        let (stream_tx, stream_jobs) = flume::unbounded();
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
            in_mixer: HashSet::new(),
            settings: MixerSettings::default(),
            events: events_tx,
            decode_tx,
            stream_tx: Some(stream_tx),
            streams: Vec::new(),
        };
        Self {
            handle,
            commands,
            core,
            mixer,
            decode_jobs,
            stream_jobs,
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

fn stream_request(looping: bool) -> PlayRequest {
    PlayRequest {
        stream: true,
        ..request(looping)
    }
}

fn ready_reader() -> StreamReader {
    crate::stream::test_reader(&[[0.5; 2]; 16], 22_050, true).1
}

#[test]
fn stream_play_opens_a_stream_instead_of_reading_the_whole_clip() {
    let mut h = Harness::new();
    h.handle
        .play(voice(1), "music/theme.ogg", stream_request(true))
        .unwrap();
    h.step();
    assert_eq!(h.reads(), 0, "файл не читается целиком");
    let job = h.stream_jobs.try_recv().expect("задание потоку стриминга");
    assert_eq!(
        (job.voice, job.path.as_str(), job.looping),
        (voice(1), "music/theme.ogg", true)
    );
    assert!(
        h.sent().is_empty(),
        "до заполнения кольца микшер голос не видит"
    );

    h.core.stream_event(StreamEvent::Ready {
        voice: voice(1),
        reader: ready_reader(),
    });
    assert!(matches!(
        h.sent().as_slice(),
        [MixerCommand::PlayStream { voice: started, lane: 0, request, .. }]
            if *started == voice(1) && request.stream
    ));
    assert!(h.core.cache.is_empty(), "стрим не попадает в кеш клипов");
}

#[test]
fn stop_before_the_stream_is_ready_ends_the_voice_and_drops_the_late_ring() {
    let mut h = Harness::new();
    h.handle
        .play(voice(1), "music/theme.ogg", stream_request(false))
        .unwrap();
    h.step();
    h.handle.stop_fade(voice(1), 2.0).unwrap();
    h.step();
    assert_eq!(h.handle.lane_of(voice(1)), None);
    assert_eq!(
        h.handle.poll_events(),
        vec![AudioEvent::VoiceEnded(voice(1))]
    );
    let _ = h.sent();

    h.core.stream_event(StreamEvent::Ready {
        voice: voice(1),
        reader: ready_reader(),
    });
    assert!(h.sent().is_empty(), "поздний стрим не запускается");
}

#[test]
fn stream_that_fails_to_open_reports_once_and_later_plays_end_at_once() {
    let mut h = Harness::new();
    h.handle
        .play(voice(1), "music/missing.ogg", stream_request(false))
        .unwrap();
    h.step();
    let _ = h.stream_jobs.try_recv().unwrap();
    h.core.stream_event(StreamEvent::OpenFailed {
        voice: voice(1),
        path: "music/missing.ogg".into(),
        message: "нет файла".into(),
    });
    let events = h.handle.poll_events();
    assert!(matches!(&events[0], AudioEvent::Failed(message) if message.contains("нет файла")));
    assert_eq!(events[1], AudioEvent::VoiceEnded(voice(1)));
    assert_eq!(h.handle.lane_of(voice(1)), None);

    h.handle
        .play(voice(2), "music/missing.ogg", stream_request(false))
        .unwrap();
    h.step();
    assert!(h.stream_jobs.try_recv().is_err(), "повторного открытия нет");
    assert_eq!(
        h.handle.poll_events(),
        vec![AudioEvent::VoiceEnded(voice(2))]
    );
}

#[test]
fn stream_broken_mid_play_reports_but_does_not_poison_the_clip() {
    let mut h = Harness::new();
    h.handle
        .play(voice(1), "music/theme.ogg", stream_request(false))
        .unwrap();
    h.step();
    let _ = h.stream_jobs.try_recv().unwrap();
    h.core.stream_event(StreamEvent::Broken {
        path: "music/theme.ogg".into(),
        message: "чтение: диск недоступен".into(),
    });
    let events = h.handle.poll_events();
    assert!(
        matches!(events.as_slice(), [AudioEvent::Failed(message)] if message.contains("диск недоступен")),
        "{events:?}"
    );
    assert_eq!(
        h.handle.lane_of(voice(1)),
        Some(0),
        "ожидающий голос того же клипа жив"
    );

    h.handle
        .play(voice(2), "music/theme.ogg", stream_request(false))
        .unwrap();
    h.step();
    assert!(
        h.stream_jobs.try_recv().is_ok(),
        "клип можно запустить снова"
    );
}

#[test]
fn stream_without_an_output_device_ends_at_once_without_opening() {
    let mut h = Harness::new();
    h.core.mixer = None;
    h.handle
        .play(voice(1), "music/theme.ogg", stream_request(true))
        .unwrap();
    h.step();
    assert!(h.stream_jobs.try_recv().is_err());
    assert_eq!(
        h.handle.poll_events(),
        vec![AudioEvent::VoiceEnded(voice(1))]
    );
}

#[test]
fn fade_out_and_group_effects_reach_the_mixer() {
    let mut h = Harness::new();
    let effects = AudioGroupEffects {
        lowpass_hz: 800.0,
        ..AudioGroupEffects::default()
    };
    h.handle.stop_fade(voice(7), 1.5).unwrap();
    h.handle
        .set_group_effects(AudioGroup::Music, effects)
        .unwrap();
    h.step();
    assert!(matches!(
        h.sent().as_slice(),
        [
            MixerCommand::FadeOut { voice: stopped, seconds },
            MixerCommand::SetGroupEffects { group: AudioGroup::Music, effects: sent },
        ] if *stopped == voice(7) && *seconds == 1.5 && *sent == effects
    ));
}

/// Сколько `VoiceEnded` пришло на голос.
fn voice_ended_count(events: &[AudioEvent], voice_id: VoiceId) -> usize {
    events
        .iter()
        .filter(|event| **event == AudioEvent::VoiceEnded(voice_id))
        .count()
}

fn failed_events(events: &[AudioEvent]) -> Vec<String> {
    events
        .iter()
        .filter_map(|event| match event {
            AudioEvent::Failed(message) => Some(message.clone()),
            AudioEvent::VoiceEnded(_) => None,
        })
        .collect()
}

/// Открытое подставным `open` устройство: концы колец, которыми владеет тест, и сигналы callback-а.
struct FakeDevice {
    commands: Consumer<MixerCommand>,
    ended: Producer<VoiceId>,
    signals: Signals,
}

/// Поток `rve-audio` с подставным устройством. `script` задаёт исходы открытий по порядку;
/// без записи в нём открытие удаётся. Время — смещения от `base`, сна нет.
struct Rig {
    h: Harness,
    device: Device<()>,
    devices: Rc<RefCell<Vec<FakeDevice>>>,
    script: Rc<RefCell<VecDeque<bool>>>,
    base: Instant,
}

impl Rig {
    fn new(output: bool) -> Self {
        let mut h = Harness::new();
        h.core.mixer = None;
        let devices = Rc::new(RefCell::new(Vec::new()));
        let script = Rc::new(RefCell::new(VecDeque::new()));
        let (opened, outcomes) = (Rc::clone(&devices), Rc::clone(&script));
        let open: OpenOutput<()> = Box::new(move |channels: Channels, signals: Signals| {
            if outcomes.borrow_mut().pop_front().unwrap_or(true) {
                opened.borrow_mut().push(FakeDevice {
                    commands: channels.commands,
                    ended: channels.ended,
                    signals,
                });
                Ok(())
            } else {
                Err("устройство занято".to_owned())
            }
        });
        let base = Instant::now();
        let device = Device::new(output.then_some(open), h.core.events.clone(), base);
        Self {
            h,
            device,
            devices,
            script,
            base,
        }
    }

    /// Выполняет очередь потери и открытия в момент `ms` от старта.
    fn poll_at(&mut self, ms: u64) {
        let now = self.base + Duration::from_millis(ms);
        self.device.poll(now, &mut self.h.core);
    }

    fn fail_next(&self, times: usize) {
        self.script
            .borrow_mut()
            .extend(std::iter::repeat_n(false, times));
    }

    fn opened(&self) -> usize {
        self.devices.borrow().len()
    }

    fn set_alive(&self, index: usize) {
        self.devices.borrow()[index]
            .signals
            .alive
            .store(true, AtomicOrdering::Release);
    }

    fn set_lost(&self, index: usize) {
        self.devices.borrow()[index]
            .signals
            .lost
            .store(true, AtomicOrdering::Release);
    }

    /// Команды, которые устройство `index` получило из кольца, в порядке поступления.
    fn received(&mut self, index: usize) -> Vec<MixerCommand> {
        let mut devices = self.devices.borrow_mut();
        let commands = &mut devices[index].commands;
        std::iter::from_fn(|| commands.pop().ok()).collect()
    }

    /// Голоса, которые устройство `index` получило командой `Play` или `PlayStream`.
    fn played(&mut self, index: usize) -> Vec<VoiceId> {
        self.received(index)
            .iter()
            .filter_map(|command| match command {
                MixerCommand::Play { voice, .. } | MixerCommand::PlayStream { voice, .. } => {
                    Some(*voice)
                }
                _ => None,
            })
            .collect()
    }

    /// Callback устройства `index` сообщает, что голос закончился.
    fn callback_ended(&self, index: usize, voice_id: VoiceId) {
        self.devices.borrow_mut()[index]
            .ended
            .push(voice_id)
            .expect("кольцо завершённых голосов свободно");
    }
}

#[test]
fn a_failed_start_reports_once_and_retries_a_quarter_second_later() {
    let mut r = Rig::new(true);
    r.fail_next(1);
    r.poll_at(0);
    let failed = failed_events(&r.h.handle.poll_events());
    assert_eq!(failed.len(), 1, "{failed:?}");
    assert!(failed[0].starts_with("звук отключён"), "{failed:?}");

    r.poll_at(249);
    assert_eq!(r.opened(), 0, "вторая попытка не раньше 0.25 с");
    r.poll_at(250);
    assert_eq!(r.opened(), 1);
    assert!(failed_events(&r.h.handle.poll_events()).is_empty());

    r.h.handle
        .play(voice(1), "audio/hum.wav", request(false))
        .unwrap();
    r.h.reads.send(Ok(wav_bytes(2205))).unwrap();
    r.h.step();
    assert_eq!(
        r.played(0),
        vec![voice(1)],
        "после второй попытки звук играет"
    );
}

#[test]
fn a_loss_reports_once_and_reopens_at_once_when_the_stream_had_run() {
    let mut r = Rig::new(true);
    r.poll_at(0);
    r.set_alive(0);
    r.poll_at(10);
    r.set_lost(0);
    r.fail_next(1);
    r.poll_at(20);
    let failed = failed_events(&r.h.handle.poll_events());
    assert_eq!(failed.len(), 1, "одна потеря — одно Failed: {failed:?}");
    assert!(failed[0].contains("потеряно"), "{failed:?}");
    assert_eq!(r.opened(), 1, "первая попытка сразу, но она не удалась");

    r.poll_at(269);
    assert_eq!(r.opened(), 1);
    r.poll_at(270);
    assert_eq!(r.opened(), 2, "следующая попытка через 0.25 с");
    assert!(
        failed_events(&r.h.handle.poll_events()).is_empty(),
        "повторные неудачи не дают новых Failed"
    );
}

#[test]
fn a_stream_that_dies_before_its_first_callback_waits_longer_each_time() {
    let mut r = Rig::new(true);
    r.poll_at(0);
    r.set_lost(0);
    r.poll_at(0);
    assert_eq!(r.opened(), 1, "первая попытка после открытия ждёт 0.25 с");
    r.poll_at(249);
    assert_eq!(r.opened(), 1);
    r.poll_at(250);
    assert_eq!(r.opened(), 2);

    r.set_lost(1);
    r.poll_at(250);
    r.poll_at(749);
    assert_eq!(r.opened(), 2, "после второго сбоя без callback ждём 0.5 с");
    r.poll_at(750);
    assert_eq!(r.opened(), 3);
}

#[test]
fn without_output_the_device_never_tries_to_open() {
    let mut r = Rig::new(false);
    for ms in (0..10_000).step_by(100) {
        r.poll_at(ms);
    }
    assert_eq!(r.opened(), 0);
    assert_eq!(r.device.retry_in(r.base), None);
    assert!(r.h.handle.poll_events().is_empty());
}

#[test]
fn a_loss_is_handled_before_the_commands_of_the_same_wakeup() {
    let mut r = Rig::new(true);
    r.poll_at(0);
    r.h.handle
        .play(voice(1), "audio/hum.wav", request(false))
        .unwrap();
    r.h.reads.send(Ok(wav_bytes(2205))).unwrap();
    r.h.step();
    assert_eq!(
        r.played(0),
        vec![voice(1)],
        "голос 1 звучит на первом устройстве"
    );

    r.set_alive(0);
    r.poll_at(1);
    r.set_lost(0);
    r.h.handle
        .play(voice(2), "audio/hum.wav", request(false))
        .unwrap();
    r.poll_at(2);
    r.h.step();

    let events = r.h.handle.poll_events();
    assert_eq!(voice_ended_count(&events, voice(1)), 1);
    assert_eq!(voice_ended_count(&events, voice(2)), 0);
    assert_eq!(
        r.played(1),
        vec![voice(2)],
        "команда дошла до нового устройства"
    );
}

#[test]
fn a_voice_in_the_mixer_ends_once_when_the_device_is_lost() {
    let mut r = Rig::new(true);
    r.poll_at(0);
    r.h.handle
        .play(voice(1), "audio/hum.wav", request(false))
        .unwrap();
    r.h.reads.send(Ok(wav_bytes(2205))).unwrap();
    r.h.step();
    let _ = r.received(0);
    r.callback_ended(0, voice(1));
    r.set_alive(0);
    r.poll_at(0);
    r.set_lost(0);
    r.poll_at(0);

    let events = r.h.handle.poll_events();
    assert_eq!(voice_ended_count(&events, voice(1)), 1, "{events:?}");
    assert_eq!(r.h.handle.lane_of(voice(1)), None);
    assert_eq!(r.opened(), 2, "открыто заново");
}

#[test]
fn a_voice_the_old_ring_still_holds_ends_once() {
    let mut r = Rig::new(true);
    r.poll_at(0);
    r.h.handle
        .play(voice(1), "audio/hum.wav", request(false))
        .unwrap();
    r.h.reads.send(Ok(wav_bytes(2205))).unwrap();
    r.h.step();
    r.callback_ended(0, voice(1));
    r.set_lost(0);
    r.poll_at(0);
    r.h.core.mixer_ended(voice(1));

    let events = r.h.handle.poll_events();
    assert_eq!(voice_ended_count(&events, voice(1)), 1, "{events:?}");
}

#[test]
fn a_voice_pushed_into_a_full_ring_ends_when_the_device_is_lost() {
    let mut r = Rig::new(true);
    r.poll_at(0);
    r.h.handle
        .play(voice(100), "audio/hum.wav", request(false))
        .unwrap();
    r.h.reads.send(Ok(wav_bytes(2205))).unwrap();
    r.h.step();
    for index in 0..MIXER_CAPACITY {
        r.h.core.send_mixer(MixerCommand::SetGroupVolume {
            group: AudioGroup::Music,
            volume: index as f32,
        });
    }
    r.h.handle
        .play(voice(9), "audio/hum.wav", request(false))
        .unwrap();
    r.h.step();
    assert!(!r.h.core.outbox.is_empty(), "команда ждёт места в кольце");

    r.set_lost(0);
    r.poll_at(0);
    assert!(
        r.h.core
            .outbox
            .iter()
            .all(|command| !matches!(command, MixerCommand::Play { .. }))
    );
    let events = r.h.handle.poll_events();
    assert_eq!(voice_ended_count(&events, voice(9)), 1, "{events:?}");
    assert_eq!(voice_ended_count(&events, voice(100)), 1, "{events:?}");
}

#[test]
fn a_voice_that_has_no_device_ends_at_once_and_plays_after_the_reopen() {
    let mut r = Rig::new(true);
    r.poll_at(0);
    r.h.handle
        .play(voice(1), "audio/hum.wav", request(false))
        .unwrap();
    r.h.reads.send(Ok(wav_bytes(2205))).unwrap();
    r.h.step();
    r.set_alive(0);
    r.poll_at(1);
    r.set_lost(0);
    r.fail_next(1);
    r.poll_at(2);
    let _ = r.h.handle.poll_events();

    r.h.handle
        .play(voice(2), "audio/hum.wav", request(false))
        .unwrap();
    r.h.step();
    assert_eq!(voice_ended_count(&r.h.handle.poll_events(), voice(2)), 1);

    r.poll_at(252);
    r.h.handle
        .play(voice(3), "audio/hum.wav", request(false))
        .unwrap();
    r.h.step();
    assert_eq!(
        r.played(1),
        vec![voice(3)],
        "после восстановления звук снова играет"
    );
}

#[test]
fn a_loaded_voice_waiting_for_its_clip_keeps_its_lane_and_plays_on_the_new_device() {
    let mut r = Rig::new(true);
    r.poll_at(0);
    r.h.handle
        .play(voice(1), "audio/new.wav", request(false))
        .unwrap();
    r.h.step();
    assert_eq!(r.h.handle.lane_of(voice(1)), Some(0));

    r.set_alive(0);
    r.poll_at(1);
    r.set_lost(0);
    r.poll_at(1);
    assert_eq!(r.h.handle.lane_of(voice(1)), Some(0), "полоса сохранена");
    assert_eq!(voice_ended_count(&r.h.handle.poll_events(), voice(1)), 0);

    r.h.reads.send(Ok(wav_bytes(2205))).unwrap();
    r.h.step();
    assert_eq!(
        r.played(1),
        vec![voice(1)],
        "после декодирования голос уходит в новое устройство"
    );
}

#[test]
fn a_stream_waiting_for_its_ring_survives_the_loss_and_starts_on_the_new_device() {
    let mut r = Rig::new(true);
    r.poll_at(0);
    r.h.handle
        .play(voice(1), "music/theme.ogg", stream_request(true))
        .unwrap();
    r.h.step();
    let _ = r.h.stream_jobs.try_recv();

    r.set_alive(0);
    r.poll_at(1);
    r.set_lost(0);
    r.poll_at(1);
    assert_eq!(voice_ended_count(&r.h.handle.poll_events(), voice(1)), 0);

    r.h.core.stream_event(StreamEvent::Ready {
        voice: voice(1),
        reader: ready_reader(),
    });
    assert_eq!(
        r.played(1),
        vec![voice(1)],
        "готовый стрим уходит в новое устройство"
    );
}

#[test]
fn mixer_settings_are_restored_first_on_the_new_device() {
    let mut r = Rig::new(true);
    r.poll_at(0);
    let effects = AudioGroupEffects {
        lowpass_hz: 800.0,
        ..AudioGroupEffects::default()
    };
    r.h.handle.set_paused(true).unwrap();
    r.h.handle.set_mono_output(true).unwrap();
    r.h.handle.set_group_volume(AudioGroup::Music, 0.5).unwrap();
    r.h.handle
        .set_group_effects(AudioGroup::Music, effects)
        .unwrap();
    r.h.step();
    let _ = r.received(0);

    r.set_alive(0);
    r.poll_at(1);
    r.set_lost(0);
    r.poll_at(1);
    let restored = r.received(1);
    assert!(
        matches!(
            restored.as_slice(),
            [
                MixerCommand::SetPaused(true),
                MixerCommand::SetMonoOutput(true),
                MixerCommand::SetGroupVolume { group: AudioGroup::Music, volume },
                MixerCommand::SetGroupEffects { group: AudioGroup::Music, effects: restored_effects },
            ] if *volume == 0.5 && *restored_effects == effects
        ),
        "{restored:?}"
    );
}

#[test]
fn a_loss_with_eviction_ends_both_voices_once_and_late_reports_are_ignored() {
    let mut h = Harness::new();
    h.handle
        .play(voice(1), "audio/hum.wav", request(false))
        .unwrap();
    h.reads.send(Ok(wav_bytes(2205))).unwrap();
    h.step();
    for id in 2..=VOICE_SLOTS as u64 {
        h.handle
            .play(voice(id), "audio/hum.wav", request(false))
            .unwrap();
    }
    h.handle
        .play(
            voice(VOICE_SLOTS as u64 + 1),
            "audio/hum.wav",
            request(false),
        )
        .unwrap();
    h.step();

    h.core.device_lost();
    h.core.mixer_ended(voice(1));
    let events = h.handle.poll_events();
    for id in 1..=VOICE_SLOTS as u64 + 1 {
        assert_eq!(voice_ended_count(&events, voice(id)), 1, "голос {id}");
    }
    assert_eq!(h.handle.lane_of(voice(1)), None);
}
