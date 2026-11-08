//! Рукопожатие: форматы тел пакетов, cookie и таймеры. Машины состояний — в `server` и `client`.
//!
//! Тела пакетов (после открытого заголовка `packet::Header`):
//! - Hello, 1200 байт: client_public[32] ‖ nonce[16] ‖ protocol_id[32] ‖ scene_hash[32] ‖ нули.
//! - Cookie, 40 байт: time_ms[8] ‖ mac[32], где mac — keyed blake3 от (адрес, time_ms, hash(hello)).
//! - Confirm, 1200 байт: cookie[40] ‖ hello[112] ‖ нули.
//! - Accept, 128 байт: server_public[32] ‖ identity_public[32] ‖ signature[64].
//! - Reject: длина[1] ‖ причина в UTF-8. Не подписан: отправляется только до аутентификации.

use std::net::SocketAddr;
use std::time::Duration;

use crate::codec::Reader;
use crate::crypto::ct_eq;
use crate::packet::{HEADER_LEN, Header, Kind, MAX_DATAGRAM, pad_to_max};

pub(crate) const HELLO_BODY: usize = 112;
pub(crate) const COOKIE_BODY: usize = 40;
pub(crate) const ACCEPT_BODY: usize = 128;
/// Срок жизни cookie: ответ на Confirm позже этого срока не принимается.
const COOKIE_TTL_MS: u64 = 5_000;
/// Срок незавершённого рукопожатия на любой стороне.
pub(crate) const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);
/// Интервал повторной отправки рукопожатных пакетов, которые могли потеряться.
pub(crate) const HANDSHAKE_RESEND: Duration = Duration::from_millis(250);
const MAX_REASON: usize = 255;

/// Тело ClientHello.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Hello {
    pub client_public: [u8; 32],
    pub nonce: [u8; 16],
    pub protocol_id: [u8; 32],
    pub scene_hash: [u8; 32],
}

impl Hello {
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(HELLO_BODY);
        out.extend_from_slice(&self.client_public);
        out.extend_from_slice(&self.nonce);
        out.extend_from_slice(&self.protocol_id);
        out.extend_from_slice(&self.scene_hash);
        out
    }

    pub fn decode(body: &[u8]) -> Option<Self> {
        let mut reader = Reader::new(body);
        Some(Self {
            client_public: reader.array()?,
            nonce: reader.array()?,
            protocol_id: reader.array()?,
            scene_hash: reader.array()?,
        })
    }
}

pub(crate) fn hello_datagram(hello: &Hello) -> Vec<u8> {
    let mut out = Vec::with_capacity(MAX_DATAGRAM);
    Header {
        kind: Kind::Hello,
        conn_id: 0,
    }
    .encode(&mut out);
    out.extend_from_slice(&hello.encode());
    pad_to_max(&mut out);
    out
}

pub(crate) fn cookie_datagram(cookie: &[u8; COOKIE_BODY]) -> Vec<u8> {
    let mut out = Vec::with_capacity(HEADER_LEN + COOKIE_BODY);
    Header {
        kind: Kind::Cookie,
        conn_id: 0,
    }
    .encode(&mut out);
    out.extend_from_slice(cookie);
    out
}

pub(crate) fn confirm_datagram(cookie: &[u8; COOKIE_BODY], hello: &Hello) -> Vec<u8> {
    let mut out = Vec::with_capacity(MAX_DATAGRAM);
    Header {
        kind: Kind::Confirm,
        conn_id: 0,
    }
    .encode(&mut out);
    out.extend_from_slice(cookie);
    out.extend_from_slice(&hello.encode());
    pad_to_max(&mut out);
    out
}

pub(crate) fn accept_datagram(
    conn_id: u32,
    server_public: &[u8; 32],
    identity_public: &[u8; 32],
    signature: &[u8; 64],
) -> Vec<u8> {
    let mut out = Vec::with_capacity(HEADER_LEN + ACCEPT_BODY);
    Header {
        kind: Kind::Accept,
        conn_id,
    }
    .encode(&mut out);
    out.extend_from_slice(server_public);
    out.extend_from_slice(identity_public);
    out.extend_from_slice(signature);
    out
}

pub(crate) fn parse_accept(body: &[u8]) -> Option<([u8; 32], [u8; 32], [u8; 64])> {
    if body.len() != ACCEPT_BODY {
        return None;
    }
    let mut reader = Reader::new(body);
    Some((reader.array()?, reader.array()?, reader.array()?))
}

pub(crate) fn parse_confirm(body: &[u8]) -> Option<([u8; COOKIE_BODY], Hello)> {
    let mut reader = Reader::new(body);
    let cookie: [u8; COOKIE_BODY] = reader.array()?;
    let hello = Hello::decode(reader.take(HELLO_BODY)?)?;
    Some((cookie, hello))
}

pub(crate) fn reject_datagram(reason: &str) -> Vec<u8> {
    let bytes = &reason.as_bytes()[..reason.len().min(MAX_REASON)];
    let mut out = Vec::with_capacity(HEADER_LEN + 1 + bytes.len());
    Header {
        kind: Kind::Reject,
        conn_id: 0,
    }
    .encode(&mut out);
    out.push(bytes.len() as u8);
    out.extend_from_slice(bytes);
    out
}

