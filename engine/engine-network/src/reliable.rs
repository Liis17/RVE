//! Надёжность и порядок доставки поверх зашифрованных пакетов.
//!
//! Пакет несёт кадры каналов. Надёжные кадры хранятся до подтверждения и при потере (по таймауту
//! или по трём более новым подтверждениям) переотправляются в новом пакете. Приёмник восстанавливает
//! порядок по номерам кадров канала и собирает фрагменты. Подтверждение — наибольший принятый номер
//! пакета и 64-битная маска предыдущих.

use std::collections::{BTreeMap, VecDeque};
use std::time::{Duration, Instant};

use crate::codec::Reader;
use crate::packet::{FRAGMENT_PAYLOAD, MAX_MESSAGE_BYTES};
use crate::{Channel, NetError, check_message_size};

const CHANNELS: usize = 4;
/// Размер кадра без полезной нагрузки: канал, номер, фрагмент, число фрагментов, длина.
pub(crate) const FRAME_HEADER: usize = 1 + 4 + 2 + 2 + 2;
/// Наибольшее число неподтверждённых пакетов с надёжными кадрами. Пакет несёт не больше одной датаграммы,
/// поэтому объём надёжных кадров в полёте ограничен этим числом.
const MAX_INFLIGHT: usize = 64;
/// Суммарный объём надёжных кадров в очередях одного пира.
const MAX_QUEUED_BYTES: usize = 512 * 1024;
/// Объём ненадёжных кадров в очередях. При переполнении вытесняются самые старые кадры.
const MAX_SOFT_QUEUED_BYTES: usize = 256 * 1024;
/// Насколько новый надёжный кадр канала может опережать старейший неподтверждённый кадр того же канала.
/// Приёмник держит не больше `MAX_SEQ_GAP - 1` ожидающих кадров на канал. Два надёжных канала с кадрами
/// по `FRAGMENT_PAYLOAD` байт и оценкой записи `BUFFER_ENTRY_COST` занимают
/// 2 × (512 − 1) × (1000 + 64) = 1 087 408 байт, что укладывается в `MAX_BUFFERED_BYTES` (2 097 152).
/// Поэтому при одновременной потере головных кадров обоих каналов честный пир не переполняет буфер приёма.
const MAX_SEQ_GAP: i32 = 512;
/// Насколько далеко вперёд приёмник принимает кадры канала. Окно широкое: при потере одного пакета
/// отправитель продолжает слать мелкие сообщения, и их число за время повтора велико. Память ограничивает
/// `MAX_BUFFERED_BYTES`, а не число кадров.
const REORDER_WINDOW: u32 = 65_536;
/// Суммарный объём кадров, ожидающих своей очереди, на одно соединение, с учётом накладных расходов записи.
const MAX_BUFFERED_BYTES: usize = 2 * 1024 * 1024;
/// Оценка памяти на одну запись буфера приёма помимо полезной нагрузки.
const BUFFER_ENTRY_COST: usize = 64;
// Бюджет приёма проверяется на этапе компиляции: худший случай двух каналов укладывается в буфер.
const _: () = assert!(
    2 * (MAX_SEQ_GAP as usize - 1) * (FRAGMENT_PAYLOAD + BUFFER_ENTRY_COST) <= MAX_BUFFERED_BYTES
);
// Бюджет приёма проверяется на этапе компиляции: худший случай двух каналов укладывается в буфер.
/// Лимит отправки на пира: байт в секунду и размер всплеска.
const BYTES_PER_SEC: f64 = 256.0 * 1024.0;
const BURST_BYTES: f64 = 64.0 * 1024.0;
const INITIAL_RTO: Duration = Duration::from_millis(300);
const MIN_RTO: Duration = Duration::from_millis(50);
const MAX_RTO: Duration = Duration::from_secs(2);
/// Потолок показателя экспоненциального роста RTO при подряд идущих таймаутах.
const MAX_RTO_BACKOFF: u32 = 6;
/// Пакет считается потерянным, если три более новых пакета уже подтверждены.
const REORDER_THRESHOLD: u64 = 3;
/// Маска подтверждений охватывает 64 предыдущих пакета.
const ACK_BITS: u64 = 64;

/// Кадр канала: один фрагмент сообщения.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Frame {
    pub channel: Channel,
    pub seq: u32,
    pub index: u16,
    pub count: u16,
    pub payload: Vec<u8>,
}

impl Frame {
    pub fn encode(&self, out: &mut Vec<u8>) {
        out.push(self.channel as u8);
        out.extend_from_slice(&self.seq.to_be_bytes());
        out.extend_from_slice(&self.index.to_be_bytes());
        out.extend_from_slice(&self.count.to_be_bytes());
        out.extend_from_slice(&(self.payload.len() as u16).to_be_bytes());
        out.extend_from_slice(&self.payload);
    }

