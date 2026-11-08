//! Сервер: рукопожатие с cookie, полуоткрытые сессии и слоты подключений. Ввода-вывода нет.
//!
//! Слот `max_clients` занимается только после первого аутентифицированного пакета клиента
//! с верным доказательством пароля. Полуоткрытые сессии ограничены общим числом и числом на IP-адрес
//! и сбрасываются по таймауту, поэтому повтор записанного Confirm слот не занимает. Новые Confirm
//! с одного IP-адреса ограничены по частоте, а адрес с серией неудачных входов временно отклоняется.

use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::time::{Duration, Instant};

use engine_scene::network::{PeerId, SERVER_PEER_ID};
use zeroize::Zeroizing;

use crate::crypto::{
    EphemeralKey, Password, ServerIdentity, Session, Transcript, ct_eq, derive_keys, password_hash,
    password_proof, random_bytes, reject_session, transcript_hash,
};
use crate::handshake::{
    HANDSHAKE_TIMEOUT, Hello, accept_datagram, cookie_datagram, cookie_valid, make_cookie,
    parse_confirm, reject_datagram,
};
use crate::link::{
    AUTH_BODY, Link, OP_AUTH, OP_DISCONNECT, OP_FRAMES, disconnect_body, parse_disconnect,
    welcome_body,
};
use crate::packet::{HEADER_LEN, Header, Kind, MAX_DATAGRAM, SEQ_LEN, TAG_LEN};
use crate::{Channel, NetError, NetworkEvent, Output};

/// Наибольшее число клиентов на сервере.
pub const MAX_CLIENTS: usize = 64;
const MAX_HALF_OPEN: usize = 256;
/// Полуоткрытые сессии с одного IP-адреса. Считаются по адресу, а не по порту.
pub(crate) const MAX_HALF_OPEN_PER_IP: usize = 4;
/// Запас новых Confirm с одного IP-адреса и скорость пополнения в секунду.
const CONFIRM_BURST: u32 = 8;
const CONFIRM_PER_SEC: f64 = 1.0;
/// Наибольшее число адресов, которые сервер помнит одновременно (частота Confirm и отказы входа).
const MAX_TRACKED_IPS: usize = 1024;
/// Число неудачных входов подряд, после которого адрес временно отклоняется.
pub(crate) const AUTH_FAILURE_LIMIT: u32 = 5;
const AUTH_BACKOFF_MAX: Duration = Duration::from_secs(60);
const PASSWORD_REJECT: &str = "не удалось проверить пароль";
/// Размер датаграммы с попыткой входа: по нему опознаётся неудачный Auth.
const AUTH_DATAGRAM: usize = HEADER_LEN + SEQ_LEN + AUTH_BODY + TAG_LEN;
/// Доказательство пароля в теле Auth идёт сразу за операцией.
const PROOF_RANGE: std::ops::Range<usize> = 1..33;

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
    /// Сессия с ключами от пароля: открывает Auth.
    session: Session,
    /// Сессия отказа: ключи не зависят от пароля, поэтому клиент прочтёт отказ без него.
    reject: Session,
    transcript: [u8; 32],
    accept: Vec<u8>,
    expires: Instant,
}

/// Запас новых Confirm одного адреса: пополняется, но не больше `CONFIRM_BURST`.
#[derive(Clone, Copy)]
struct Bucket {
    tokens: f64,
    updated: Instant,
}

impl Bucket {
    fn available(&self, now: Instant) -> f64 {
        let elapsed = now.saturating_duration_since(self.updated).as_secs_f64();
        (self.tokens + elapsed * CONFIRM_PER_SEC).min(f64::from(CONFIRM_BURST))
    }
}

/// Ограничение частоты новых Confirm по IP-адресу. Таблица ограничена: когда она полна, новые
/// адреса отклоняются, пока в ней не освободятся записи с полным запасом.
#[derive(Default)]
struct ConfirmLimiter {
    buckets: HashMap<IpAddr, Bucket>,
}

impl ConfirmLimiter {
    fn allow(&mut self, ip: IpAddr, now: Instant) -> bool {
        let full = f64::from(CONFIRM_BURST);
        if !self.buckets.contains_key(&ip) && self.buckets.len() >= MAX_TRACKED_IPS {
            self.buckets
                .retain(|_, bucket| bucket.available(now) < full);
            if self.buckets.len() >= MAX_TRACKED_IPS {
                return false;
            }
        }
        let bucket = self.buckets.entry(ip).or_insert(Bucket {
            tokens: full,
            updated: now,
        });
        let available = bucket.available(now);
        let allowed = available >= 1.0;
        let tokens = if allowed { available - 1.0 } else { available };
        *bucket = Bucket {
            tokens,
            updated: now,
        };
        allowed
    }
}

