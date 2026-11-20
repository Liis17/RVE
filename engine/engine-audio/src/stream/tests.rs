use super::*;
use crate::decode::{decode_ogg, decode_wav};
use std::sync::Mutex;
use std::time::Instant;

const OGG_FIXTURE: &[u8] = include_bytes!("../../tests/fixtures/tone_mono_22050.ogg");
/// Стерео Ogg из трёх аудиостраниц: обрыв можно поставить на границе страницы после первого долива.
const PAGED_OGG_FIXTURE: &[u8] = include_bytes!("../../tests/fixtures/tone_stereo_22050_2s.ogg");
/// Верхняя граница ожидания событий; в успешном пути ответ приходит за миллисекунды.
const WAIT: Duration = Duration::from_secs(5);

/// Источник в памяти; `fail_after` ломает чтения начиная с указанного номера, `reads` считает их.
struct Memory {
    bytes: Arc<Vec<u8>>,
    reads: Arc<Mutex<Vec<Range<u64>>>>,
    fail_after: Option<usize>,
}

impl ByteSource for Memory {
    fn size(&self) -> u64 {
        self.bytes.len() as u64
    }

    fn read(&mut self, range: Range<u64>) -> Result<Vec<u8>, String> {
        let mut reads = self.reads.lock().unwrap();
        if self.fail_after.is_some_and(|limit| reads.len() >= limit) {
            return Err("диск недоступен".into());
        }
        reads.push(range.clone());
        Ok(self.bytes[range.start as usize..range.end as usize].to_vec())
    }
}

fn memory(bytes: Vec<u8>) -> Box<dyn ByteSource> {
    Box::new(Memory {
        bytes: Arc::new(bytes),
        reads: Arc::default(),
        fail_after: None,
    })
}

/// Файл с обрывом: заявляет `size` байт, а отдаёт только `data`. `stalls` первых чтений
/// возвращают пустой ответ, `max_read` ограничивает ответ одного чтения.
struct Truncated {
    size: u64,
    data: Vec<u8>,
    stalls: usize,
    max_read: u64,
}

impl ByteSource for Truncated {
    fn size(&self) -> u64 {
        self.size
    }

    fn read(&mut self, range: Range<u64>) -> Result<Vec<u8>, String> {
        if self.stalls > 0 {
            self.stalls -= 1;
            return Ok(Vec::new());
        }
        let available = self.data.len() as u64;
        let start = range.start.min(available);
        let end = range
            .end
            .min(available)
            .min(start.saturating_add(self.max_read));
        Ok(self.data[start as usize..end as usize].to_vec())
    }
}

fn truncated(size: usize, data: Vec<u8>) -> Box<dyn ByteSource> {
    Box::new(Truncated {
        size: size as u64,
        data,
        stalls: 0,
        max_read: u64::MAX,
    })
}

/// Концы Ogg-страниц: заголовок с таблицей сегментов и тело страницы.
fn ogg_page_ends(bytes: &[u8]) -> Vec<usize> {
    let mut ends = Vec::new();
    let mut position = 0;
    while position < bytes.len() {
        assert_eq!(&bytes[position..position + 4], b"OggS");
        let segments = usize::from(bytes[position + 26]);
        let body: usize = bytes[position + 27..position + 27 + segments]
            .iter()
            .map(|&length| usize::from(length))
            .sum();
        position += 27 + segments + body;
        ends.push(position);
    }
    ends
}

/// 16-битный стерео WAV: левый канал — номер кадра, правый — с обратным знаком.
fn stereo_wav(frames: usize, rate: u32) -> Vec<u8> {
    let data_len = (frames * 4) as u32;
    let mut out = Vec::new();
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&(36 + 12 + data_len).to_le_bytes());
    out.extend_from_slice(b"WAVE");
    // Неизвестный chunk перед fmt: разбор заголовка должен его пропустить.
    out.extend_from_slice(b"LIST");
    out.extend_from_slice(&4u32.to_le_bytes());
    out.extend_from_slice(b"INFO");
    out.extend_from_slice(b"fmt ");
    out.extend_from_slice(&16u32.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&2u16.to_le_bytes());
    out.extend_from_slice(&rate.to_le_bytes());
    out.extend_from_slice(&(rate * 4).to_le_bytes());
    out.extend_from_slice(&4u16.to_le_bytes());
    out.extend_from_slice(&16u16.to_le_bytes());
    out.extend_from_slice(b"data");
    out.extend_from_slice(&data_len.to_le_bytes());
    for frame in 0..frames {
        let value = (frame % 30_000) as i16;
        out.extend_from_slice(&value.to_le_bytes());
        out.extend_from_slice(&(-value).to_le_bytes());
    }
    out
}