    pub fn decode(reader: &mut Reader<'_>) -> Result<Self, &'static str> {
        let channel = reader
            .u8()
            .and_then(Channel::from_u8)
            .ok_or("неизвестный канал")?;
        let seq = reader.u32().ok_or("усечённый кадр")?;
        let index = reader.u16().ok_or("усечённый кадр")?;
        let count = reader.u16().ok_or("усечённый кадр")?;
        let len = usize::from(reader.u16().ok_or("усечённый кадр")?);
        if len > FRAGMENT_PAYLOAD {
            return Err("кадр больше допустимого");
        }
        let payload = reader.take(len).ok_or("усечённый кадр")?.to_vec();
        if count == 0 || index >= count {
            return Err("неверный номер фрагмента");
        }
        if !channel.is_reliable() && (count != 1 || index != 0) {
            return Err("ненадёжный канал передан фрагментами");
        }
        Ok(Self {
            channel,
            seq,
            index,
            count,
            payload,
        })
    }
}

struct InFlight {
    sent_at: Instant,
    frames: Vec<Frame>,
}

/// Входящий поток одного канала.
#[derive(Default)]
struct Inbound {
    /// Следующий ожидаемый номер надёжного кадра.
    expected: u32,
    pending: BTreeMap<u32, Frame>,
    message: Vec<u8>,
    next_index: u16,
    count: u16,
    /// Последний номер для канала `State`: старые кадры отбрасываются.
    latest: Option<u32>,
}

/// Состояние надёжности одного соединения.
pub(crate) struct Reliability {
    queues: [VecDeque<Frame>; CHANNELS],
    /// Байты надёжных кадров в очередях.
    queued_reliable: usize,
    /// Байты ненадёжных кадров в очередях; отдельный бюджет не даёт им вытеснить надёжные.
    queued_soft: usize,
    next_seq: [u32; CHANNELS],
    inflight: BTreeMap<u64, InFlight>,
    largest_received: Option<u64>,
    received_bits: u64,
    ack_needed: bool,
    srtt: Option<Duration>,
    rttvar: Duration,
    /// Число подряд идущих таймаутов без подтверждений: множитель RTO как степень двойки.
    rto_backoff: u32,
    inbound: [Inbound; CHANNELS],
    buffered_bytes: usize,
    tokens: f64,
    refilled_at: Instant,
}

impl Reliability {
    pub(crate) fn new(now: Instant) -> Self {
        Self {
            queues: std::array::from_fn(|_| VecDeque::new()),
            queued_reliable: 0,
            queued_soft: 0,
            next_seq: [0; CHANNELS],
            inflight: BTreeMap::new(),
            largest_received: None,
            received_bits: 0,
            ack_needed: false,
            srtt: None,
            rttvar: Duration::ZERO,
            rto_backoff: 0,
            inbound: std::array::from_fn(|_| Inbound::default()),
            buffered_bytes: 0,
            tokens: BURST_BYTES,
            refilled_at: now,
        }
    }

    /// Ставит сообщение в очередь канала, разбивая его на фрагменты.
    /// Надёжное сообщение при переполнении очереди отклоняется (`QueueFull`), ненадёжное
    /// вытесняет самые старые ненадёжные кадры и никогда не отказывает.
    pub(crate) fn enqueue(&mut self, channel: Channel, bytes: &[u8]) -> Result<(), NetError> {
        check_message_size(channel, bytes.len())?;
        if channel.is_reliable() {
            if self.queued_reliable + bytes.len() > MAX_QUEUED_BYTES {
                return Err(NetError::QueueFull);
            }
        } else {
            while self.queued_soft + bytes.len() > MAX_SOFT_QUEUED_BYTES {
                if !self.evict_soft() {
                    break;
                }
            }
        }
        let parts: Vec<&[u8]> = if bytes.is_empty() {
            vec![bytes]
        } else {
            bytes.chunks(FRAGMENT_PAYLOAD).collect()
        };
        let count = parts.len() as u16;
        let slot = channel.index();
        for (position, part) in parts.into_iter().enumerate() {
            let seq = self.next_seq[slot];
            self.next_seq[slot] = seq.wrapping_add(1);
            self.queues[slot].push_back(Frame {
                channel,
                seq,
                index: position as u16,
                count,
                payload: part.to_vec(),
            });
            *self.queued_counter(channel) += part.len();
        }
        Ok(())
    }

    /// Удаляет самый старый ненадёжный кадр из очередей. Сначала состояния, потом ненадёжные.
    fn evict_soft(&mut self) -> bool {
        for slot in [Channel::State.index(), Channel::Unreliable.index()] {
            if let Some(frame) = self.queues[slot].pop_front() {
                self.queued_soft -= frame.payload.len();
                return true;
            }
        }
        false
    }

    fn queued_counter(&mut self, channel: Channel) -> &mut usize {
        if channel.is_reliable() {
            &mut self.queued_reliable
        } else {
            &mut self.queued_soft
        }
    }

    /// Можно ли отправить ещё один пакет с надёжными кадрами: ограничено число пакетов в полёте.
    pub(crate) fn can_send(&self) -> bool {
        self.inflight.len() < MAX_INFLIGHT
    }

