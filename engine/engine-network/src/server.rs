//! Сервер: рукопожатие с cookie, полуоткрытые сессии и слоты подключений. Ввода-вывода нет.
//!
//! Слот `max_clients` занимается только после первого аутентифицированного пакета клиента
//! с верным доказательством пароля. Полуоткрытые сессии ограничены общим числом и числом на адрес
//! и сбрасываются по таймауту, поэтому повтор записанного Confirm слот не занимает.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::time::{Duration, Instant};

use engine_scene::network::{PeerId, SERVER_PEER_ID};
use zeroize::Zeroizing;

use crate::crypto::{
    EphemeralKey, Password, ServerIdentity, Session, Transcript, ct_eq, derive_keys, password_hash,
    password_proof, random_bytes, transcript_hash,
};
use crate::handshake::{
    HANDSHAKE_TIMEOUT, Hello, accept_datagram, cookie_datagram, cookie_valid, make_cookie,
    parse_confirm, reject_datagram,
};
use crate::link::{
    Link, OP_AUTH, OP_DISCONNECT, OP_FRAMES, disconnect_body, parse_disconnect, welcome_body,
};
use crate::packet::{HEADER_LEN, Header, Kind, MAX_DATAGRAM};
use crate::{Channel, NetError, NetworkEvent, Output};

/// Наибольшее число клиентов на сервере.
pub const MAX_CLIENTS: usize = 64;
const MAX_HALF_OPEN: usize = 256;
const MAX_HALF_OPEN_PER_ADDR: usize = 4;
const AUTH_BODY: usize = 1 + 32;

/// Настройки сервера.
pub struct ServerConfig {
    pub protocol_id: [u8; 32],
    pub scene_hash: [u8; 32],
    pub max_clients: usize,
    pub password: Option<Password>,
    /// Срок молчания, после которого соединение считается разорванным.
    pub timeout: Duration,
}

/// Рукопожатие, которое ещё не получило верного Auth.
struct HalfOpen {
    addr: SocketAddr,
    client_public: [u8; 32],
    session: Session,
    transcript: [u8; 32],
    accept: Vec<u8>,
    expires: Instant,
}

/// Машина состояний сервера.
pub struct Server {
    protocol_id: [u8; 32],
    scene_hash: [u8; 32],
    max_clients: usize,
    password: Option<Password>,
    timeout: Duration,
    identity: ServerIdentity,
    cookie_key: Zeroizing<[u8; 32]>,
    epoch: Instant,
    halves: HashMap<u32, HalfOpen>,
    links: HashMap<u32, Link>,
    next_peer: PeerId,
}

impl Server {
    pub fn new(
        config: ServerConfig,
        identity: ServerIdentity,
        now: Instant,
    ) -> Result<Self, NetError> {
        if !(1..=MAX_CLIENTS).contains(&config.max_clients) {
            return Err(NetError::Config(format!(
                "max_clients должен быть в диапазоне 1..={MAX_CLIENTS}"
            )));
        }
        Ok(Self {
            protocol_id: config.protocol_id,
            scene_hash: config.scene_hash,
            max_clients: config.max_clients,
            password: config.password,
            timeout: config.timeout,
            identity,
            cookie_key: Zeroizing::new(random_bytes::<32>()?),
            epoch: now,
            halves: HashMap::new(),
            links: HashMap::new(),
            next_peer: SERVER_PEER_ID + 1,
        })
    }

    /// Обрабатывает входящую датаграмму и возвращает ответы и события.
    pub fn handle_datagram(&mut self, from: SocketAddr, bytes: &[u8], now: Instant) -> Output {
        let mut out = Output::default();
        let Some(header) = Header::decode(bytes) else {
            return out;
        };
        match header.kind {
            Kind::Hello => self.on_hello(from, bytes, now, &mut out),
            Kind::Confirm => self.on_confirm(from, bytes, now, &mut out),
            Kind::Data => self.on_data(from, header.conn_id, bytes, now, &mut out),
            Kind::Cookie | Kind::Accept | Kind::Reject => {}
        }
        out
    }