fn drain(reader: &mut StreamReader) -> Vec<Frame> {
    std::iter::from_fn(|| match reader.pull() {
        Pull::Frame(frame) => Some(frame),
        Pull::Underrun | Pull::End => None,
    })
    .collect()
}

fn job(path: &str, looping: bool) -> StreamJob {
    StreamJob {
        voice: VoiceId::from_raw(1).unwrap(),
        path: path.into(),
        looping,
    }
}

fn opener(bytes: Vec<u8>) -> OpenSource {
    let bytes = Arc::new(bytes);
    Box::new(move |_path: &str| {
        Ok(Box::new(Memory {
            bytes: Arc::clone(&bytes),
            reads: Arc::default(),
            fail_after: None,
        }) as Box<dyn ByteSource>)
    })
}

#[test]
fn wav_stream_yields_the_same_frames_as_a_full_decode_and_then_ends() {
    let bytes = stereo_wav(20_000, 22_050);
    let clip = decode_wav(&bytes).unwrap();
    let (mut active, mut reader) =
        open_stream(&mut opener(bytes), &job("music/theme.wav", false)).unwrap();
    assert_eq!((reader.sample_rate, reader.channels), (22_050, 2));

    let mut frames = Vec::new();
    loop {
        frames.extend(drain(&mut reader));
        if reader.pull() == Pull::End {
            break;
        }
        active.fill().unwrap();
    }
    let expected: Vec<Frame> = clip.samples.as_chunks::<2>().0.to_vec();
    assert_eq!(frames, expected);
}

#[test]
fn ring_holds_only_about_half_a_second_not_the_whole_clip() {
    let rate = 48_000;
    let (_active, mut reader) = open_stream(
        &mut opener(stereo_wav(rate as usize * 10, rate)),
        &job("music/long.wav", false),
    )
    .unwrap();
    let buffered = drain(&mut reader).len();
    assert_eq!(buffered, (rate as f32 * BUFFER_SECONDS) as usize);
    assert_eq!(
        reader.pull(),
        Pull::Underrun,
        "стрим не закончился, а ждёт долива"
    );
}

#[test]
fn wav_header_is_read_in_small_ranges_instead_of_the_whole_file() {
    let bytes = stereo_wav(48_000 * 4, 48_000);
    let len = bytes.len() as u64;
    let reads = Arc::new(Mutex::new(Vec::new()));
    let source = Box::new(Memory {
        bytes: Arc::new(bytes),
        reads: Arc::clone(&reads),
        fail_after: None,
    });
    let mut decoder = WavStream::open(source).unwrap();
    let mut out = Vec::new();
    assert!(decoder.decode(&mut out).unwrap());
    let reads = reads.lock().unwrap();
    assert!(
        reads
            .iter()
            .all(|range| range.end - range.start <= READ_CHUNK_BYTES),
        "{reads:?}"
    );
    assert!(
        reads
            .iter()
            .map(|range| range.end - range.start)
            .sum::<u64>()
            < len / 4
    );
}

#[test]
fn looping_stream_wraps_to_the_start_without_ending() {
    let bytes = stereo_wav(100, 22_050);
    let (mut active, mut reader) =
        open_stream(&mut opener(bytes), &job("ambient/wind.wav", true)).unwrap();
    let first = drain(&mut reader);
    assert!(first.len() > 100, "кольцо заполнено несколькими проходами");
    assert_eq!(first[0], first[100], "после конца клип начинается заново");
    active.fill().unwrap();
    assert!(matches!(reader.pull(), Pull::Frame(_)));
}

#[test]
fn empty_looping_clip_ends_instead_of_spinning() {
    let bytes = stereo_wav(0, 22_050);
    let (_active, mut reader) =
        open_stream(&mut opener(bytes), &job("ambient/empty.wav", true)).unwrap();
    assert_eq!(reader.pull(), Pull::End);
}