    /// Забирает кадры в порядке приоритета каналов, не больше `room` байт.
    /// Надёжный кадр не уходит дальше `MAX_SEQ_GAP` от старейшего неподтверждённого кадра своего канала.
    pub(crate) fn take_frames(&mut self, room: usize) -> Vec<Frame> {
        let mut oldest = self.oldest_unacked();
        let mut taken = Vec::new();
        let mut used = 0;
        // Освобождённые байты по группам: надёжные и ненадёжные.
        let mut dequeued = [0usize; 2];
        for (slot, queue) in self.queues.iter_mut().enumerate() {
            while let Some(frame) = queue.front() {
                let size = FRAME_HEADER + frame.payload.len();
                if used + size > room
                    || (frame.channel.is_reliable() && beyond_gap(frame.seq, oldest[slot]))
                {
                    break;
                }
                let Some(frame) = queue.pop_front() else {
                    break;
                };
                used += size;
                dequeued[usize::from(!frame.channel.is_reliable())] += frame.payload.len();
                // Отправленный надёжный кадр станет неподтверждённым. База отставания — самый старый неподтверждённый
                // кадр канала, в том числе переотправленный головной кадр, поэтому берётся минимум.
                if frame.channel.is_reliable() {
                    oldest[slot] =
                        Some(oldest[slot].map_or(frame.seq, |current| older(current, frame.seq)));
                }
                taken.push(frame);
            }
        }
        self.queued_reliable -= dequeued[0];
        self.queued_soft -= dequeued[1];
        taken
    }

    /// Старейший неподтверждённый надёжный кадр каждого канала: по нему ограничивается отставание.
    fn oldest_unacked(&self) -> [Option<u32>; CHANNELS] {
        let mut oldest: [Option<u32>; CHANNELS] = [None; CHANNELS];
        for flight in self.inflight.values() {
            for frame in &flight.frames {
                let slot = &mut oldest[frame.channel.index()];
                if slot.is_none_or(|current| (frame.seq.wrapping_sub(current) as i32) < 0) {
                    *slot = Some(frame.seq);
                }
            }
        }
        oldest
    }

    /// Запоминает надёжные кадры отправленного пакета до подтверждения.
    pub(crate) fn record_sent(&mut self, seq: u64, now: Instant, frames: Vec<Frame>) {
        let reliable: Vec<Frame> = frames
            .into_iter()
            .filter(|frame| frame.channel.is_reliable())
            .collect();
        if reliable.is_empty() {
            return;
        }
        self.inflight.insert(
            seq,
            InFlight {
                sent_at: now,
                frames: reliable,
            },
        );
    }

    /// Снимает пакет с учёта полёта и возвращает его кадры.
    fn take_flight(&mut self, seq: u64) -> Option<InFlight> {
        self.inflight.remove(&seq)
    }

    /// Переотправляет кадры пакетов, у которых истёк RTO.
    pub(crate) fn on_timers(&mut self, now: Instant) {
        let rto = self.rto();
        let expired: Vec<u64> = self
            .inflight
            .iter()
            .filter(|(_, flight)| now.saturating_duration_since(flight.sent_at) >= rto)
            .map(|(seq, _)| *seq)
            .collect();
        if !expired.is_empty() {
            self.rto_backoff = (self.rto_backoff + 1).min(MAX_RTO_BACKOFF);
        }
        self.requeue_packets(&expired);
    }

    /// Обрабатывает подтверждение: измеряет RTT и помечает потерянные пакеты.
    pub(crate) fn on_ack(&mut self, now: Instant, largest: u64, bits: u64) {
        let acked: Vec<u64> = self
            .inflight
            .range(..=largest)
            .filter(|(seq, _)| acked_by(largest, bits, **seq))
            .map(|(seq, _)| *seq)
            .collect();
        if !acked.is_empty() {
            // Подтверждение пошло: сеть отвечает, поэтому RTO возвращается к базовому значению.
            self.rto_backoff = 0;
        }
        for seq in acked {
            if let Some(flight) = self.take_flight(seq)
                && seq == largest
            {
                self.sample_rtt(now.saturating_duration_since(flight.sent_at));
            }
        }
        if let Some(limit) = largest.checked_sub(REORDER_THRESHOLD) {
            let lost: Vec<u64> = self.inflight.range(..=limit).map(|(seq, _)| *seq).collect();
            self.requeue_packets(&lost);
        }
    }

    fn requeue_packets(&mut self, packets: &[u64]) {
        let mut lost = Vec::new();
        for seq in packets {
            if let Some(flight) = self.take_flight(*seq) {
                lost.extend(flight.frames);
            }
        }
        lost.sort_by_key(|frame| (frame.channel.index(), frame.seq));
        for frame in lost.into_iter().rev() {
            *self.queued_counter(frame.channel) += frame.payload.len();
            self.queues[frame.channel.index()].push_front(frame);
        }
    }

    fn sample_rtt(&mut self, sample: Duration) {
        match self.srtt {
            None => {
                self.srtt = Some(sample);
                self.rttvar = sample / 2;
            }
            Some(srtt) => {
                let diff = srtt.abs_diff(sample);
                self.rttvar = (self.rttvar * 3 + diff) / 4;
                self.srtt = Some((srtt * 7 + sample) / 8);
            }
        }
    }

    /// Базовый RTO по RTT, умноженный на 2^`rto_backoff` и ограниченный `MAX_RTO`.
    fn rto(&self) -> Duration {
        let base = match self.srtt {
            None => INITIAL_RTO,
            Some(srtt) => (srtt + self.rttvar * 4).clamp(MIN_RTO, MAX_RTO),
        };
        (base * (1u32 << self.rto_backoff)).min(MAX_RTO)
    }

