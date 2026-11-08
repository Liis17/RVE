//! Клиент: рукопожатие с закреплением отпечатка сервера, затем соединение. Ввода-вывода нет.
//!
//! Пароль и Auth уходят только после проверки подписи сервера и отпечатка. Без закреплённого
//! отпечатка клиент подключается, только если `insecure_no_pin` задан явно.

use std::net::SocketAddr;
use std::time::{Duration, Instant};

use engine_scene::network::{PeerId, SERVER_PEER_ID};

use crate::crypto::{
    EphemeralKey, Password, Session, Transcript, ct_eq, derive_keys, fingerprint_of, password_hash,
    password_proof, random_bytes, transcript_hash, verify_accept,
};
use crate::handshake::{
    ACCEPT_BODY, COOKIE_BODY, HANDSHAKE_RESEND, HANDSHAKE_TIMEOUT, Hello, confirm_datagram,
    hello_datagram, parse_accept, parse_reject,
};
use crate::link::{
    Link, OP_DISCONNECT, OP_FRAMES, auth_body, disconnect_body, parse_disconnect, parse_welcome,
};
use crate::packet::{HEADER_LEN, Header, Kind};
use crate::{Channel, NetError, NetworkEvent, Output};

/// Настройки клиента.
pub struct ClientConfig {
    pub protocol_id: [u8; 32],
    pub scene_hash: [u8; 32],
    pub server: SocketAddr,
    /// Закреплённый отпечаток ключа сервера (blake3 от открытого ключа Ed25519).
    pub fingerprint: Option<[u8; 32]>,
    /// Разрешение подключаться без отпечатка. Без него отсутствие отпечатка — ошибка.
    pub insecure_no_pin: bool,
    pub password: Option<Password>,
    pub timeout: Duration,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Stage {
    /// Ждём cookie на Hello.
    Hello,
    /// Ждём Accept на Confirm.
    Confirm,
    /// Auth отправлен, ждём Welcome.
    Auth,
}

/// Состояние рукопожатия клиента.
struct Handshake {
    stage: Stage,
    ephemeral: EphemeralKey,
    hello: Hello,
    started: Instant,
    /// `None` — пакет ещё не отправлялся.
    sent_at: Option<Instant>,
    cookie: [u8; COOKIE_BODY],
    conn_id: u32,
    session: Option<Session>,
    auth: Vec<u8>,
}

enum State {
    Handshake(Box<Handshake>),
    Connected(Box<Link>),
    Closed,
}

struct Settings {
    server: SocketAddr,
    fingerprint: Option<[u8; 32]>,
    password: Option<Password>,
    timeout: Duration,
}

/// Машина состояний клиента.
pub struct Client {
    settings: Settings,
    state: State,
    local_peer: Option<PeerId>,
}

impl Client {
    pub fn new(config: ClientConfig, now: Instant) -> Result<Self, NetError> {
        if config.fingerprint.is_none() {
            if !config.insecure_no_pin {
                return Err(NetError::Config(
                    "не задан отпечаток сервера; укажите его или разрешите подключение без проверки"
                        .into(),
                ));
            }
            log::warn!(
                "подключение к {} без проверки отпечатка сервера",
                config.server
            );
        }
        let ephemeral = EphemeralKey::random()?;
        let hello = Hello {
            client_public: ephemeral.public(),
            nonce: random_bytes::<16>()?,
            protocol_id: config.protocol_id,
            scene_hash: config.scene_hash,
        };
        Ok(Self {
            settings: Settings {
                server: config.server,
                fingerprint: config.fingerprint,
                password: config.password,
                timeout: config.timeout,
            },
            state: State::Handshake(Box::new(Handshake {
                stage: Stage::Hello,
                ephemeral,
                hello,
                started: now,
                sent_at: None,
                cookie: [0; COOKIE_BODY],
                conn_id: 0,
                session: None,
                auth: Vec::new(),
            })),
            local_peer: None,
        })
    }

    /// Номер участника, который назначил сервер; `None`, пока соединение не установлено.
    pub fn local_peer(&self) -> Option<PeerId> {
        self.local_peer
    }