    /// Истекающие сессии, таймауты и отправка накопленных кадров.
    pub fn update(&mut self, now: Instant) -> Output {
        let mut out = Output::default();
        self.halves.retain(|_, half| half.expires > now);
        let timed_out: Vec<u32> = self
            .links
            .iter()
            .filter(|(_, link)| link.timed_out(now))
            .map(|(conn_id, _)| *conn_id)
            .collect();
        for conn_id in timed_out {
            self.close(conn_id, "таймаут", false, &mut out);
        }
        let ids: Vec<u32> = self.links.keys().copied().collect();
        for conn_id in ids {
            let Some(link) = self.links.get_mut(&conn_id) else {
                continue;
            };
            if let Err(reason) = link.flush(now, &mut out) {
                self.close(conn_id, reason, true, &mut out);
            }
        }
        out
    }

    /// Ставит сообщение в очередь одного участника.
    pub fn send(&mut self, peer: PeerId, channel: Channel, bytes: &[u8]) -> Result<(), NetError> {
        let conn_id = self.conn_of(peer).ok_or(NetError::UnknownPeer)?;
        match self.links.get_mut(&conn_id) {
            Some(link) => link.enqueue(channel, bytes),
            None => Err(NetError::UnknownPeer),
        }
    }

    /// Ставит сообщение всем участникам. Возвращает первую ошибку, но остальным очередь не отменяет.
    pub fn broadcast(&mut self, channel: Channel, bytes: &[u8]) -> Result<(), NetError> {
        let mut result = Ok(());
        for link in self.links.values_mut() {
            if let Err(err) = link.enqueue(channel, bytes)
                && result.is_ok()
            {
                result = Err(err);
            }
        }
        result
    }

    /// Отключает участника и сообщает ему причину.
    pub fn disconnect(&mut self, peer: PeerId, reason: &str) -> Output {
        let mut out = Output::default();
        if let Some(conn_id) = self.conn_of(peer) {
            self.close(conn_id, reason, true, &mut out);
        }
        out
    }

    /// Отключает всех участников при остановке сервера.
    pub fn shutdown(&mut self, reason: &str) -> Output {
        let mut out = Output::default();
        let ids: Vec<u32> = self.links.keys().copied().collect();
        for conn_id in ids {
            self.close(conn_id, reason, true, &mut out);
        }
        out
    }

    fn conn_of(&self, peer: PeerId) -> Option<u32> {
        self.links
            .iter()
            .find(|(_, link)| link.peer == peer)
            .map(|(conn_id, _)| *conn_id)
    }

    fn now_ms(&self, now: Instant) -> u64 {
        u64::try_from(now.saturating_duration_since(self.epoch).as_millis()).unwrap_or(u64::MAX)
    }

