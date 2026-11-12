//! Стриминг длинных клипов: файл читается диапазонами и декодируется по частям в потоке
//! `rve-audio-stream`, а голос микшера забирает кадры из SPSC-кольца `rtrb`.
//!
//! В памяти держится только кольцо (около [`BUFFER_SECONDS`] звука) и один прочитанный
//! диапазон файла, а не весь декодированный клип.
//!
//! Владение: кольцо выделяет поток стриминга. Голос микшера держит [`StreamReader`], а по
//! завершении callback возвращает его потоку `rve-audio` через отдельное кольцо, потому что
//! callback не освобождает память. Поток стриминга держит свой конец, пока читатель жив.

use crate::decode::{DecodeError, WavFormat, convert, parse_fmt};
use engine_scene::VoiceId;
use flume::{Receiver, RecvTimeoutError, Sender};
use lewton::inside_ogg::OggStreamReader;
use rtrb::{Consumer, Producer, RingBuffer};
use std::fmt;
use std::io::{self, Read, Seek, SeekFrom};
use std::ops::Range;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

/// Стерео-кадр; у моно-клипа оба канала равны.
pub type Frame = [f32; 2];

/// Сколько секунд звука держит кольцо одного голоса.
pub const BUFFER_SECONDS: f32 = 0.5;
/// Наименьшее кольцо в кадрах: короткие клипы и низкие частоты.
const MIN_BUFFER_FRAMES: usize = 4096;
/// Размер одного чтения файла.
const READ_CHUNK_BYTES: u64 = 64 * 1024;
/// Период долива колец.
const REFILL_PERIOD: Duration = Duration::from_millis(10);
/// Наибольший chunk `fmt ` WAV, который читается при разборе заголовка.
const MAX_FMT_BYTES: u64 = 4096;

/// Блокирующее чтение диапазонов файла. Вызывается только из потока стриминга.
pub trait ByteSource: Send {
    /// Размер файла в байтах.
    fn size(&self) -> u64;
    /// Читает `[start, end)`; диапазон лежит внутри файла.
    fn read(&mut self, range: Range<u64>) -> Result<Vec<u8>, String>;
}

/// Открывает источник байт по виртуальному пути.
pub type OpenSource = Box<dyn FnMut(&str) -> Result<Box<dyn ByteSource>, String> + Send>;

struct Shared {
    /// Поток стриминга записал последний кадр незацикленного клипа.
    finished: AtomicBool,
}

/// Конец кольца у голоса микшера. Не выделяет и не освобождает память при чтении.
pub struct StreamReader {
    frames: Consumer<Frame>,
    shared: Arc<Shared>,
    pub sample_rate: u32,
    pub channels: u16,
}

/// Результат чтения кадра из кольца.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Pull {
    Frame(Frame),
    /// Кольцо пусто, но поток ещё пишет: голос звучит тишиной до новых кадров.
    Underrun,
    /// Клип закончился.
    End,
}

impl StreamReader {
    pub fn pull(&mut self) -> Pull {
        if let Ok(frame) = self.frames.pop() {
            return Pull::Frame(frame);
        }
        if !self.shared.finished.load(Ordering::Acquire) {
            return Pull::Underrun;
        }
        // Кадры могли прийти между неудачным `pop` и чтением флага.
        match self.frames.pop() {
            Ok(frame) => Pull::Frame(frame),
            Err(_) => Pull::End,
        }
    }
}

impl fmt::Debug for StreamReader {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("StreamReader")
            .field("sample_rate", &self.sample_rate)
            .field("channels", &self.channels)
            .finish_non_exhaustive()
    }
}

/// Задание потоку стриминга.
pub(crate) struct StreamJob {
    pub voice: VoiceId,
    pub path: String,
    pub looping: bool,
}

/// Ответ потока стриминга потоку `rve-audio`.
pub(crate) enum StreamEvent {
    /// Кольцо заполнено, голос можно отдать микшеру.
    Ready {
        voice: VoiceId,
        reader: StreamReader,
    },
    /// Клип не открылся; голос не начнёт звучать.
    OpenFailed {
        voice: VoiceId,
        path: String,
        message: String,
    },
    /// Ошибка чтения или декодирования во время воспроизведения; голос доиграет буфер и закончится.
    Broken { path: String, message: String },
}

/// Декодер, выдающий кадры порциями.
trait FrameDecoder: Send {
    fn sample_rate(&self) -> u32;
    fn channels(&self) -> u16;
    /// Добавляет в `out` следующую порцию кадров; `false` — конец клипа.
    fn decode(&mut self, out: &mut Vec<Frame>) -> Result<bool, DecodeError>;
    /// Возвращается к началу клипа.
    fn rewind(&mut self) -> Result<(), DecodeError>;
}

