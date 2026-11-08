//! Соединение после рукопожатия: шифрование датаграмм, кадры, подтверждения, keepalive и таймаут.
//!
//! Тело зашифрованного пакета начинается с байта операции:
//! - `OP_AUTH` (клиент→сервер): proof[32], первый пакет после рукопожатия;
//! - `OP_WELCOME` (сервер→клиент): peer_id[4], ответ на подтверждённый Auth;
//! - `OP_FRAMES`: флаги[1] (бит 0 — есть подтверждение), largest[8], bits[8], число кадров[1], кадры;
//! - `OP_DISCONNECT`: длина[2] ‖ причина в UTF-8.

use std::net::SocketAddr;
use std::time::{Duration, Instant};

use engine_scene::network::PeerId;

use crate::codec::Reader;
use crate::crypto::Session;
use crate::packet::{HEADER_LEN, MAX_DATAGRAM, SEQ_LEN, TAG_LEN};
use crate::reliable::{Frame, Reliability};
use crate::{Channel, NetError, NetworkEvent, Output};

pub(crate) const OP_AUTH: u8 = 1;
pub(crate) const OP_WELCOME: u8 = 2;
pub(crate) const OP_FRAMES: u8 = 3;
pub(crate) const OP_DISCONNECT: u8 = 4;

/// Интервал пустого пакета-подтверждения, если данных нет: держит соединение живым.
const KEEPALIVE: Duration = Duration::from_millis(500);
const MAX_REASON: usize = 256;
/// Всё, что занимает место в датаграмме до кадров: заголовок, номер, тег, операция, флаги, подтверждение, число кадров.
const FRAMES_OVERHEAD: usize = HEADER_LEN + SEQ_LEN + TAG_LEN + 1 + 1 + 8 + 8 + 1;
const TRUNCATED: &str = "усечённый пакет";
/// Тело Auth фиксированной длины: операция, доказательство пароля и заполнение нулями.
/// Длина не меньше наибольшего отказа, поэтому сервер не отвечает на короткий запрос длиннее него.
pub(crate) const AUTH_BODY: usize = 300;
/// Причина отключения, когда очередь надёжных сообщений одного пира переполнена.
pub(crate) const QUEUE_OVERFLOW: &str = "очередь отправки переполнена";

/// Соединение с одним пиром: сессия, надёжность и таймеры.
pub(crate) struct Link {
    pub peer: PeerId,
    pub addr: SocketAddr,
    conn_id: u32,
    session: Session,
    rel: Reliability,
    last_recv: Instant,
    last_send: Instant,
    timeout: Duration,
    /// Причина, по которой соединение нужно закрыть (переполнена очередь надёжных сообщений).
    fault: Option<&'static str>,
}

impl Link {
    pub(crate) fn new(
        peer: PeerId,
        addr: SocketAddr,
        conn_id: u32,
        session: Session,
        now: Instant,
        timeout: Duration,
    ) -> Self {
        Self {
            peer,
            addr,
            conn_id,
            session,
            rel: Reliability::new(now),
            last_recv: now,
            last_send: now,
            timeout,
            fault: None,
        }
    }