/// Неудачные входы одного адреса и время временного отказа.
#[derive(Default, Clone, Copy)]
struct AuthRecord {
    failures: u32,
    blocked_until: Option<Instant>,
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
    confirms: ConfirmLimiter,
    auth_failures: HashMap<IpAddr, AuthRecord>,
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
            confirms: ConfirmLimiter::default(),
            auth_failures: HashMap::new(),
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
        // Участник с переполненной очередью надёжных сообщений отключается с причиной.
        let faulted: Vec<(u32, &'static str)> = self
            .links
            .iter()
            .filter_map(|(conn_id, link)| link.fault().map(|reason| (*conn_id, reason)))
            .collect();
        for (conn_id, reason) in faulted {
            self.close(conn_id, reason, true, &mut out);
        }
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

    /// Ставит сообщение в очередь одного участника. При переполнении надёжной очереди участник
    /// отключается в следующем `update` с причиной; сама ошибка возвращается сразу.
    pub fn send(&mut self, peer: PeerId, channel: Channel, bytes: &[u8]) -> Result<(), NetError> {
        let conn_id = self.conn_of(peer).ok_or(NetError::UnknownPeer)?;
        match self.links.get_mut(&conn_id) {
            Some(link) => link.enqueue(channel, bytes),
            None => Err(NetError::UnknownPeer),
        }
    }

    /// Ставит сообщение всем участникам. Медленный участник с переполненной надёжной очередью
    /// отключается, остальные получают сообщение.
    pub fn broadcast(&mut self, channel: Channel, bytes: &[u8]) -> Result<(), NetError> {
        let mut result = Ok(());
        for link in self.links.values_mut() {
            match link.enqueue(channel, bytes) {
                Ok(()) => {}
                // Переполнение надёжной очереди отключает этого участника; остальным сообщение уйдёт.
                Err(NetError::QueueFull) if channel.is_reliable() => {}
                Err(err) => {
                    if result.is_ok() {
                        result = Err(err);
                    }
                }
            }
        }
        result
    }

    /// Вызывается, пока приложение не забирает события: молчание пиров тогда вызвано нами,
    /// поэтому таймаут соединений не идёт.
    pub fn hold_timeouts(&mut self, now: Instant) {
        for link in self.links.values_mut() {
            link.hold(now);
        }
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
        // Лимиты считаются по IP-адресу: иначе один хост открывает их по одному разу на каждый порт.
        let per_ip = self
            .halves
            .values()
            .filter(|half| half.addr.ip() == from.ip())
            .count();
        if self.halves.len() >= MAX_HALF_OPEN || per_ip >= MAX_HALF_OPEN_PER_IP {
            return;
        }
        if !self.confirms.allow(from.ip(), now) {
            return;
        }
        match self.open_half(from, &hello, now) {
            Ok(accept) => out.datagrams.push((from, accept)),
            // Вырожденный ключ присылает сбойный или злонамеренный клиент; это не ошибка сервера.
            Err(err) => log::debug!("рукопожатие с {from} не начато: {err}"),
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
        let reject = reject_session(&shared, &transcript);
        let signature = self.identity.sign(&transcript);
        let accept = accept_datagram(conn_id, &server_public, &identity_public, &signature);
        let session = Session::new(&keys.server_to_client, &keys.client_to_server);
        self.halves.insert(
            conn_id,
            HalfOpen {
                addr,
                client_public: hello.client_public,
                session,
                reject,
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
        self.on_half_data(from, conn_id, bytes, now, out);
    }

    fn on_half_data(
        &mut self,
        from: SocketAddr,
        conn_id: u32,
        bytes: &[u8],
        now: Instant,
        out: &mut Output,
    ) {
        if self.auth_blocked(from.ip(), now) {
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
            // Отказ шифруется ключом рукопожатия, его не подделать. Он отправляется, только если запрос
            // не короче отказа, и рукопожатие после него закрывается: сервер не усиливает чужой трафик
            // и не отвечает на один и тот же запрос дважды.
            let reply = half.reject.seal(conn_id, &disconnect_body(PASSWORD_REJECT));
            if let Some((_, datagram)) = reply
                && bytes.len() >= datagram.len()
            {
                self.halves.remove(&conn_id);
                out.datagrams.push((from, datagram));
                if bytes.len() == AUTH_DATAGRAM {
                    self.record_auth_failure(from.ip(), now);
                }
            }
            return;
        };
        let Some(mut half) = self.halves.remove(&conn_id) else {
            return;
        };
        if body.first() != Some(&OP_AUTH) || body.len() != AUTH_BODY {
            reject_half(from, conn_id, &mut half, "нарушение протокола", out);
            return;
        }
        let expected = password_proof(self.password.as_ref(), &half.transcript);
        if !ct_eq(&body[PROOF_RANGE], &expected) {
            self.record_auth_failure(from.ip(), now);
            reject_half(from, conn_id, &mut half, "неверный пароль", out);
            return;
        }
        // Номера участников не переиспользуются: исчерпание u32 равносильно заполненному серверу.
        if self.links.len() >= self.max_clients || self.next_peer == PeerId::MAX {
            reject_half(from, conn_id, &mut half, "сервер заполнен", out);
            return;
        }
        self.auth_failures.remove(&from.ip());
        self.promote(conn_id, half, now, out);
    }

    /// Адрес временно отклонён после серии неудачных входов.
    fn auth_blocked(&self, ip: IpAddr, now: Instant) -> bool {
        self.auth_failures
            .get(&ip)
            .and_then(|record| record.blocked_until)
            .is_some_and(|until| now < until)
    }

    /// Учитывает неудачный вход. С `AUTH_FAILURE_LIMIT` попытки подряд адрес отклоняется на срок,
    /// который растёт вдвое с каждой следующей неудачей и ограничен `AUTH_BACKOFF_MAX`.
    fn record_auth_failure(&mut self, ip: IpAddr, now: Instant) {
        if !self.auth_failures.contains_key(&ip) && self.auth_failures.len() >= MAX_TRACKED_IPS {
            self.auth_failures
                .retain(|_, record| record.blocked_until.is_some_and(|until| until > now));
            if self.auth_failures.len() >= MAX_TRACKED_IPS {
                return;
            }
        }
        let record = self.auth_failures.entry(ip).or_default();
        record.failures = record.failures.saturating_add(1);
        if record.failures >= AUTH_FAILURE_LIMIT {
            let steps = (record.failures - AUTH_FAILURE_LIMIT).min(16);
            let backoff = Duration::from_secs(1u64 << steps).min(AUTH_BACKOFF_MAX);
            record.blocked_until = Some(now + backoff);
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

/// Отказ на рукопожатии, которое прошло расшифровку: шифруется ключом отказа, не паролем.
fn reject_half(
    from: SocketAddr,
    conn_id: u32,
    half: &mut HalfOpen,
    reason: &str,
    out: &mut Output,
) {
    if let Some((_, datagram)) = half.reject.seal(conn_id, &disconnect_body(reason)) {
        out.datagrams.push((from, datagram));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn confirm_limiter_allows_a_burst_then_refills_per_address() {
        let mut limiter = ConfirmLimiter::default();
        let now = Instant::now();
        let ip: IpAddr = [127, 0, 0, 1].into();
        let burst = (0..CONFIRM_BURST)
            .filter(|_| limiter.allow(ip, now))
            .count();
        assert_eq!(burst, CONFIRM_BURST as usize);
        assert!(!limiter.allow(ip, now), "запас исчерпан");
        let other: IpAddr = [127, 0, 0, 2].into();
        assert!(limiter.allow(other, now), "у другого адреса свой запас");
        assert!(
            limiter.allow(ip, now + Duration::from_secs(1)),
            "запас пополнился"
        );
    }

    #[test]
    fn confirm_limiter_table_is_bounded() {
        let mut limiter = ConfirmLimiter::default();
        let now = Instant::now();
        for index in 0..MAX_TRACKED_IPS {
            let ip = IpAddr::from([10, 0, (index >> 8) as u8, index as u8]);
            assert!(limiter.allow(ip, now));
        }
        assert!(!limiter.allow([10, 1, 0, 0].into(), now), "таблица полна");
        let later = now + Duration::from_secs(10);
        assert!(
            limiter.allow([10, 1, 0, 0].into(), later),
            "полные записи освобождают место"
        );
    }

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