pub(crate) fn parse_reject(body: &[u8]) -> String {
    let mut reader = Reader::new(body);
    let bytes = reader.u8().and_then(|len| reader.take(usize::from(len)));
    match bytes {
        Some(bytes) => String::from_utf8_lossy(bytes).into_owned(),
        None => "сервер отказал в подключении".to_owned(),
    }
}

fn cookie_mac(secret: &[u8; 32], addr: SocketAddr, time_ms: u64, hello_body: &[u8]) -> [u8; 32] {
    let mut hasher = blake3::Hasher::new_keyed(secret);
    hasher.update(addr.to_string().as_bytes());
    hasher.update(&time_ms.to_be_bytes());
    hasher.update(blake3::hash(hello_body).as_bytes());
    *hasher.finalize().as_bytes()
}

/// Выдаёт cookie для Hello. Состояние на сервере не хранится: cookie проверяется повторным расчётом.
pub(crate) fn make_cookie(
    secret: &[u8; 32],
    addr: SocketAddr,
    now_ms: u64,
    hello: &Hello,
) -> [u8; COOKIE_BODY] {
    let mut cookie = [0u8; COOKIE_BODY];
    cookie[..8].copy_from_slice(&now_ms.to_be_bytes());
    cookie[8..].copy_from_slice(&cookie_mac(secret, addr, now_ms, &hello.encode()));
    cookie
}

/// Проверяет cookie: MAC совпадает, срок не истёк и время не из будущего.
pub(crate) fn cookie_valid(
    secret: &[u8; 32],
    addr: SocketAddr,
    now_ms: u64,
    cookie: &[u8; COOKIE_BODY],
    hello: &Hello,
) -> bool {
    let mut time = [0u8; 8];
    time.copy_from_slice(&cookie[..8]);
    let time_ms = u64::from_be_bytes(time);
    if time_ms > now_ms || now_ms - time_ms > COOKIE_TTL_MS {
        return false;
    }
    let expected = cookie_mac(secret, addr, time_ms, &hello.encode());
    ct_eq(&cookie[8..], &expected)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hello() -> Hello {
        Hello {
            client_public: [1; 32],
            nonce: [2; 16],
            protocol_id: [3; 32],
            scene_hash: [4; 32],
        }
    }

    fn addr(port: u16) -> SocketAddr {
        SocketAddr::from(([127, 0, 0, 1], port))
    }

    #[test]
    fn hello_and_confirm_datagrams_are_padded_to_the_maximum() {
        assert_eq!(hello_datagram(&hello()).len(), MAX_DATAGRAM);
        let cookie = make_cookie(&[9; 32], addr(1), 100, &hello());
        assert_eq!(confirm_datagram(&cookie, &hello()).len(), MAX_DATAGRAM);
    }

    #[test]
    fn cookie_is_bound_to_address_time_and_hello() {
        let secret = [9u8; 32];
        let cookie = make_cookie(&secret, addr(1), 1_000, &hello());
        assert!(cookie_valid(&secret, addr(1), 1_000, &cookie, &hello()));
        assert!(
            !cookie_valid(&secret, addr(2), 1_000, &cookie, &hello()),
            "чужой адрес"
        );
        let mut other = hello();
        other.nonce = [7; 16];
        assert!(
            !cookie_valid(&secret, addr(1), 1_000, &cookie, &other),
            "другой hello"
        );
        assert!(
            !cookie_valid(&[8; 32], addr(1), 1_000, &cookie, &hello()),
            "другой секрет"
        );
    }

    #[test]
    fn cookie_expires_after_its_ttl_and_rejects_future_times() {
        let secret = [9u8; 32];
        let cookie = make_cookie(&secret, addr(1), 1_000, &hello());
        assert!(cookie_valid(
            &secret,
            addr(1),
            1_000 + COOKIE_TTL_MS,
            &cookie,
            &hello()
        ));
        assert!(!cookie_valid(
            &secret,
            addr(1),
            1_001 + COOKIE_TTL_MS,
            &cookie,
            &hello()
        ));
        assert!(
            !cookie_valid(&secret, addr(1), 999, &cookie, &hello()),
            "из будущего"
        );
    }

    #[test]
    fn tampered_cookie_mac_is_rejected() {
        let secret = [9u8; 32];
        let mut cookie = make_cookie(&secret, addr(1), 1_000, &hello());
        cookie[39] ^= 1;
        assert!(!cookie_valid(&secret, addr(1), 1_000, &cookie, &hello()));
    }

    #[test]
    fn reject_reason_round_trips_and_truncated_body_has_default_text() {
        let datagram = reject_datagram("протокол не совпадает");
        let body = &datagram[HEADER_LEN..];
        assert_eq!(parse_reject(body), "протокол не совпадает");
        assert_eq!(parse_reject(&[]), "сервер отказал в подключении");
        assert_eq!(parse_reject(&[9, b'a']), "сервер отказал в подключении");
    }

    #[test]
    fn accept_body_must_have_exact_length() {
        assert!(parse_accept(&[0u8; ACCEPT_BODY - 1]).is_none());
        assert!(parse_accept(&[0u8; ACCEPT_BODY]).is_some());
    }
}