    /// Причина закрытия, если соединение нужно разорвать.
    pub(crate) fn fault(&self) -> Option<&'static str> {
        self.fault
    }

    /// Сдвигает отсчёт таймаута. Вызывается, пока приложение не забирает события.
    pub(crate) fn hold(&mut self, now: Instant) {
        self.last_recv = now;
    }

    /// Шифрует служебное тело (Welcome, Disconnect) без очереди и подтверждений.
    pub(crate) fn seal(&mut self, body: &[u8]) -> Option<Vec<u8>> {
        self.session
            .seal(self.conn_id, body)
            .map(|(_, datagram)| datagram)
    }

    /// Расшифровывает датаграмму; обновляет время последнего приёма только для подлинных пакетов.
    pub(crate) fn open(&mut self, now: Instant, datagram: &[u8]) -> Option<(u64, Vec<u8>)> {
        let opened = self.session.open(datagram)?;
        self.last_recv = now;
        Some(opened)
    }

    pub(crate) fn timed_out(&self, now: Instant) -> bool {
        now.saturating_duration_since(self.last_recv) >= self.timeout
    }

    /// Ставит сообщение в очередь. Переполнение надёжной очереди помечает соединение на закрытие:
    /// молча терять надёжные сообщения нельзя.
    pub(crate) fn enqueue(&mut self, channel: Channel, bytes: &[u8]) -> Result<(), NetError> {
        let result = self.rel.enqueue(channel, bytes);
        if channel.is_reliable() && matches!(result, Err(NetError::QueueFull)) {
            self.fault.get_or_insert(QUEUE_OVERFLOW);
        }
        result
    }

    /// Обрабатывает тело `OP_FRAMES`. Ошибка означает нарушение протокола пиром.
    pub(crate) fn receive_frames(
        &mut self,
        now: Instant,
        seq: u64,
        body: &[u8],
        events: &mut Vec<NetworkEvent>,
    ) -> Result<(), &'static str> {
        let mut reader = Reader::new(body);
        let flags = reader.u8().ok_or(TRUNCATED)?;
        let largest = reader.u64().ok_or(TRUNCATED)?;
        let bits = reader.u64().ok_or(TRUNCATED)?;
        let count = reader.u8().ok_or(TRUNCATED)?;
        if flags & 1 != 0 {
            self.rel.on_ack(now, largest, bits);
        }
        let mut frames = Vec::with_capacity(usize::from(count));
        for _ in 0..count {
            frames.push(Frame::decode(&mut reader)?);
        }
        if reader.remaining() != 0 {
            return Err("лишние байты в пакете");
        }
        self.rel.note_received(seq, !frames.is_empty());
        let mut delivered = Vec::new();
        for frame in frames {
            self.rel.receive(frame, &mut delivered)?;
        }
        events.extend(
            delivered
                .into_iter()
                .map(|(channel, bytes)| NetworkEvent::Message {
                    peer: self.peer,
                    channel,
                    bytes,
                }),
        );
        Ok(())
    }

    /// Отправляет накопленные кадры, подтверждения и keepalive; повторяет просроченные надёжные кадры.
    pub(crate) fn flush(&mut self, now: Instant, out: &mut Output) -> Result<(), &'static str> {
        self.rel.on_timers(now);
        self.rel.refill(now);
        loop {
            let room = self.rel.tokens().min(MAX_DATAGRAM - FRAMES_OVERHEAD);
            let frames = if self.rel.can_send() {
                self.rel.take_frames(room)
            } else {
                Vec::new()
            };
            let keepalive = now.saturating_duration_since(self.last_send) >= KEEPALIVE;
            if frames.is_empty() && !self.rel.ack_needed() && !keepalive {
                return Ok(());
            }
            let body = frames_body(self.rel.ack_fields(), &frames);
            let Some((seq, datagram)) = self.session.seal(self.conn_id, &body) else {
                return Err("номер пакета исчерпан");
            };
            self.rel.spend(datagram.len());
            self.rel.clear_ack_needed();
            self.last_send = now;
            self.rel.record_sent(seq, now, frames);
            out.datagrams.push((self.addr, datagram));
        }
    }
}

/// Тело пакета с кадрами. Число кадров не превышает ~104: `room` ограничивает размер пакета.
fn frames_body(ack: Option<(u64, u64)>, frames: &[Frame]) -> Vec<u8> {
    let (largest, bits) = ack.unwrap_or((0, 0));
    let mut body = Vec::with_capacity(MAX_DATAGRAM);
    body.push(OP_FRAMES);
    body.push(u8::from(ack.is_some()));
    body.extend_from_slice(&largest.to_be_bytes());
    body.extend_from_slice(&bits.to_be_bytes());
    body.push(frames.len() as u8);
    for frame in frames {
        frame.encode(&mut body);
    }
    body
}