    pub fn is_connected(&self) -> bool {
        matches!(self.state, State::Connected(_))
    }

    /// Обрабатывает датаграмму. Датаграммы не от сервера игнорируются.
    pub fn handle_datagram(&mut self, from: SocketAddr, bytes: &[u8], now: Instant) -> Output {
        let mut out = Output::default();
        if from != self.settings.server {
            return out;
        }
        let Some(header) = Header::decode(bytes) else {
            return out;
        };
        let state = std::mem::replace(&mut self.state, State::Closed);
        self.state = match state {
            State::Handshake(hs) => self.on_handshake(hs, header, bytes, now, &mut out),
            State::Connected(link) => self.on_connected(link, bytes, now, &mut out),
            State::Closed => State::Closed,
        };
        out
    }

    /// Повторы рукопожатия, таймауты и отправка накопленных кадров.
    pub fn update(&mut self, now: Instant) -> Output {
        let mut out = Output::default();
        let state = std::mem::replace(&mut self.state, State::Closed);
        self.state = match state {
            State::Handshake(hs) => self.resend_or_timeout(hs, now, &mut out),
            State::Connected(mut link) => {
                if link.timed_out(now) {
                    disconnected("таймаут соединения", &mut out);
                    State::Closed
                } else {
                    match link.flush(now, &mut out) {
                        Ok(()) => State::Connected(link),
                        Err(reason) => {
                            disconnected(reason, &mut out);
                            State::Closed
                        }
                    }
                }
            }
            State::Closed => State::Closed,
        };
        out
    }

    /// Ставит сообщение серверу.
    pub fn send(&mut self, channel: Channel, bytes: &[u8]) -> Result<(), NetError> {
        match &mut self.state {
            State::Connected(link) => link.enqueue(channel, bytes),
            _ => Err(NetError::NotConnected),
        }
    }

    /// Отключается от сервера с указанной причиной.
    pub fn disconnect(&mut self, reason: &str) -> Output {
        let mut out = Output::default();
        if let State::Connected(mut link) = std::mem::replace(&mut self.state, State::Closed)
            && let Some(datagram) = link.seal(&disconnect_body(reason))
        {
            out.datagrams.push((self.settings.server, datagram));
        }
        disconnected(reason, &mut out);
        out
    }

    fn resend_or_timeout(&self, mut hs: Box<Handshake>, now: Instant, out: &mut Output) -> State {
        if now.saturating_duration_since(hs.started) >= HANDSHAKE_TIMEOUT {
            disconnected("таймаут подключения", out);
            return State::Closed;
        }
        let due = hs
            .sent_at
            .is_none_or(|sent| now.saturating_duration_since(sent) >= HANDSHAKE_RESEND);
        if due {
            let datagram = match hs.stage {
                Stage::Hello => Some(hello_datagram(&hs.hello)),
                Stage::Confirm => Some(confirm_datagram(&hs.cookie, &hs.hello)),
                Stage::Auth => hs
                    .session
                    .as_mut()
                    .and_then(|session| session.seal(hs.conn_id, &hs.auth))
                    .map(|(_, datagram)| datagram),
            };
            hs.sent_at = Some(now);
            if let Some(datagram) = datagram {
                out.datagrams.push((self.settings.server, datagram));
            }
        }
        State::Handshake(hs)
    }

    fn on_handshake(
        &mut self,
        mut hs: Box<Handshake>,
        header: Header,
        datagram: &[u8],
        now: Instant,
        out: &mut Output,
    ) -> State {
        let body = &datagram[HEADER_LEN..];
        let reply = match (hs.stage, header.kind) {
            (Stage::Hello, Kind::Cookie) => Ok(on_cookie(&mut hs, body, now)),
            (Stage::Confirm, Kind::Accept) => {
                accept_reply(&self.settings, &mut hs, header.conn_id, body, now)
            }
            (Stage::Auth, Kind::Data) => return self.on_welcome(hs, datagram, now, out),
            (_, Kind::Reject) => Err(parse_reject(body)),
            _ => Ok(None),
        };
        match reply {
            Ok(Some(reply)) => {
                out.datagrams.push((self.settings.server, reply));
                State::Handshake(hs)
            }
            Ok(None) => State::Handshake(hs),
            Err(reason) => {
                disconnected(&reason, out);
                State::Closed
            }
        }
    }