/// Открывает декодер по расширению (`wav` или `ogg`, без учёта регистра).
fn open_decoder(
    path: &str,
    source: Box<dyn ByteSource>,
) -> Result<Box<dyn FrameDecoder>, DecodeError> {
    let extension = Path::new(path)
        .extension()
        .and_then(|value| value.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    match extension.as_str() {
        "wav" => Ok(Box::new(WavStream::open(source)?)),
        "ogg" => Ok(Box::new(OggStream::open(source)?)),
        other => Err(DecodeError::Unsupported(format!(
            "расширение .{other} не поддерживается"
        ))),
    }
}

fn read_error(message: String) -> DecodeError {
    DecodeError::Corrupt(format!("чтение: {message}"))
}

struct WavStream {
    source: Box<dyn ByteSource>,
    format: WavFormat,
    data: Range<u64>,
    cursor: u64,
}

impl WavStream {
    fn open(mut source: Box<dyn ByteSource>) -> Result<Self, DecodeError> {
        let len = source.size();
        if len < 12 {
            return Err(corrupt("нет заголовка RIFF/WAVE"));
        }
        let header = source.read(0..12).map_err(read_error)?;
        if &header[0..4] != b"RIFF" || &header[8..12] != b"WAVE" {
            return Err(corrupt("нет заголовка RIFF/WAVE"));
        }
        let mut format = None;
        let mut position = 12u64;
        while position + 8 <= len {
            let chunk = source.read(position..position + 8).map_err(read_error)?;
            let size = u64::from(u32::from_le_bytes([chunk[4], chunk[5], chunk[6], chunk[7]]));
            let body = position + 8;
            let available = len - body;
            if &chunk[0..4] == b"fmt " {
                if size > available || !(16..=MAX_FMT_BYTES).contains(&size) {
                    return Err(corrupt("обрезанный chunk fmt"));
                }
                let bytes = source.read(body..body + size).map_err(read_error)?;
                format = Some(parse_fmt(&bytes)?);
            } else if &chunk[0..4] == b"data" {
                let format = format.ok_or_else(|| corrupt("chunk data раньше fmt"))?;
                if size > available {
                    return Err(corrupt(&format!(
                        "data заявлено {size} байт, в файле {available}"
                    )));
                }
                let frame_bytes = (format.bytes_per_sample * usize::from(format.channels)) as u64;
                if !size.is_multiple_of(frame_bytes) {
                    return Err(corrupt("data не делится на целое число кадров"));
                }
                return Ok(Self {
                    source,
                    format,
                    data: body..body + size,
                    cursor: body,
                });
            }
            position = body + size + (size & 1);
        }
        Err(corrupt("нет chunk data"))
    }
}

impl FrameDecoder for WavStream {
    fn sample_rate(&self) -> u32 {
        self.format.sample_rate
    }

    fn channels(&self) -> u16 {
        self.format.channels
    }

    fn decode(&mut self, out: &mut Vec<Frame>) -> Result<bool, DecodeError> {
        if self.cursor >= self.data.end {
            return Ok(false);
        }
        let frame_bytes = (self.format.bytes_per_sample * usize::from(self.format.channels)) as u64;
        let chunk = (READ_CHUNK_BYTES / frame_bytes).max(1) * frame_bytes;
        let end = (self.cursor + chunk).min(self.data.end);
        let bytes = self.source.read(self.cursor..end).map_err(read_error)?;
        if bytes.len() as u64 != end - self.cursor {
            return Err(corrupt("прочитано меньше байт, чем запрошено"));
        }
        self.cursor = end;
        let step = self.format.bytes_per_sample;
        for frame in bytes.chunks_exact(frame_bytes as usize) {
            let left = convert(&frame[..step], self.format.samples)?;
            let right = if self.format.channels == 2 {
                convert(&frame[step..2 * step], self.format.samples)?
            } else {
                left
            };
            out.push([left, right]);
        }
        Ok(true)
    }

    fn rewind(&mut self) -> Result<(), DecodeError> {
        self.cursor = self.data.start;
        Ok(())
    }
}

/// `Read + Seek` поверх [`ByteSource`] с буфером одного диапазона: lewton читает мелкими кусками.
struct SourceReader {
    source: Box<dyn ByteSource>,
    len: u64,
    position: u64,
    buffer: Vec<u8>,
    buffer_start: u64,
}

impl SourceReader {
    fn new(source: Box<dyn ByteSource>) -> Self {
        Self {
            len: source.size(),
            source,
            position: 0,
            buffer: Vec::new(),
            buffer_start: 0,
        }
    }
}

impl Read for SourceReader {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        if self.position >= self.len || out.is_empty() {
            return Ok(0);
        }
        let buffered = self.buffer_start..self.buffer_start + self.buffer.len() as u64;
        if !buffered.contains(&self.position) {
            let end = (self.position + READ_CHUNK_BYTES).min(self.len);
            self.buffer = self
                .source
                .read(self.position..end)
                .map_err(io::Error::other)?;
            self.buffer_start = self.position;
            if self.buffer.is_empty() {
                return Ok(0);
            }
        }
        let offset = (self.position - self.buffer_start) as usize;
        let count = out.len().min(self.buffer.len() - offset);
        out[..count].copy_from_slice(&self.buffer[offset..offset + count]);
        self.position += count as u64;
        Ok(count)
    }
}