pub(crate) fn auth_body(proof: &[u8; 32]) -> Vec<u8> {
    let mut body = vec![OP_AUTH];
    body.extend_from_slice(proof);
    body.resize(AUTH_BODY, 0);
    body
}

pub(crate) fn welcome_body(peer: PeerId) -> Vec<u8> {
    let mut body = vec![OP_WELCOME];
    body.extend_from_slice(&peer.to_be_bytes());
    body
}

/// Разбирает `OP_WELCOME` и возвращает номер участника.
pub(crate) fn parse_welcome(body: &[u8]) -> Option<PeerId> {
    if body.first() != Some(&OP_WELCOME) {
        return None;
    }
    Reader::new(&body[1..]).u32()
}

pub(crate) fn disconnect_body(reason: &str) -> Vec<u8> {
    let bytes = &reason.as_bytes()[..reason.len().min(MAX_REASON)];
    let mut body = vec![OP_DISCONNECT];
    body.extend_from_slice(&(bytes.len() as u16).to_be_bytes());
    body.extend_from_slice(bytes);
    body
}

/// Причина из тела `OP_DISCONNECT` без байта операции.
pub(crate) fn parse_disconnect(body: &[u8]) -> String {
    let mut reader = Reader::new(body);
    let bytes = reader.u16().and_then(|len| reader.take(usize::from(len)));
    match bytes {
        Some(bytes) => String::from_utf8_lossy(bytes).into_owned(),
        None => "соединение закрыто удалённой стороной".to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disconnect_reason_round_trips() {
        let body = disconnect_body("таймаут");
        assert_eq!(body[0], OP_DISCONNECT);
        assert_eq!(parse_disconnect(&body[1..]), "таймаут");
        assert_eq!(
            parse_disconnect(&[]),
            "соединение закрыто удалённой стороной"
        );
    }

    #[test]
    fn welcome_carries_peer_id() {
        assert_eq!(parse_welcome(&welcome_body(7)), Some(7));
        assert_eq!(parse_welcome(&[OP_WELCOME, 0]), None, "усечённое тело");
        assert_eq!(parse_welcome(&auth_body(&[0; 32])), None);
    }

    #[test]
    fn frames_overhead_matches_the_encoded_layout() {
        let body = frames_body(None, &[]);
        assert_eq!(body.len(), 19);
        assert_eq!(HEADER_LEN + SEQ_LEN + TAG_LEN + body.len(), FRAMES_OVERHEAD);
    }

    #[test]
    fn auth_body_has_fixed_length_and_any_reject_fits_in_it() {
        assert_eq!(auth_body(&[7; 32]).len(), AUTH_BODY);
        let longest = "я".repeat(MAX_REASON);
        assert!(disconnect_body(&longest).len() <= AUTH_BODY);
    }

    #[test]
    fn queue_overflow_of_reliable_messages_marks_the_link_for_closing() {
        let mut link = Link::new(
            1,
            "127.0.0.1:9".parse().unwrap(),
            1,
            Session::new(&[1; 32], &[2; 32]),
            Instant::now(),
            Duration::from_secs(1),
        );
        let chunk = vec![0u8; 1000];
        while link.enqueue(Channel::Reliable, &chunk).is_ok() {}
        assert_eq!(link.fault(), Some(QUEUE_OVERFLOW));
    }

    #[test]
    fn overflow_of_unreliable_messages_does_not_close_the_link() {
        let mut link = Link::new(
            1,
            "127.0.0.1:9".parse().unwrap(),
            1,
            Session::new(&[1; 32], &[2; 32]),
            Instant::now(),
            Duration::from_secs(1),
        );
        for _ in 0..1_000 {
            link.enqueue(Channel::State, &[0u8; 1000]).unwrap();
        }
        assert_eq!(link.fault(), None);
    }
}