    fn mismatch(&self, hello: &Hello) -> Option<&'static str> {
        if hello.protocol_id != self.protocol_id {
            Some("протокол не совпадает")
        } else if hello.scene_hash != self.scene_hash {
            Some("версия сцены не совпадает")
        } else {
            None
        }
    }

    fn on_hello(&self, from: SocketAddr, bytes: &[u8], now: Instant, out: &mut Output) {
        if bytes.len() != MAX_DATAGRAM {
            return;
        }
        let Some(hello) = Hello::decode(&bytes[HEADER_LEN..]) else {
            return;
        };
        if let Some(reason) = self.mismatch(&hello) {
            out.datagrams.push((from, reject_datagram(reason)));
            return;
        }
        let cookie = make_cookie(&self.cookie_key, from, self.now_ms(now), &hello);
        out.datagrams.push((from, cookie_datagram(&cookie)));
    }

    fn on_confirm(&mut self, from: SocketAddr, bytes: &[u8], now: Instant, out: &mut Output) {
        if bytes.len() != MAX_DATAGRAM {
            return;
        }
        let Some((cookie, hello)) = parse_confirm(&bytes[HEADER_LEN..]) else {
            return;
        };
        if self.links.values().any(|link| link.addr == from) {
            return;
        }
        if !cookie_valid(&self.cookie_key, from, self.now_ms(now), &cookie, &hello) {
            return;
        }
        if let Some(reason) = self.mismatch(&hello) {
            out.datagrams.push((from, reject_datagram(reason)));
            return;
        }
        if let Some(half) = self
            .halves
            .values()
            .find(|half| half.addr == from && half.client_public == hello.client_public)
        {
            out.datagrams.push((from, half.accept.clone()));
            return;
        }
        let per_addr = self
            .halves
            .values()
            .filter(|half| half.addr == from)
            .count();
        if self.halves.len() >= MAX_HALF_OPEN || per_addr >= MAX_HALF_OPEN_PER_ADDR {
            return;
        }
        match self.open_half(from, &hello, now) {
            Ok(accept) => out.datagrams.push((from, accept)),
            Err(err) => log::warn!("рукопожатие с {from} не начато: {err}"),
        }
    }

    fn open_half(
        &mut self,
        addr: SocketAddr,
        hello: &Hello,
        now: Instant,
    ) -> Result<Vec<u8>, NetError> {
        let conn_id = self.fresh_conn_id()?;
        let ephemeral = EphemeralKey::random()?;
        let server_public = ephemeral.public();
        let shared = ephemeral
            .agree(&hello.client_public)
            .ok_or_else(|| NetError::InvalidMessage("вырожденный ключ клиента".into()))?;
        let identity_public = self.identity.public_key();
        let transcript = transcript_hash(&Transcript {
            protocol_id: &self.protocol_id,
            scene_hash: &self.scene_hash,
            client_public: &hello.client_public,
            nonce: &hello.nonce,
            server_public: &server_public,
            identity_public: &identity_public,
            conn_id,
        });
        let keys = derive_keys(&shared, &transcript, &password_hash(self.password.as_ref()));
        let signature = self.identity.sign(&transcript);
        let accept = accept_datagram(conn_id, &server_public, &identity_public, &signature);
        let session = Session::new(&keys.server_to_client, &keys.client_to_server);
        self.halves.insert(
            conn_id,
            HalfOpen {
                addr,
                client_public: hello.client_public,
                session,
                transcript,
                accept: accept.clone(),
                expires: now + HANDSHAKE_TIMEOUT,
            },
        );
        Ok(accept)
    }

    fn fresh_conn_id(&self) -> Result<u32, NetError> {
        for _ in 0..16 {
            let conn_id = u32::from_be_bytes(random_bytes::<4>()?);
            if conn_id != 0
                && !self.halves.contains_key(&conn_id)
                && !self.links.contains_key(&conn_id)
            {
                return Ok(conn_id);
            }
        }
        Err(NetError::Internal(
            "не удалось выбрать идентификатор соединения".into(),
        ))
    }

    fn on_data(
        &mut self,
        from: SocketAddr,
        conn_id: u32,
        bytes: &[u8],
        now: Instant,
        out: &mut Output,
    ) {
        if let Some(link) = self.links.get_mut(&conn_id) {
            if link.addr != from {
                return;
            }
            let Some((seq, body)) = link.open(now, bytes) else {
                return;
            };
            self.on_link_body(conn_id, seq, &body, now, out);
            return;
        }
        let Some(half) = self.halves.get_mut(&conn_id) else {
            return;
        };
        if half.addr != from {
            return;
        }
        let Some((_, body)) = half.session.open(bytes) else {
            // Ключи производные от пароля: неверный пароль проявляется как ошибка расшифровки.
            // Reject не подписан и отправляется только до аутентификации.
            out.datagrams
                .push((from, reject_datagram("не удалось проверить пароль")));
            return;
        };
        let Some(mut half) = self.halves.remove(&conn_id) else {
            return;
        };
        if body.first() != Some(&OP_AUTH) || body.len() != AUTH_BODY {
            self.reject_half(from, conn_id, &mut half, "нарушение протокола", out);
            return;
        }
        let expected = password_proof(self.password.as_ref(), &half.transcript);
        if !ct_eq(&body[1..], &expected) {
            self.reject_half(from, conn_id, &mut half, "неверный пароль", out);
            return;
        }
        // Номера участников не переиспользуются: исчерпание u32 равносильно заполненному серверу.
        if self.links.len() >= self.max_clients || self.next_peer == PeerId::MAX {
            self.reject_half(from, conn_id, &mut half, "сервер заполнен", out);
            return;
        }
        self.promote(conn_id, half, now, out);
    }

    fn reject_half(
        &self,
        from: SocketAddr,
        conn_id: u32,
        half: &mut HalfOpen,
        reason: &str,
        out: &mut Output,
    ) {
        if let Some((_, datagram)) = half.session.seal(conn_id, &disconnect_body(reason)) {
            out.datagrams.push((from, datagram));
        }
    }

    fn promote(&mut self, conn_id: u32, half: HalfOpen, now: Instant, out: &mut Output) {
        let peer = self.next_peer;
        self.next_peer += 1;
        let mut link = Link::new(peer, half.addr, conn_id, half.session, now, self.timeout);
        if let Some(datagram) = link.seal(&welcome_body(peer)) {
            out.datagrams.push((link.addr, datagram));
        }
        self.links.insert(conn_id, link);
        out.events.push(NetworkEvent::Connected { peer });
    }

    fn on_link_body(
        &mut self,
        conn_id: u32,
        seq: u64,
        body: &[u8],
        now: Instant,
        out: &mut Output,
    ) {
        match body.first().copied() {
            Some(OP_FRAMES) => {
                let Some(link) = self.links.get_mut(&conn_id) else {
                    return;
                };
                if let Err(reason) = link.receive_frames(now, seq, &body[1..], &mut out.events) {
                    self.close(conn_id, reason, true, out);
                }
            }
            Some(OP_DISCONNECT) => {
                let reason = parse_disconnect(&body[1..]);
                self.close(conn_id, &reason, false, out);
            }
            Some(OP_AUTH) => {
                // Повторный Auth: Welcome мог потеряться, отправляем его снова.
                if let Some(link) = self.links.get_mut(&conn_id) {
                    let peer = link.peer;
                    let addr = link.addr;
                    if let Some(datagram) = link.seal(&welcome_body(peer)) {
                        out.datagrams.push((addr, datagram));
                    }
                }
            }
            _ => {}
        }
    }

    fn close(&mut self, conn_id: u32, reason: &str, notify: bool, out: &mut Output) {
        let Some(mut link) = self.links.remove(&conn_id) else {
            return;
        };
        if notify && let Some(datagram) = link.seal(&disconnect_body(reason)) {
            out.datagrams.push((link.addr, datagram));
        }
        out.events.push(NetworkEvent::Disconnected {
            peer: link.peer,
            reason: reason.to_owned(),
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn max_clients_must_be_within_the_documented_range() {
        let config = |max_clients| ServerConfig {
            protocol_id: [0; 32],
            scene_hash: [0; 32],
            max_clients,
            password: None,
            timeout: Duration::from_secs(1),
        };
        let identity = || ServerIdentity::ephemeral().unwrap();
        let now = Instant::now();
        assert!(Server::new(config(0), identity(), now).is_err());
        assert!(Server::new(config(MAX_CLIENTS + 1), identity(), now).is_err());
        assert!(Server::new(config(MAX_CLIENTS), identity(), now).is_ok());
    }
}