    fn on_welcome(
        &mut self,
        mut hs: Box<Handshake>,
        datagram: &[u8],
        now: Instant,
        out: &mut Output,
    ) -> State {
        let Some(session) = hs.session.as_mut() else {
            return State::Handshake(hs);
        };
        let Some((_, body)) = session.open(datagram) else {
            return State::Handshake(hs);
        };
        if let Some(peer) = parse_welcome(&body) {
            self.local_peer = Some(peer);
            let Some(session) = hs.session.take() else {
                return State::Closed;
            };
            let link = Link::new(
                SERVER_PEER_ID,
                self.settings.server,
                hs.conn_id,
                session,
                now,
                self.settings.timeout,
            );
            out.events.push(NetworkEvent::Connected {
                peer: SERVER_PEER_ID,
            });
            return State::Connected(Box::new(link));
        }
        if body.first() == Some(&OP_DISCONNECT) {
            disconnected(&parse_disconnect(&body[1..]), out);
            return State::Closed;
        }
        State::Handshake(hs)
    }

    fn on_connected(
        &mut self,
        mut link: Box<Link>,
        datagram: &[u8],
        now: Instant,
        out: &mut Output,
    ) -> State {
        let Some((seq, body)) = link.open(now, datagram) else {
            return State::Connected(link);
        };
        match body.first().copied() {
            Some(OP_FRAMES) => match link.receive_frames(now, seq, &body[1..], &mut out.events) {
                Ok(()) => State::Connected(link),
                Err(reason) => {
                    if let Some(reply) = link.seal(&disconnect_body(reason)) {
                        out.datagrams.push((self.settings.server, reply));
                    }
                    disconnected(reason, out);
                    State::Closed
                }
            },
            Some(OP_DISCONNECT) => {
                disconnected(&parse_disconnect(&body[1..]), out);
                State::Closed
            }
            _ => State::Connected(link),
        }
    }
}

fn disconnected(reason: &str, out: &mut Output) {
    out.events.push(NetworkEvent::Disconnected {
        peer: SERVER_PEER_ID,
        reason: reason.to_owned(),
    });
}

/// Cookie получен: переходим к Confirm и сразу отправляем его.
fn on_cookie(hs: &mut Handshake, body: &[u8], now: Instant) -> Option<Vec<u8>> {
    if body.len() != COOKIE_BODY || hs.stage != Stage::Hello {
        return None;
    }
    hs.cookie.copy_from_slice(body);
    hs.stage = Stage::Confirm;
    hs.sent_at = Some(now);
    Some(confirm_datagram(&hs.cookie, &hs.hello))
}

/// Accept получен: проверяем отпечаток, подпись и общий секрет, затем отправляем Auth с доказательством пароля.
fn accept_reply(
    settings: &Settings,
    hs: &mut Handshake,
    conn_id: u32,
    body: &[u8],
    now: Instant,
) -> Result<Option<Vec<u8>>, String> {
    if conn_id == 0 || body.len() != ACCEPT_BODY {
        return Ok(None);
    }
    let Some((server_public, identity, signature)) = parse_accept(body) else {
        return Ok(None);
    };
    if let Some(pin) = settings.fingerprint
        && !ct_eq(&fingerprint_of(&identity), &pin)
    {
        return Err("отпечаток ключа сервера не совпадает".into());
    }
    let transcript = transcript_hash(&Transcript {
        protocol_id: &hs.hello.protocol_id,
        scene_hash: &hs.hello.scene_hash,
        client_public: &hs.hello.client_public,
        nonce: &hs.hello.nonce,
        server_public: &server_public,
        identity_public: &identity,
        conn_id,
    });
    if !verify_accept(&identity, &transcript, &signature) {
        return Err("подпись сервера не проверена".into());
    }
    let Some(shared) = hs.ephemeral.agree(&server_public) else {
        return Err("вырожденный ключ сервера".into());
    };
    let keys = derive_keys(
        &shared,
        &transcript,
        &password_hash(settings.password.as_ref()),
    );
    let mut session = Session::new(&keys.client_to_server, &keys.server_to_client);
    let proof = password_proof(settings.password.as_ref(), &transcript);
    let auth = auth_body(&proof);
    let Some((_, datagram)) = session.seal(conn_id, &auth) else {
        return Err("номер пакета исчерпан".into());
    };
    hs.conn_id = conn_id;
    hs.session = Some(session);
    hs.auth = auth;
    hs.stage = Stage::Auth;
    hs.sent_at = Some(now);
    Ok(Some(datagram))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(fingerprint: Option<[u8; 32]>, insecure_no_pin: bool) -> ClientConfig {
        ClientConfig {
            protocol_id: [1; 32],
            scene_hash: [2; 32],
            server: SocketAddr::from(([127, 0, 0, 1], 7777)),
            fingerprint,
            insecure_no_pin,
            password: None,
            timeout: Duration::from_secs(1),
        }
    }

    #[test]
    fn missing_pin_is_refused_unless_insecure_is_explicit() {
        assert!(matches!(
            Client::new(config(None, false), Instant::now()),
            Err(NetError::Config(_))
        ));
        assert!(Client::new(config(None, true), Instant::now()).is_ok());
        assert!(Client::new(config(Some([3; 32]), false), Instant::now()).is_ok());
    }

    #[test]
    fn datagrams_from_other_addresses_are_ignored() {
        let now = Instant::now();
        let mut client = Client::new(config(Some([3; 32]), false), now).unwrap();
        let other = SocketAddr::from(([127, 0, 0, 2], 7777));
        let out = client.handle_datagram(other, &[0u8; 20], now);
        assert!(out.datagrams.is_empty() && out.events.is_empty());
        assert!(client.local_peer().is_none());
        assert!(!client.is_connected());
        assert!(matches!(
            client.send(Channel::Reliable, b"x"),
            Err(NetError::NotConnected)
        ));
    }

    #[test]
    fn degenerate_server_key_is_refused_even_with_a_valid_signature() {
        use crate::ServerIdentity;
        use crate::crypto::{Transcript, transcript_hash};
        use crate::handshake::accept_datagram;

        let identity = ServerIdentity::ephemeral().unwrap();
        let server = SocketAddr::from(([127, 0, 0, 1], 7777));
        let now = Instant::now();
        let mut client = Client::new(config(Some(identity.fingerprint()), false), now).unwrap();
        let State::Handshake(hs) = &mut client.state else {
            panic!("клиент должен быть в рукопожатии");
        };
        hs.stage = Stage::Confirm;
        let transcript = transcript_hash(&Transcript {
            protocol_id: &hs.hello.protocol_id,
            scene_hash: &hs.hello.scene_hash,
            client_public: &hs.hello.client_public,
            nonce: &hs.hello.nonce,
            server_public: &[0; 32],
            identity_public: &identity.public_key(),
            conn_id: 7,
        });
        let signature = identity.sign(&transcript);
        let accept = accept_datagram(7, &[0; 32], &identity.public_key(), &signature);

        let out = client.handle_datagram(server, &accept, now);
        assert_eq!(
            out.events,
            vec![NetworkEvent::Disconnected {
                peer: SERVER_PEER_ID,
                reason: "вырожденный ключ сервера".into()
            }]
        );
        assert!(out.datagrams.is_empty(), "пароль не отправлен");
    }

    #[test]
    fn disconnect_reports_the_reason_once() {
        let mut client = Client::new(config(Some([3; 32]), false), Instant::now()).unwrap();
        let out = client.disconnect("выход");
        assert_eq!(
            out.events,
            vec![NetworkEvent::Disconnected {
                peer: SERVER_PEER_ID,
                reason: "выход".into()
            }]
        );
    }
}