    /// Отмечает принятый пакет. Пакет с кадрами требует подтверждения.
    pub(crate) fn note_received(&mut self, seq: u64, ack_eliciting: bool) {
        match self.largest_received {
            None => {
                self.largest_received = Some(seq);
                self.received_bits = 0;
            }
            Some(largest) if seq > largest => {
                let shift = seq - largest;
                self.received_bits = if shift >= ACK_BITS {
                    0
                } else {
                    self.received_bits << shift
                };
                if shift <= ACK_BITS {
                    self.received_bits |= 1u64 << (shift - 1);
                }
                self.largest_received = Some(seq);
            }
            Some(largest) => {
                let distance = largest - seq;
                if (1..=ACK_BITS).contains(&distance) {
                    self.received_bits |= 1u64 << (distance - 1);
                }
            }
        }
        if ack_eliciting {
            self.ack_needed = true;
        }
    }

    /// Текущее подтверждение: наибольший принятый номер и маска предыдущих.
    pub(crate) fn ack_fields(&self) -> Option<(u64, u64)> {
        self.largest_received
            .map(|largest| (largest, self.received_bits))
    }

    pub(crate) fn ack_needed(&self) -> bool {
        self.ack_needed
    }

    pub(crate) fn clear_ack_needed(&mut self) {
        self.ack_needed = false;
    }

    /// Принимает кадр и добавляет в `out` готовые сообщения канала в порядке доставки.
    pub(crate) fn receive(
        &mut self,
        frame: Frame,
        out: &mut Vec<(Channel, Vec<u8>)>,
    ) -> Result<(), &'static str> {
        match frame.channel {
            Channel::Reliable | Channel::Rpc => self.receive_ordered(frame, out),
            Channel::State => {
                let inbound = &mut self.inbound[frame.channel.index()];
                if inbound
                    .latest
                    .is_some_and(|latest| !is_newer(frame.seq, latest))
                {
                    return Ok(());
                }
                inbound.latest = Some(frame.seq);
                out.push((frame.channel, frame.payload));
                Ok(())
            }
            Channel::Unreliable => {
                out.push((frame.channel, frame.payload));
                Ok(())
            }
        }
    }

    fn receive_ordered(
        &mut self,
        frame: Frame,
        out: &mut Vec<(Channel, Vec<u8>)>,
    ) -> Result<(), &'static str> {
        let slot = frame.channel.index();
        let expected = self.inbound[slot].expected;
        // Разность с учётом переполнения u32: отрицательная означает уже доставленный кадр.
        let ahead = frame.seq.wrapping_sub(expected) as i32;
        if ahead < 0 {
            return Ok(());
        }
        if ahead as u32 >= REORDER_WINDOW {
            return Err("переполнено окно приёма");
        }
        if self.inbound[slot].pending.contains_key(&frame.seq) {
            return Ok(());
        }
        let cost = frame.payload.len() + BUFFER_ENTRY_COST;
        if self.buffered_bytes + cost > MAX_BUFFERED_BYTES {
            return Err("переполнен буфер приёма");
        }
        self.buffered_bytes += cost;
        self.inbound[slot].pending.insert(frame.seq, frame);

        loop {
            let inbound = &mut self.inbound[slot];
            let Some(next) = inbound.pending.remove(&inbound.expected) else {
                return Ok(());
            };
            inbound.expected = inbound.expected.wrapping_add(1);
            self.buffered_bytes -= next.payload.len() + BUFFER_ENTRY_COST;
            assemble(inbound, next, out)?;
        }
    }

    /// Пополняет лимит отправки по времени.
    pub(crate) fn refill(&mut self, now: Instant) {
        let elapsed = now
            .saturating_duration_since(self.refilled_at)
            .as_secs_f64();
        self.tokens = (self.tokens + elapsed * BYTES_PER_SEC).min(BURST_BYTES);
        self.refilled_at = now;
    }

    /// Сколько байт можно отправить сейчас.
    pub(crate) fn tokens(&self) -> usize {
        self.tokens.max(0.0) as usize
    }

    pub(crate) fn spend(&mut self, bytes: usize) {
        self.tokens -= bytes as f64;
    }
}

/// Опережает ли надёжный кадр `seq` старейший неподтверждённый кадр `oldest` больше допустимого.
/// Кадры старее `oldest` (requeue потерянных) разрешены: их и нужно переотправить.
fn beyond_gap(seq: u32, oldest: Option<u32>) -> bool {
    oldest.is_some_and(|oldest| (seq.wrapping_sub(oldest) as i32) >= MAX_SEQ_GAP)
}

/// Более старый из двух номеров кадров канала с учётом переполнения u32.
fn older(a: u32, b: u32) -> u32 {
    if (a.wrapping_sub(b) as i32) < 0 { a } else { b }
}

fn acked_by(largest: u64, bits: u64, seq: u64) -> bool {
    let distance = largest - seq;
    distance == 0 || (distance <= ACK_BITS && bits & (1u64 << (distance - 1)) != 0)
}

/// Номера кадров канала `State` сравниваются с учётом переполнения u32.
fn is_newer(seq: u32, latest: u32) -> bool {
    seq.wrapping_sub(latest) as i32 > 0
}