#[test]
fn ogg_stream_matches_a_full_decode_and_rewinds_when_looping() {
    let clip = decode_ogg(OGG_FIXTURE, &AtomicBool::new(false)).unwrap();
    let (mut active, mut reader) = open_stream(
        &mut opener(OGG_FIXTURE.to_vec()),
        &job("music/tone.ogg", false),
    )
    .unwrap();
    assert_eq!((reader.sample_rate, reader.channels), (22_050, 1));
    let mut frames = Vec::new();
    loop {
        frames.extend(drain(&mut reader));
        if reader.pull() == Pull::End {
            break;
        }
        active.fill().unwrap();
    }
    let expected: Vec<Frame> = clip.samples.iter().map(|&value| [value, value]).collect();
    assert_eq!(frames, expected);

    let mut decoder = OggStream::open(memory(OGG_FIXTURE.to_vec())).unwrap();
    let mut first = Vec::new();
    while decoder.decode(&mut first).unwrap() {}
    decoder.rewind().unwrap();
    let mut second = Vec::new();
    while decoder.decode(&mut second).unwrap() {}
    assert_eq!(first, second);
}

#[test]
fn unreadable_or_corrupt_files_fail_to_open() {
    let mut failing: OpenSource = Box::new(|_path: &str| Err("нет файла".to_owned()));
    assert!(open_stream(&mut failing, &job("music/missing.wav", false)).is_err());
    let corrupt = open_stream(&mut opener(b"not a wav".to_vec()), &job("a.wav", false));
    assert!(corrupt.is_err());
    let unsupported = open_stream(&mut opener(stereo_wav(10, 22_050)), &job("a.mp3", false));
    assert!(matches!(unsupported, Err(message) if message.contains("mp3")));
}

#[test]
fn read_error_mid_stream_is_reported_and_the_voice_drains_then_ends() {
    let rate = 22_050;
    let bytes = Arc::new(stereo_wav(rate as usize * 4, rate));
    // Заголовок (3 чтения) и первые диапазоны проходят, дальше диск отказывает.
    let mut open: OpenSource = Box::new(move |_path: &str| {
        Ok(Box::new(Memory {
            bytes: Arc::clone(&bytes),
            reads: Arc::default(),
            fail_after: Some(6),
        }) as Box<dyn ByteSource>)
    });
    let (active, mut reader) = open_stream(&mut open, &job("music/broken.wav", false)).unwrap();
    let (events_tx, events_rx) = flume::unbounded();
    let mut streams = vec![active];
    let buffered = drain(&mut reader).len();
    assert!(buffered > 0);
    refill(&mut streams, &events_tx);
    assert!(matches!(
        events_rx.try_recv(),
        Ok(StreamEvent::Broken { path, .. }) if path == "music/broken.wav"
    ));
    drain(&mut reader);
    assert_eq!(reader.pull(), Pull::End);
}

#[test]
fn refill_drops_streams_whose_reader_is_gone() {
    let (active, reader) = open_stream(
        &mut opener(stereo_wav(48_000, 48_000)),
        &job("music/theme.wav", true),
    )
    .unwrap();
    let (events_tx, _events_rx) = flume::unbounded();
    let mut streams = vec![active];
    refill(&mut streams, &events_tx);
    assert_eq!(streams.len(), 1);
    drop(reader);
    refill(&mut streams, &events_tx);
    assert!(streams.is_empty());
}

#[test]
fn stream_loop_reports_ready_and_failed_opens_and_exits_when_jobs_close() {
    let (jobs_tx, jobs_rx) = flume::unbounded();
    let (events_tx, events_rx) = flume::unbounded();
    let bytes = Arc::new(stereo_wav(1000, 22_050));
    let open: OpenSource = Box::new(move |path: &str| {
        if path.contains("missing") {
            return Err("нет файла".into());
        }
        Ok(Box::new(Memory {
            bytes: Arc::clone(&bytes),
            reads: Arc::default(),
            fail_after: None,
        }) as Box<dyn ByteSource>)
    });
    jobs_tx.send(job("music/theme.wav", false)).unwrap();
    jobs_tx
        .send(StreamJob {
            voice: VoiceId::from_raw(2).unwrap(),
            path: "music/missing.wav".into(),
            looping: false,
        })
        .unwrap();
    drop(jobs_tx);
    stream_loop(jobs_rx, events_tx, open, &AtomicBool::new(false));
    let events: Vec<StreamEvent> = events_rx.try_iter().collect();
    assert!(matches!(&events[0], StreamEvent::Ready { voice, .. } if voice.get() == 1));
    assert!(matches!(
        &events[1],
        StreamEvent::OpenFailed { voice, message, .. } if voice.get() == 2 && message.contains("нет файла")
    ));
}