impl Seek for SourceReader {
    fn seek(&mut self, target: SeekFrom) -> io::Result<u64> {
        let position = match target {
            SeekFrom::Start(offset) => Some(offset),
            SeekFrom::End(offset) => self.len.checked_add_signed(offset),
            SeekFrom::Current(offset) => self.position.checked_add_signed(offset),
        }
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "смещение вне файла"))?;
        self.position = position;
        Ok(position)
    }
}

struct OggStream {
    reader: Option<OggStreamReader<SourceReader>>,
    sample_rate: u32,
    channels: u16,
}

impl OggStream {
    fn open(source: Box<dyn ByteSource>) -> Result<Self, DecodeError> {
        let reader = OggStreamReader::new(SourceReader::new(source))
            .map_err(|error| corrupt(&format!("ogg: {error}")))?;
        let channels = u16::from(reader.ident_hdr.audio_channels);
        if !(1..=2).contains(&channels) {
            return Err(DecodeError::Unsupported(format!(
                "{channels} каналов; поддерживаются моно и стерео"
            )));
        }
        Ok(Self {
            sample_rate: reader.ident_hdr.audio_sample_rate,
            channels,
            reader: Some(reader),
        })
    }
}

impl FrameDecoder for OggStream {
    fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    fn channels(&self) -> u16 {
        self.channels
    }

    fn decode(&mut self, out: &mut Vec<Frame>) -> Result<bool, DecodeError> {
        let Some(reader) = self.reader.as_mut() else {
            return Err(corrupt("ogg: поток не восстановился после перемотки"));
        };
        let packet = match reader.read_dec_packet_generic::<Vec<Vec<f32>>>() {
            Ok(Some(packet)) => packet,
            Ok(None) => return Ok(false),
            Err(error) => return Err(corrupt(&format!("ogg: {error}"))),
        };
        if packet.len() != usize::from(self.channels) {
            return Err(corrupt("число каналов пакета не совпадает с заголовком"));
        }
        let left = &packet[0];
        let right = packet.get(1).unwrap_or(left);
        out.extend(left.iter().zip(right).map(|(&l, &r)| [l, r]));
        Ok(true)
    }

    fn rewind(&mut self) -> Result<(), DecodeError> {
        let reader = self
            .reader
            .take()
            .ok_or_else(|| corrupt("ogg: поток не восстановился после перемотки"))?;
        let mut source = reader.into_inner().into_inner();
        source
            .seek(SeekFrom::Start(0))
            .map_err(|error| corrupt(&format!("ogg: {error}")))?;
        let reader =
            OggStreamReader::new(source).map_err(|error| corrupt(&format!("ogg: {error}")))?;
        self.reader = Some(reader);
        Ok(())
    }
}

fn corrupt(message: &str) -> DecodeError {
    DecodeError::Corrupt(message.to_owned())
}

/// Открытый стрим в потоке стриминга.
struct Active {
    path: String,
    decoder: Box<dyn FrameDecoder>,
    frames: Producer<Frame>,
    shared: Arc<Shared>,
    looping: bool,
    /// Декодированные кадры, которые ещё не влезли в кольцо.
    pending: Vec<Frame>,
    pending_at: usize,
    /// Кадров с последней перемотки: пустой зацикленный клип не должен крутиться бесконечно.
    since_rewind: usize,
    done: bool,
}