/// Собирает фрагменты в сообщение. Фрагменты приходят строго по порядку.
fn assemble(
    inbound: &mut Inbound,
    frame: Frame,
    out: &mut Vec<(Channel, Vec<u8>)>,
) -> Result<(), &'static str> {
    if frame.index != inbound.next_index {
        return Err("нарушен порядок фрагментов");
    }
    if frame.index == 0 {
        inbound.count = frame.count;
    } else if frame.count != inbound.count {
        return Err("нарушено число фрагментов");
    }
    if inbound.message.len() + frame.payload.len() > MAX_MESSAGE_BYTES {
        return Err("сообщение больше 64 KiB");
    }
    let channel = frame.channel;
    inbound.message.extend_from_slice(&frame.payload);
    inbound.next_index += 1;
    if inbound.next_index == inbound.count {
        inbound.next_index = 0;
        out.push((channel, std::mem::take(&mut inbound.message)));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(channel: Channel, seq: u32, index: u16, count: u16, payload: &[u8]) -> Frame {
        Frame {
            channel,
            seq,
            index,
            count,
            payload: payload.to_vec(),
        }
    }

    #[test]
    fn out_of_order_reliable_frames_are_delivered_in_order_once() {
        let mut rel = Reliability::new(Instant::now());
        let mut out = Vec::new();
        rel.receive(frame(Channel::Reliable, 1, 0, 1, b"second"), &mut out)
            .unwrap();
        assert!(out.is_empty(), "ждём кадр 0");
        rel.receive(frame(Channel::Reliable, 0, 0, 1, b"first"), &mut out)
            .unwrap();
        rel.receive(frame(Channel::Reliable, 0, 0, 1, b"first"), &mut out)
            .unwrap();
        assert_eq!(
            out,
            vec![
                (Channel::Reliable, b"first".to_vec()),
                (Channel::Reliable, b"second".to_vec())
            ]
        );
    }

    /// Регрессия: при потере одного пакета за ним копятся тысячи мелких кадров. Это не переполнение окна.
    #[test]
    fn many_small_frames_behind_a_missing_frame_are_buffered() {
        let mut rel = Reliability::new(Instant::now());
        let mut out = Vec::new();
        for seq in 1..=2000u32 {
            rel.receive(frame(Channel::Reliable, seq, 0, 1, b"m"), &mut out)
                .unwrap();
        }
        assert!(out.is_empty(), "ждём пропавший кадр 0");
        rel.receive(frame(Channel::Reliable, 0, 0, 1, b"m"), &mut out)
            .unwrap();
        assert_eq!(out.len(), 2001);
    }

    #[test]
    fn buffered_bytes_limit_is_a_protocol_error() {
        let mut rel = Reliability::new(Instant::now());
        let payload = vec![0u8; FRAGMENT_PAYLOAD];
        let mut out = Vec::new();
        let mut result = Ok(());
        let mut accepted = 0u32;
        for seq in 1..=10_000u32 {
            result = rel.receive(frame(Channel::Reliable, seq, 0, 1, &payload), &mut out);
            if result.is_err() {
                break;
            }
            accepted += 1;
        }
        assert_eq!(result, Err("переполнен буфер приёма"));
        assert!(accepted < 2_200, "лимит по байтам: {accepted} кадров");
    }

    #[test]
    fn fragments_are_reassembled_and_limited() {
        let mut rel = Reliability::new(Instant::now());
        let mut out = Vec::new();
        rel.receive(frame(Channel::Rpc, 0, 0, 2, b"ab"), &mut out)
            .unwrap();
        rel.receive(frame(Channel::Rpc, 1, 1, 2, b"cd"), &mut out)
            .unwrap();
        assert_eq!(out, vec![(Channel::Rpc, b"abcd".to_vec())]);

        assert!(
            rel.receive(frame(Channel::Rpc, 2, 1, 2, b"x"), &mut Vec::new())
                .is_err(),
            "фрагмент без начала"
        );
    }

    #[test]
    fn fragment_count_mismatch_and_oversized_message_are_protocol_errors() {
        let mut rel = Reliability::new(Instant::now());
        let mut out = Vec::new();
        rel.receive(frame(Channel::Reliable, 0, 0, 2, b"a"), &mut out)
            .unwrap();
        assert!(
            rel.receive(frame(Channel::Reliable, 1, 1, 3, b"b"), &mut out)
                .is_err()
        );

        let mut rel = Reliability::new(Instant::now());
        let big = vec![0u8; FRAGMENT_PAYLOAD];
        let count = (MAX_MESSAGE_BYTES / FRAGMENT_PAYLOAD + 2) as u16;
        let mut result = Ok(());
        for index in 0..count {
            result = rel.receive(
                frame(Channel::Reliable, u32::from(index), index, count, &big),
                &mut out,
            );
            if result.is_err() {
                break;
            }
        }
        assert!(result.is_err(), "сообщение больше 64 KiB");
    }

    #[test]
    fn state_channel_drops_older_frames() {
        let mut rel = Reliability::new(Instant::now());
        let mut out = Vec::new();
        rel.receive(frame(Channel::State, 5, 0, 1, b"new"), &mut out)
            .unwrap();
        rel.receive(frame(Channel::State, 4, 0, 1, b"old"), &mut out)
            .unwrap();
        rel.receive(frame(Channel::State, 6, 0, 1, b"newest"), &mut out)
            .unwrap();
        assert_eq!(
            out,
            vec![
                (Channel::State, b"new".to_vec()),
                (Channel::State, b"newest".to_vec())
            ]
        );
    }

    #[test]
    fn frame_decode_rejects_fragmented_unreliable_frames() {
        let mut bytes = Vec::new();
        frame(Channel::Unreliable, 0, 0, 2, b"x").encode(&mut bytes);
        assert!(Frame::decode(&mut Reader::new(&bytes)).is_err());
        let mut bytes = Vec::new();
        frame(Channel::Reliable, 0, 0, 1, b"x").encode(&mut bytes);
        assert_eq!(
            Frame::decode(&mut Reader::new(&bytes)).unwrap().payload,
            b"x"
        );
    }

    #[test]
    fn ack_bitfield_tracks_gaps_across_shifts() {
        let mut rel = Reliability::new(Instant::now());
        rel.note_received(1, true);
        rel.note_received(3, true);
        assert_eq!(rel.ack_fields(), Some((3, 0b10)), "пакет 1 — бит 1");
        rel.note_received(2, true);
        assert_eq!(rel.ack_fields(), Some((3, 0b11)));
        rel.note_received(100, true);
        assert_eq!(rel.ack_fields(), Some((100, 0)), "старые пакеты вне маски");
    }

    #[test]
    fn lost_frames_are_retransmitted_after_three_newer_acks() {
        let start = Instant::now();
        let mut rel = Reliability::new(start);
        rel.enqueue(Channel::Reliable, b"lost").unwrap();
        let frames = rel.take_frames(usize::MAX);
        rel.record_sent(0, start, frames);
        for seq in 1..=3u64 {
            rel.enqueue(Channel::Unreliable, b"u").unwrap();
            let frames = rel.take_frames(usize::MAX);
            rel.record_sent(seq, start, frames);
        }
        rel.on_ack(start, 3, 0b011);
        let again = rel.take_frames(usize::MAX);
        assert_eq!(again.len(), 1);
        assert_eq!(again[0].payload, b"lost");
        assert_eq!(again[0].seq, 0, "номер кадра сохраняется");
    }

    #[test]
    fn unacknowledged_frames_are_retransmitted_by_timeout_and_rtt_is_sampled() {
        let start = Instant::now();
        let mut rel = Reliability::new(start);
        rel.enqueue(Channel::Rpc, b"rpc").unwrap();
        let frames = rel.take_frames(usize::MAX);
        rel.record_sent(0, start, frames);

        rel.on_timers(start + Duration::from_millis(100));
        assert!(rel.take_frames(usize::MAX).is_empty(), "RTO ещё не истёк");

        rel.on_timers(start + INITIAL_RTO);
        assert_eq!(
            rel.take_frames(usize::MAX).len(),
            1,
            "переотправка по таймауту"
        );

        let mut rel = Reliability::new(start);
        rel.enqueue(Channel::Reliable, b"x").unwrap();
        let frames = rel.take_frames(usize::MAX);
        rel.record_sent(0, start, frames);
        rel.on_ack(start + Duration::from_millis(40), 0, 0);
        assert_eq!(
            rel.rto(),
            (Duration::from_millis(40) + Duration::from_millis(20) * 4).clamp(MIN_RTO, MAX_RTO)
        );
    }

    #[test]
    fn reliable_queue_is_bounded_and_reports_queue_full() {
        let mut rel = Reliability::new(Instant::now());
        let chunk = vec![0u8; FRAGMENT_PAYLOAD];
        let mut accepted = 0;
        while rel.enqueue(Channel::Reliable, &chunk).is_ok() {
            accepted += 1;
        }
        assert_eq!(accepted, MAX_QUEUED_BYTES / FRAGMENT_PAYLOAD);
        assert_eq!(
            rel.enqueue(Channel::Reliable, &chunk),
            Err(NetError::QueueFull)
        );
    }

    /// Состояния не отказывают при переполнении: вытесняются самые старые кадры.
    #[test]
    fn state_queue_evicts_stale_frames_instead_of_refusing() {
        let mut rel = Reliability::new(Instant::now());
        let chunk = vec![0u8; FRAGMENT_PAYLOAD];
        for _ in 0..(MAX_SOFT_QUEUED_BYTES / FRAGMENT_PAYLOAD) * 2 {
            rel.enqueue(Channel::State, &chunk)
                .expect("состояния не отказывают");
        }
        assert!(rel.queued_soft <= MAX_SOFT_QUEUED_BYTES);
        let frames = rel.take_frames(usize::MAX);
        assert!(frames.first().is_some_and(|frame| frame.seq > 0));
    }

    /// Заполненная очередь состояний не мешает надёжным сообщениям.
    #[test]
    fn soft_flood_leaves_the_reliable_budget_untouched() {
        let mut rel = Reliability::new(Instant::now());
        let chunk = vec![0u8; FRAGMENT_PAYLOAD];
        for _ in 0..1_000 {
            rel.enqueue(Channel::State, &chunk).unwrap();
        }
        assert!(rel.enqueue(Channel::Reliable, b"control").is_ok());
        assert_eq!(rel.queued_reliable, b"control".len());
    }

    /// Регрессия: сравнение номеров через переполнение u32 (после 2^32 кадров).
    #[test]
    fn wrapped_sequence_numbers_keep_frames_in_order() {
        let mut rel = Reliability::new(Instant::now());
        rel.inbound[Channel::Reliable.index()].expected = u32::MAX - 1;
        let mut out = Vec::new();
        rel.receive(frame(Channel::Reliable, u32::MAX - 1, 0, 1, b"a"), &mut out)
            .unwrap();
        // Кадр 0 опережает MAX, поэтому ждёт кадр MAX и не теряется.
        rel.receive(frame(Channel::Reliable, 0, 0, 1, b"c"), &mut out)
            .unwrap();
        assert_eq!(out.len(), 1, "кадр 0 ждёт кадр MAX");
        rel.receive(frame(Channel::Reliable, u32::MAX, 0, 1, b"b"), &mut out)
            .unwrap();
        assert_eq!(
            out,
            vec![
                (Channel::Reliable, b"a".to_vec()),
                (Channel::Reliable, b"b".to_vec()),
                (Channel::Reliable, b"c".to_vec()),
            ]
        );
    }

    /// Отправитель не уходит дальше `MAX_SEQ_GAP` от старейшего неподтверждённого кадра.
    #[test]
    fn sender_stops_far_ahead_of_the_oldest_unacknowledged_frame() {
        let start = Instant::now();
        let mut rel = Reliability::new(start);
        for _ in 0..4_000 {
            rel.enqueue(Channel::Reliable, &[0u8; 100]).unwrap();
        }
        let mut max_seq = 0;
        for packet in 0..200u64 {
            if !rel.can_send() {
                break;
            }
            let frames = rel.take_frames(usize::MAX);
            for frame in &frames {
                max_seq = max_seq.max(frame.seq);
            }
            rel.record_sent(packet, start, frames);
        }
        assert!(
            max_seq < MAX_SEQ_GAP as u32,
            "кадры не уходят дальше {MAX_SEQ_GAP} от неподтверждённого"
        );
    }

    /// Место под кадры одной датаграммы: не больше 1200 байт за вычетом служебных полей пакета
    /// (см. `link::FRAMES_OVERHEAD`).
    const DATAGRAM_ROOM: usize = 1_100;

    /// Надёжные кадры в полёте ограничены числом пакетов; каждый пакет укладывается в датаграмму,
    /// поэтому объём в полёте не превышает `MAX_INFLIGHT * DATAGRAM_ROOM`.
    #[test]
    fn inflight_packets_are_bounded_and_each_fits_one_datagram() {
        let start = Instant::now();
        let mut rel = Reliability::new(start);
        for _ in 0..300 {
            rel.enqueue(Channel::Reliable, &[0u8; FRAGMENT_PAYLOAD])
                .unwrap();
        }
        let mut packets = 0u64;
        let mut inflight_bytes = 0;
        while rel.can_send() {
            let frames = rel.take_frames(DATAGRAM_ROOM);
            let size: usize = frames
                .iter()
                .map(|frame| FRAME_HEADER + frame.payload.len())
                .sum();
            assert!(size <= DATAGRAM_ROOM, "пакет помещается в датаграмму");
            inflight_bytes += size;
            rel.record_sent(packets, start, frames);
            packets += 1;
        }
        assert_eq!(
            packets, MAX_INFLIGHT as u64,
            "число пакетов в полёте ограничено"
        );
        assert!(inflight_bytes <= MAX_INFLIGHT * DATAGRAM_ROOM);
    }

    /// Регрессия: переотправка головного кадра понижает базу отставания. Раньше база оставалась на старом
    /// значении, и отправитель уходил на `MAX_SEQ_GAP` дальше потерянного кадра.
    #[test]
    fn requeued_head_frame_lowers_the_gap_base() {
        let start = Instant::now();
        let mut rel = Reliability::new(start);
        for _ in 0..2_000 {
            rel.enqueue(Channel::Reliable, &[0u8; 8]).unwrap();
        }
        // Кадр `n` уходит в пакете `n`, пакеты 1..=4 подтверждены, пакет 0 потерян и переотправляется.
        for packet in 0..=10u64 {
            let frames = rel.take_frames(FRAME_HEADER + 8);
            assert_eq!(frames.len(), 1);
            rel.record_sent(packet, start, frames);
        }
        rel.on_ack(start, 4, 0b111);
        let frames = rel.take_frames(usize::MAX);
        let max_seq = frames
            .iter()
            .filter(|frame| frame.channel == Channel::Reliable)
            .map(|frame| frame.seq)
            .max()
            .unwrap();
        assert!(
            frames.iter().any(|frame| frame.seq == 0),
            "головной кадр переотправлен"
        );
        assert!(
            max_seq < MAX_SEQ_GAP as u32,
            "отправитель не уходит дальше {MAX_SEQ_GAP} от потерянного кадра"
        );
    }

    /// RTO растёт при подряд идущих таймаутах и возвращается к базовому значению после подтверждения.
    #[test]
    fn rto_doubles_on_consecutive_timeouts_and_resets_on_ack() {
        let start = Instant::now();
        let mut rel = Reliability::new(start);
        rel.enqueue(Channel::Rpc, b"rpc").unwrap();
        let frames = rel.take_frames(usize::MAX);
        rel.record_sent(0, start, frames);
        rel.on_timers(start + INITIAL_RTO);
        assert_eq!(rel.rto(), INITIAL_RTO * 2);

        let later = start + INITIAL_RTO;
        let frames = rel.take_frames(usize::MAX);
        rel.record_sent(1, later, frames);
        rel.on_timers(later + INITIAL_RTO * 2);
        assert_eq!(rel.rto(), INITIAL_RTO * 4);

        let retry_at = later + INITIAL_RTO * 2;
        let frames = rel.take_frames(usize::MAX);
        rel.record_sent(2, retry_at, frames);
        rel.on_ack(retry_at + Duration::from_millis(10), 2, 0);
        assert_eq!(rel.rto_backoff, 0);
    }

    /// Отправитель и приёмник соединены в памяти. Головные кадры каналов `lost` теряются вместе с пакетами,
    /// в которых идут, до истечения 20 с. Возвращает номера доставленных сообщений по каждому каналу.
    /// Приёмник не должен закрываться ни с какой ошибкой: честный пир обязан выдержать такую потерю.
    fn long_loss_of_heads(
        channels: &[Channel],
        lost: &[Channel],
        per_channel: u32,
    ) -> Vec<Vec<u32>> {
        let start = Instant::now();
        let lost_until = start + Duration::from_secs(20);
        let mut sender = Reliability::new(start);
        let mut receiver = Reliability::new(start);
        let mut queued = vec![0u32; channels.len()];
        let mut delivered: Vec<Vec<u32>> = vec![Vec::new(); channels.len()];
        let mut packet = 0u64;
        let mut now = start;
        let done = |delivered: &[Vec<u32>]| {
            delivered
                .iter()
                .all(|numbers| numbers.len() >= per_channel as usize)
        };
        while !done(&delivered) && now < start + Duration::from_secs(600) {
            // Сообщения ставятся по очереди в каждый канал, пока его очередь принимает их.
            loop {
                let mut progressed = false;
                for (index, channel) in channels.iter().enumerate() {
                    if queued[index] >= per_channel {
                        continue;
                    }
                    let mut message = vec![0u8; FRAGMENT_PAYLOAD];
                    message[..4].copy_from_slice(&queued[index].to_be_bytes());
                    if sender.enqueue(*channel, &message).is_ok() {
                        queued[index] += 1;
                        progressed = true;
                    }
                }
                if !progressed {
                    break;
                }
            }
            sender.on_timers(now);
            sender.refill(now);
            while sender.can_send() {
                let frames = sender.take_frames(sender.tokens().min(DATAGRAM_ROOM));
                if frames.is_empty() {
                    break;
                }
                let size: usize = frames
                    .iter()
                    .map(|frame| FRAME_HEADER + frame.payload.len())
                    .sum();
                sender.spend(size);
                let lost_now = now < lost_until
                    && frames
                        .iter()
                        .any(|frame| lost.contains(&frame.channel) && frame.seq == 0);
                sender.record_sent(packet, now, frames.clone());
                if !lost_now {
                    receiver.note_received(packet, true);
                    for frame in frames {
                        let mut out = Vec::new();
                        receiver
                            .receive(frame, &mut out)
                            .expect("честный пир не закрыт");
                        for (channel, bytes) in out {
                            let index = channels.iter().position(|c| *c == channel).unwrap();
                            delivered[index]
                                .push(u32::from_be_bytes(bytes[..4].try_into().unwrap()));
                        }
                    }
                }
                packet += 1;
            }
            if let Some((largest, bits)) = receiver.ack_fields() {
                sender.on_ack(now, largest, bits);
            }
            now += Duration::from_millis(10);
        }
        delivered
    }

    /// Регрессия: долгая потеря одного кадра при большом потоке не переполняет буфер честного пира.
    #[test]
    fn long_loss_of_one_frame_does_not_overflow_the_receiver() {
        let delivered = long_loss_of_heads(&[Channel::Reliable], &[Channel::Reliable], 4_000);
        assert_eq!(delivered[0], (0..4_000).collect::<Vec<_>>());
    }

    /// Регрессия: головные кадры обоих надёжных каналов теряются одновременно. Два канала с окном в 1024 кадра
    /// занимали больше `MAX_BUFFERED_BYTES`, и приёмник закрывал честного пира с ошибкой переполнения буфера.
    #[test]
    fn long_loss_of_both_reliable_heads_does_not_overflow_the_receiver() {
        let channels = [Channel::Reliable, Channel::Rpc];
        let delivered = long_loss_of_heads(&channels, &channels, 2_000);
        for numbers in &delivered {
            assert_eq!(numbers, &(0..2_000).collect::<Vec<_>>());
        }
    }

    #[test]
    fn unreliable_messages_larger_than_one_frame_are_rejected() {
        let mut rel = Reliability::new(Instant::now());
        let big = vec![0u8; FRAGMENT_PAYLOAD + 1];
        assert!(matches!(
            rel.enqueue(Channel::Unreliable, &big),
            Err(NetError::MessageTooLarge { .. })
        ));
        assert!(rel.enqueue(Channel::Reliable, &big).is_ok());
    }

    #[test]
    fn send_budget_limits_bytes_per_second() {
        let start = Instant::now();
        let mut rel = Reliability::new(start);
        rel.spend(BURST_BYTES as usize);
        assert_eq!(rel.tokens(), 0);
        rel.refill(start + Duration::from_millis(100));
        assert_eq!(rel.tokens(), (BYTES_PER_SEC * 0.1) as usize);
        rel.refill(start + Duration::from_secs(10));
        assert_eq!(rel.tokens(), BURST_BYTES as usize, "не больше всплеска");
    }
}