#[test]
fn source_reader_reports_truncation_inside_the_declared_size() {
    let mut reader = SourceReader::new(truncated(100, vec![7; 40]));
    let mut bytes = Vec::new();
    let error = reader.read_to_end(&mut bytes).unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::UnexpectedEof);
    assert_eq!(bytes.len(), 40, "данные до обрыва отданы");
    let message = error.to_string();
    assert!(
        message.contains("позиция 40") && message.contains("ожидалось 100"),
        "{message}"
    );
}

#[test]
fn source_reader_ends_quietly_only_at_the_declared_size() {
    let mut whole = SourceReader::new(truncated(40, vec![7; 40]));
    let mut bytes = Vec::new();
    assert_eq!(whole.read_to_end(&mut bytes).unwrap(), 40);
    assert_eq!(whole.read(&mut [0u8; 8]).unwrap(), 0);

    let mut cut = SourceReader::new(truncated(100, vec![7; 40]));
    assert_eq!(
        cut.read(&mut []).unwrap(),
        0,
        "пустой вывод не читает источник и не считается обрывом"
    );
}

#[test]
fn source_reader_treats_a_short_non_empty_read_as_partial() {
    let data: Vec<u8> = (0..=255).collect();
    let source = Box::new(Truncated {
        size: 256,
        data: data.clone(),
        stalls: 0,
        max_read: 7,
    });
    let mut reader = SourceReader::new(source);
    let mut bytes = Vec::new();
    assert_eq!(reader.read_to_end(&mut bytes).unwrap(), 256);
    assert_eq!(bytes, data);
}

#[test]
fn source_reader_asks_the_source_again_after_an_empty_answer() {
    let data: Vec<u8> = (0..100).collect();
    let source = Box::new(Truncated {
        size: 100,
        data: data.clone(),
        stalls: 1,
        max_read: 100,
    });
    let mut reader = SourceReader::new(source);
    let mut bytes = Vec::new();
    assert_eq!(
        reader.read_to_end(&mut bytes).unwrap_err().kind(),
        io::ErrorKind::UnexpectedEof
    );
    assert_eq!(reader.read_to_end(&mut bytes).unwrap(), 100);
    assert_eq!(
        bytes, data,
        "пустой ответ не запоминается как прочитанный диапазон"
    );
}

/// Регрессия: Ogg, оборванный ровно на границе страницы после `Ready`, при заявленном размере
/// больше фактического. Без проверки обрыва ogg принимает пустое чтение за конец потока и клип
/// тихо заканчивается.
#[test]
fn stream_loop_reports_a_file_cut_on_a_page_boundary_as_broken() {
    let rate = 22_050;
    let full = PAGED_OGG_FIXTURE;
    // Конец второй аудиостраницы (21504 кадра): первый долив кольца (11025 кадров) не доходит до конца.
    let cut = ogg_page_ends(full)[2];
    assert!(cut < full.len(), "оборван не весь файл");
    let open: OpenSource =
        Box::new(move |_path: &str| Ok(truncated(full.len(), full[..cut].to_vec())));

    let (jobs_tx, jobs_rx) = flume::unbounded();
    let (events_tx, events_rx) = flume::unbounded();
    let cancel = Arc::new(AtomicBool::new(false));
    let worker = {
        let cancel = Arc::clone(&cancel);
        std::thread::spawn(move || stream_loop(jobs_rx, events_tx, open, &cancel))
    };
    jobs_tx.send(job("music/tone.ogg", false)).unwrap();
    let mut reader = match events_rx.recv_timeout(WAIT) {
        Ok(StreamEvent::Ready { reader, .. }) => reader,
        _ => panic!("клип должен открыться до обрыва"),
    };

    // Потребитель освобождает кольцо, поток стриминга доливает его и доходит до обрыва.
    let deadline = Instant::now() + WAIT;
    let mut frames = 0usize;
    loop {
        match reader.pull() {
            Pull::Frame(_) => frames += 1,
            Pull::Underrun => {
                assert!(Instant::now() < deadline, "кольцо перестало доливаться");
                std::thread::yield_now();
            }
            Pull::End => break,
        }
    }
    let event = events_rx.recv_timeout(WAIT);
    cancel.store(true, Ordering::Relaxed);
    worker.join().unwrap();

    assert!(
        frames > (rate as f32 * BUFFER_SECONDS) as usize,
        "обрыв должен прийти после первого долива, отдано {frames} кадров"
    );
    assert!(
        matches!(
            &event,
            Ok(StreamEvent::Broken { path, .. }) if path == "music/tone.ogg"
        ),
        "обрыв на границе страницы должен быть Broken, а не тихим концом клипа"
    );
}