impl Active {
    /// Доливает кольцо. Ошибка заканчивает стрим: голос доиграет то, что уже в кольце.
    fn fill(&mut self) -> Result<(), DecodeError> {
        while !self.done && self.frames.slots() > 0 {
            if self.pending_at < self.pending.len() {
                let free = self.frames.slots();
                let end = (self.pending_at + free).min(self.pending.len());
                for &frame in &self.pending[self.pending_at..end] {
                    // Место проверено выше; у кольца единственный писатель.
                    let _ = self.frames.push(frame);
                }
                self.pending_at = end;
                continue;
            }
            self.pending.clear();
            self.pending_at = 0;
            let more = match self.decoder.decode(&mut self.pending) {
                Ok(more) => more,
                Err(error) => {
                    self.finish();
                    return Err(error);
                }
            };
            self.since_rewind += self.pending.len();
            if more {
                continue;
            }
            if self.looping && self.since_rewind > 0 {
                self.since_rewind = 0;
                if let Err(error) = self.decoder.rewind() {
                    self.finish();
                    return Err(error);
                }
            } else if self.pending.is_empty() {
                self.finish();
            }
        }
        Ok(())
    }

    fn finish(&mut self) {
        self.done = true;
        self.shared.finished.store(true, Ordering::Release);
    }
}

/// Открывает стрим и заполняет его кольцо.
fn open_stream(open: &mut OpenSource, job: &StreamJob) -> Result<(Active, StreamReader), String> {
    let source = open(&job.path)?;
    let decoder = open_decoder(&job.path, source).map_err(|error| error.to_string())?;
    let sample_rate = decoder.sample_rate();
    if sample_rate == 0 {
        return Err(DecodeError::Corrupt("частота дискретизации 0".into()).to_string());
    }
    let capacity = ((sample_rate as f32 * BUFFER_SECONDS) as usize).max(MIN_BUFFER_FRAMES);
    let (producer, consumer) = RingBuffer::new(capacity);
    let shared = Arc::new(Shared {
        finished: AtomicBool::new(false),
    });
    let reader = StreamReader {
        frames: consumer,
        shared: Arc::clone(&shared),
        sample_rate,
        channels: decoder.channels(),
    };
    let mut active = Active {
        path: job.path.clone(),
        decoder,
        frames: producer,
        shared,
        looping: job.looping,
        pending: Vec::new(),
        pending_at: 0,
        since_rewind: 0,
        done: false,
    };
    active.fill().map_err(|error| error.to_string())?;
    Ok((active, reader))
}

/// Поток `rve-audio-stream`: открывает стримы и доливает их кольца, пока читатель жив.
pub(crate) fn stream_loop(
    jobs: Receiver<StreamJob>,
    events: Sender<StreamEvent>,
    mut open: OpenSource,
    cancel: &AtomicBool,
) {
    let mut active: Vec<Active> = Vec::new();
    loop {
        if cancel.load(Ordering::Relaxed) {
            break;
        }
        match jobs.recv_timeout(REFILL_PERIOD) {
            Ok(job) => {
                let event = match open_stream(&mut open, &job) {
                    Ok((stream, reader)) => {
                        active.push(stream);
                        StreamEvent::Ready {
                            voice: job.voice,
                            reader,
                        }
                    }
                    Err(message) => StreamEvent::OpenFailed {
                        voice: job.voice,
                        path: job.path,
                        message,
                    },
                };
                if events.send(event).is_err() {
                    break;
                }
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break,
        }
        refill(&mut active, &events);
    }
}

/// Убирает стримы без читателя и доливает остальные.
fn refill(active: &mut Vec<Active>, events: &Sender<StreamEvent>) {
    active.retain(|stream| !stream.frames.is_abandoned());
    for stream in active.iter_mut() {
        if let Err(error) = stream.fill() {
            // Лог пишет поток `rve-audio` вместе с событием `Failed`.
            let _ = events.send(StreamEvent::Broken {
                path: stream.path.clone(),
                message: error.to_string(),
            });
        }
    }
}

/// Кольцо с готовыми кадрами для тестов микшера; `finished` помечает конец клипа.
#[cfg(test)]
pub(crate) fn test_reader(
    frames: &[Frame],
    sample_rate: u32,
    finished: bool,
) -> (Producer<Frame>, StreamReader) {
    let (mut producer, consumer) = RingBuffer::new(frames.len().max(1));
    for &frame in frames {
        producer.push(frame).expect("кольцо вмещает все кадры");
    }
    let reader = StreamReader {
        frames: consumer,
        shared: Arc::new(Shared {
            finished: AtomicBool::new(finished),
        }),
        sample_rate,
        channels: 2,
    };
    (producer, reader)
}

#[cfg(test)]
mod tests;
