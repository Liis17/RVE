//! Криптография протокола: X25519, Ed25519, производные ключи, ChaCha20-Poly1305 и окно повторов.
//!
//! Все секреты создаются через `getrandom`; ключи, которые живут дольше одного вызова, обнуляются.

use std::fmt::{self, Write as _};
use std::io::Write as _;
use std::path::Path;
use std::time::{Duration, SystemTime};

use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use chacha20poly1305::{ChaCha20Poly1305, Key, Nonce};
use ed25519_dalek::{Signature, Signer, SigningKey, VerifyingKey};
use x25519_dalek::{PublicKey, StaticSecret};
use zeroize::{Zeroize, Zeroizing};

use crate::NetError;
use crate::packet::{HEADER_LEN, Header, Kind, SEQ_LEN, TAG_LEN};

/// Размер окна защиты от повторов в номерах пакетов.
const REPLAY_WINDOW: u64 = 1024;
const REPLAY_WORDS: usize = (REPLAY_WINDOW / 64) as usize;

/// Пароль комнаты. Хранится в памяти, обнуляется при удалении и не выводится в отладочной печати.
#[derive(Clone)]
pub struct Password(Zeroizing<Vec<u8>>);

impl Password {
    pub fn new(bytes: impl Into<Vec<u8>>) -> Self {
        Self(Zeroizing::new(bytes.into()))
    }

    pub(crate) fn as_bytes(&self) -> &[u8] {
        &self.0
    }
}

impl fmt::Debug for Password {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Password(***)")
    }
}

/// Долговременный ключ сервера (Ed25519). Клиент закрепляет его отпечаток.
pub struct ServerIdentity {
    signing: SigningKey,
}

impl ServerIdentity {
    /// Новый ключ, который не сохраняется на диск.
    pub fn ephemeral() -> Result<Self, NetError> {
        let mut seed = random_bytes::<32>()?;
        let signing = SigningKey::from_bytes(&seed);
        seed.zeroize();
        Ok(Self { signing })
    }

    /// Читает ключ из файла (ровно 32 байта) или создаёт новый файл. На Unix файл создаётся с правами 0600.
    pub fn load_or_create(path: &Path) -> Result<Self, NetError> {
        remove_stale_temps(path, SystemTime::now());
        match std::fs::read(path) {
            Ok(mut bytes) => {
                warn_if_shared(path);
                let result = Self::from_seed_bytes(path, &bytes);
                bytes.zeroize();
                result
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Self::create(path),
            Err(err) => Err(file_error(path, &err)),
        }
    }

    /// Создаёт ключ и публикует его атомарно: частично записанный файл не появляется под именем ключа.
    fn create(path: &Path) -> Result<Self, NetError> {
        let mut seed = random_bytes::<32>()?;
        let result = match publish_new_file(path, &seed) {
            Ok(()) => Ok(Self {
                signing: SigningKey::from_bytes(&seed),
            }),
            // Файл появился между проверкой и публикацией: используем его.
            Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {
                seed.zeroize();
                return Self::load_or_create(path);
            }
            Err(err) => Err(file_error(path, &err)),
        };
        seed.zeroize();
        result
    }

    fn from_seed_bytes(path: &Path, bytes: &[u8]) -> Result<Self, NetError> {
        let mut seed: [u8; 32] = bytes.try_into().map_err(|_| {
            NetError::Config(format!(
                "файл ключа сервера {} должен содержать 32 байта",
                path.display()
            ))
        })?;
        let signing = SigningKey::from_bytes(&seed);
        seed.zeroize();
        Ok(Self { signing })
    }

    /// Открытый ключ Ed25519.
    pub fn public_key(&self) -> [u8; 32] {
        self.signing.verifying_key().to_bytes()
    }

    /// Отпечаток для закрепления у клиента: blake3 от открытого ключа.
    pub fn fingerprint(&self) -> [u8; 32] {
        fingerprint_of(&self.public_key())
    }

    pub fn fingerprint_hex(&self) -> String {
        to_hex(&self.fingerprint())
    }

    /// Подпись транскрипта рукопожатия.
    pub(crate) fn sign(&self, transcript: &[u8; 32]) -> [u8; 64] {
        self.signing.sign(&accept_message(transcript)).to_bytes()
    }
}

fn file_error(path: &Path, err: &std::io::Error) -> NetError {
    NetError::Config(format!("ключ сервера {}: {err}", path.display()))
}

/// Срок, после которого временный файл публикации считается оставшимся после аварийного завершения.
const STALE_TEMP_AGE: Duration = Duration::from_secs(3600);

/// Пишет ключ во временный файл рядом с `path` и публикует его под именем `path` без перезаписи
/// (hard link). Временный файл удаляется в любом случае. Если hard link недоступен (например, на FAT
/// или части сетевых ресурсов), ключ пишется напрямую под именем `path` через `create_new`.
fn publish_new_file(path: &Path, seed: &[u8; 32]) -> std::io::Result<()> {
    publish_with(path, seed, |temp, target| std::fs::hard_link(temp, target))
}

/// Публикация с внедряемой операцией связывания, чтобы тесты проверяли запасной путь без зависимости от ФС.
/// Существующий ключ не перезаписывается ни одним путём: `AlreadyExists` возвращается сразу.
fn publish_with(
    path: &Path,
    seed: &[u8; 32],
    link: impl FnOnce(&Path, &Path) -> std::io::Result<()>,
) -> std::io::Result<()> {
    let mut suffix = [0u8; 8];
    getrandom::fill(&mut suffix).map_err(|err| std::io::Error::other(format!("{err}")))?;
    let temp = path.with_file_name(format!(".{}.{}.tmp", file_name(path), to_hex(&suffix)));
    let result = write_new_file(&temp, seed).and_then(|()| link(&temp, path));
    let _ = std::fs::remove_file(&temp);
    match result {
        Err(err) if err.kind() != std::io::ErrorKind::AlreadyExists => write_new_file(path, seed),
        other => other,
    }
}

fn file_name(path: &Path) -> String {
    path.file_name().map_or_else(
        || "server.key".into(),
        |name| name.to_string_lossy().into_owned(),
    )
}

/// Создаёт файл, только если его нет, и пишет в него ключ. При ошибке записи частичный файл удаляется,
/// чтобы недописанный ключ не остался под именем ключа.
fn write_new_file(path: &Path, seed: &[u8; 32]) -> std::io::Result<()> {
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(path)?;
    let written = file.write_all(seed).and_then(|()| file.sync_all());
    // Файл закрывается до удаления: на Windows открытый файл удалить нельзя.
    drop(file);
    if written.is_err() {
        let _ = std::fs::remove_file(path);
    }
    written
}

/// Удаляет временные файлы публикации ключа, оставшиеся рядом с `path` после аварии. Свежие файлы не трогаем:
/// их может публиковать другой процесс прямо сейчас.
fn remove_stale_temps(path: &Path, now: SystemTime) {
    let dir = match path.parent() {
        Some(dir) if !dir.as_os_str().is_empty() => dir,
        _ => Path::new("."),
    };
    let prefix = format!(".{}.", file_name(path));
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        if !is_temp_name(name, &prefix) {
            continue;
        }
        let modified = entry.metadata().and_then(|meta| meta.modified());
        let stale = modified.is_ok_and(|modified| {
            now.duration_since(modified)
                .is_ok_and(|age| age >= STALE_TEMP_AGE)
        });
        if stale {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

/// Имя временного файла публикации: префикс, 16 шестнадцатеричных символов и `.tmp`.
fn is_temp_name(name: &str, prefix: &str) -> bool {
    name.strip_prefix(prefix)
        .and_then(|rest| rest.strip_suffix(".tmp"))
        .is_some_and(|hex| hex.len() == 16 && hex.bytes().all(|byte| byte.is_ascii_hexdigit()))
}

/// Предупреждает, если к файлу ключа имеют доступ другие пользователи (только Unix).
#[cfg(unix)]
fn warn_if_shared(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    if let Ok(metadata) = std::fs::metadata(path)
        && has_group_or_world_access(metadata.permissions().mode())
    {
        log::warn!(
            "файл ключа сервера {} доступен другим пользователям; рекомендуется режим 0600",
            path.display()
        );
    }
}

#[cfg(not(unix))]
fn warn_if_shared(_path: &Path) {}

#[cfg(unix)]
fn has_group_or_world_access(mode: u32) -> bool {
    mode & 0o077 != 0
}

/// Отпечаток открытого ключа.
pub(crate) fn fingerprint_of(public: &[u8; 32]) -> [u8; 32] {
    *blake3::hash(public).as_bytes()
}

fn to_hex(bytes: &[u8]) -> String {
    let mut text = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        let _ = write!(text, "{byte:02x}");
    }
    text
}

/// Разбирает отпечаток сервера: ровно 64 hex-символа, регистр не важен.
pub fn parse_fingerprint(text: &str) -> Option<[u8; 32]> {
    let bytes = text.as_bytes();
    if bytes.len() != 64 {
        return None;
    }
    let mut fingerprint = [0_u8; 32];
    for (slot, pair) in fingerprint.iter_mut().zip(bytes.as_chunks::<2>().0) {
        let high = hex_value(pair[0])?;
        let low = hex_value(pair[1])?;
        *slot = high << 4 | low;
    }
    Some(fingerprint)
}

fn hex_value(digit: u8) -> Option<u8> {
    match digit {
        b'0'..=b'9' => Some(digit - b'0'),
        b'a'..=b'f' => Some(digit - b'a' + 10),
        b'A'..=b'F' => Some(digit - b'A' + 10),
        _ => None,
    }
}

/// Случайные байты из системного генератора.
pub(crate) fn random_bytes<const N: usize>() -> Result<[u8; N], NetError> {
    let mut bytes = [0u8; N];
    getrandom::fill(&mut bytes)
        .map_err(|_| NetError::Internal("генератор случайных чисел недоступен".into()))?;
    Ok(bytes)
}

/// Эфемерный ключ X25519 одного рукопожатия; обнуляется при удалении.
pub(crate) struct EphemeralKey(StaticSecret);

impl EphemeralKey {
    pub(crate) fn random() -> Result<Self, NetError> {
        let mut bytes = random_bytes::<32>()?;
        let secret = StaticSecret::from(bytes);
        bytes.zeroize();
        Ok(Self(secret))
    }

    pub(crate) fn public(&self) -> [u8; 32] {
        PublicKey::from(&self.0).to_bytes()
    }

    /// Общий секрет. `None`, если точка вырождена (общий секрет нулевой).
    pub(crate) fn agree(&self, peer_public: &[u8; 32]) -> Option<Zeroizing<[u8; 32]>> {
        let mut shared = self.0.diffie_hellman(&PublicKey::from(*peer_public));
        let result = shared
            .was_contributory()
            .then(|| Zeroizing::new(shared.to_bytes()));
        shared.zeroize();
        result
    }
}

impl Drop for EphemeralKey {
    fn drop(&mut self) {
        self.0.zeroize();
    }
}

/// Данные, которые связывает подпись сервера и производные ключи.
pub(crate) struct Transcript<'a> {
    pub protocol_id: &'a [u8; 32],
    pub scene_hash: &'a [u8; 32],
    pub client_public: &'a [u8; 32],
    pub nonce: &'a [u8; 16],
    pub server_public: &'a [u8; 32],
    pub identity_public: &'a [u8; 32],
    pub conn_id: u32,
}

pub(crate) fn transcript_hash(transcript: &Transcript<'_>) -> [u8; 32] {
    let mut hasher = blake3::Hasher::new_derive_key("RVE net v1 transcript");
    hasher.update(transcript.protocol_id);
    hasher.update(transcript.scene_hash);
    hasher.update(transcript.client_public);
    hasher.update(transcript.nonce);
    hasher.update(transcript.server_public);
    hasher.update(transcript.identity_public);
    hasher.update(&transcript.conn_id.to_be_bytes());
    *hasher.finalize().as_bytes()
}

fn accept_message(transcript: &[u8; 32]) -> Vec<u8> {
    let mut message = b"RVE net v1 accept".to_vec();
    message.extend_from_slice(transcript);
    message
}

/// Проверяет подпись сервера над транскриптом.
pub(crate) fn verify_accept(
    identity: &[u8; 32],
    transcript: &[u8; 32],
    signature: &[u8; 64],
) -> bool {
    let Ok(key) = VerifyingKey::from_bytes(identity) else {
        return false;
    };
    key.verify_strict(
        &accept_message(transcript),
        &Signature::from_bytes(signature),
    )
    .is_ok()
}

/// Хэш пароля для производных ключей: хэш пустой строки, если пароля нет.
pub(crate) fn password_hash(password: Option<&Password>) -> [u8; 32] {
    *blake3::hash(password.map_or(&[][..], Password::as_bytes)).as_bytes()
}

/// Доказательство знания пароля: keyed-MAC от транскрипта. Без пароля — нулевой блок.
pub(crate) fn password_proof(password: Option<&Password>, transcript: &[u8; 32]) -> [u8; 32] {
    match password {
        None => [0; 32],
        Some(password) => {
            let key = Zeroizing::new(blake3::derive_key(
                "RVE net v1 password proof",
                password.as_bytes(),
            ));
            *blake3::keyed_hash(&key, transcript).as_bytes()
        }
    }
}

/// Сессия отказа до аутентификации по паролю. Ключ зависит только от общего секрета и транскрипта,
/// поэтому клиент проверяет отказ сервера без пароля, а подделать его без общего секрета нельзя.
pub(crate) fn reject_session(shared: &[u8; 32], transcript: &[u8; 32]) -> Session {
    let mut material = Zeroizing::new([0u8; 64]);
    material[..32].copy_from_slice(shared);
    material[32..].copy_from_slice(transcript);
    let key = Zeroizing::new(blake3::derive_key("RVE net v1 reject", &material[..]));
    Session::new(&key, &key)
}

/// Направленные ключи сессии: клиент→сервер и сервер→клиент.
pub(crate) struct SessionKeys {
    pub client_to_server: Zeroizing<[u8; 32]>,
    pub server_to_client: Zeroizing<[u8; 32]>,
}

/// KDF: общий секрет ‖ транскрипт ‖ хэш пароля, поля фиксированной длины по 32 байта.
pub(crate) fn derive_keys(
    shared: &[u8; 32],
    transcript: &[u8; 32],
    password: &[u8; 32],
) -> SessionKeys {
    let mut material = Zeroizing::new([0u8; 96]);
    material[..32].copy_from_slice(shared);
    material[32..64].copy_from_slice(transcript);
    material[64..].copy_from_slice(password);
    SessionKeys {
        client_to_server: Zeroizing::new(blake3::derive_key("RVE net v1 c2s", &material[..])),
        server_to_client: Zeroizing::new(blake3::derive_key("RVE net v1 s2c", &material[..])),
    }
}

/// Сравнение за постоянное время; разная длина сразу даёт `false`.
pub(crate) fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// Сессия одного соединения: направленные ключи, счётчик отправки и окно повторов.
pub(crate) struct Session {
    send: ChaCha20Poly1305,
    recv: ChaCha20Poly1305,
    next_seq: u64,
    replay: ReplayWindow,
}

impl Session {
    pub(crate) fn new(send_key: &[u8; 32], recv_key: &[u8; 32]) -> Self {
        Self {
            send: cipher(send_key),
            recv: cipher(recv_key),
            next_seq: 0,
            replay: ReplayWindow::default(),
        }
    }

    /// Шифрует тело в датаграмму `Data`. Возвращает номер пакета и датаграмму.
    /// Заголовок и номер входят в AAD; `None`, если номера исчерпаны.
    pub(crate) fn seal(&mut self, conn_id: u32, body: &[u8]) -> Option<(u64, Vec<u8>)> {
        if self.next_seq == u64::MAX {
            return None;
        }
        let seq = self.next_seq;
        self.next_seq += 1;
        let mut datagram = Vec::with_capacity(HEADER_LEN + SEQ_LEN + body.len() + TAG_LEN);
        Header {
            kind: Kind::Data,
            conn_id,
        }
        .encode(&mut datagram);
        datagram.extend_from_slice(&seq.to_be_bytes());
        let aad_len = datagram.len();
        let ciphertext = self
            .send
            .encrypt(
                &nonce(seq),
                Payload {
                    msg: body,
                    aad: &datagram[..aad_len],
                },
            )
            .ok()?;
        datagram.extend_from_slice(&ciphertext);
        Some((seq, datagram))
    }

    /// Расшифровывает датаграмму `Data`. `None` — подделка, повтор или устаревший номер.
    pub(crate) fn open(&mut self, datagram: &[u8]) -> Option<(u64, Vec<u8>)> {
        let aad_len = HEADER_LEN + SEQ_LEN;
        if datagram.len() < aad_len + TAG_LEN {
            return None;
        }
        let seq = u64::from_be_bytes(datagram[HEADER_LEN..aad_len].try_into().ok()?);
        let body = self
            .recv
            .decrypt(
                &nonce(seq),
                Payload {
                    msg: &datagram[aad_len..],
                    aad: &datagram[..aad_len],
                },
            )
            .ok()?;
        // Окно обновляется только после проверки подписи тега.
        self.replay.accept(seq).then_some((seq, body))
    }
}

/// Шифр с копией ключа, которая обнуляется сразу после передачи в шифр.
fn cipher(key: &[u8; 32]) -> ChaCha20Poly1305 {
    let mut copy = *key;
    let cipher = ChaCha20Poly1305::new(&Key::from(copy));
    copy.zeroize();
    cipher
}

fn nonce(seq: u64) -> Nonce {
    let mut bytes = [0u8; 12];
    bytes[4..].copy_from_slice(&seq.to_be_bytes());
    Nonce::from(bytes)
}

/// Окно защиты от повторов: помнит последние `REPLAY_WINDOW` номеров пакетов.
#[derive(Default)]
struct ReplayWindow {
    highest: Option<u64>,
    /// Бит `d` означает, что пакет `highest - d` уже принят.
    seen: [u64; REPLAY_WORDS],
}

impl ReplayWindow {
    fn accept(&mut self, seq: u64) -> bool {
        let Some(highest) = self.highest else {
            self.highest = Some(seq);
            self.seen = [0; REPLAY_WORDS];
            self.seen[0] = 1;
            return true;
        };
        if seq > highest {
            self.shift(seq - highest);
            self.seen[0] |= 1;
            self.highest = Some(seq);
            return true;
        }
        let distance = highest - seq;
        if distance >= REPLAY_WINDOW {
            return false;
        }
        let word = (distance / 64) as usize;
        let bit = 1u64 << (distance % 64);
        if self.seen[word] & bit != 0 {
            return false;
        }
        self.seen[word] |= bit;
        true
    }

    /// Сдвигает маску на `by` позиций к старым номерам.
    fn shift(&mut self, by: u64) {
        if by >= REPLAY_WINDOW {
            self.seen = [0; REPLAY_WORDS];
            return;
        }
        let words = (by / 64) as usize;
        let bits = (by % 64) as u32;
        let old = self.seen;
        self.seen = std::array::from_fn(|i| {
            if i < words {
                return 0;
            }
            let src = i - words;
            let mut value = old[src] << bits;
            if bits > 0 && src > 0 {
                value |= old[src - 1] >> (64 - bits);
            }
            value
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn keys() -> (SessionKeys, [u8; 32]) {
        let transcript = [3u8; 32];
        let password = password_hash(None);
        let keys = derive_keys(&[9u8; 32], &transcript, &password);
        (keys, transcript)
    }

    fn pair() -> (Session, Session) {
        let (keys, _) = keys();
        let client = Session::new(&keys.client_to_server, &keys.server_to_client);
        let server = Session::new(&keys.server_to_client, &keys.client_to_server);
        (client, server)
    }

    #[test]
    fn sealed_datagram_opens_once_and_rejects_replay() {
        let (mut client, mut server) = pair();
        let (seq, datagram) = client.seal(42, b"hello").unwrap();
        assert_eq!(seq, 0);
        assert_eq!(server.open(&datagram), Some((0, b"hello".to_vec())));
        assert_eq!(server.open(&datagram), None, "повтор отброшен");
    }

    #[test]
    fn tampered_header_body_or_tag_is_rejected() {
        let (mut client, mut server) = pair();
        let (_, datagram) = client.seal(42, b"payload").unwrap();

        let mut header = datagram.clone();
        header[HEADER_LEN - 1] ^= 1;
        assert_eq!(server.open(&header), None, "изменён conn_id");

        let mut body = datagram.clone();
        let last = body.len() - 1;
        body[last] ^= 0x80;
        assert_eq!(server.open(&body), None, "изменён тег");

        let mut seq = datagram.clone();
        seq[HEADER_LEN] ^= 1;
        assert_eq!(server.open(&seq), None, "изменён номер");

        assert!(
            server.open(&datagram).is_some(),
            "подлинная датаграмма принимается"
        );
    }

    #[test]
    fn different_keys_do_not_open_each_others_traffic() {
        let (mut client, _) = pair();
        let other = Session::new(&[1u8; 32], &[2u8; 32]);
        let (_, datagram) = client.seal(1, b"x").unwrap();
        let mut other = other;
        assert_eq!(other.open(&datagram), None);
    }

    #[test]
    fn different_passwords_derive_different_keys() {
        let transcript = [3u8; 32];
        let shared = [9u8; 32];
        let none = derive_keys(&shared, &transcript, &password_hash(None));
        let pass = derive_keys(
            &shared,
            &transcript,
            &password_hash(Some(&Password::new("секрет"))),
        );
        assert_ne!(*none.client_to_server, *pass.client_to_server);
        assert_ne!(*none.server_to_client, *pass.server_to_client);
        assert_ne!(*none.client_to_server, *none.server_to_client);
    }

    #[test]
    fn degenerate_x25519_point_is_rejected() {
        let key = EphemeralKey::random().unwrap();
        assert!(key.agree(&[0u8; 32]).is_none(), "нулевая точка");
        let peer = EphemeralKey::random().unwrap();
        assert!(key.agree(&peer.public()).is_some());
    }

    #[test]
    fn proof_depends_on_password_and_transcript() {
        let transcript = [5u8; 32];
        let good = password_proof(Some(&Password::new("a")), &transcript);
        assert_eq!(good, password_proof(Some(&Password::new("a")), &transcript));
        assert_ne!(good, password_proof(Some(&Password::new("b")), &transcript));
        assert_ne!(good, password_proof(Some(&Password::new("a")), &[6u8; 32]));
        assert_eq!(password_proof(None, &transcript), [0u8; 32]);
    }

    #[test]
    fn constant_time_compare_matches_plain_equality() {
        assert!(ct_eq(b"abc", b"abc"));
        assert!(!ct_eq(b"abc", b"abd"));
        assert!(!ct_eq(b"abc", b"ab"));
    }

    #[test]
    fn replay_window_accepts_reordered_packets_once_and_forgets_old_ones() {
        let mut window = ReplayWindow::default();
        assert!(window.accept(10));
        assert!(window.accept(5), "пропущенный номер в окне");
        assert!(!window.accept(5), "повтор");
        assert!(window.accept(200));
        assert!(!window.accept(10), "повтор самого старшего номера");
        assert!(window.accept(11));
        assert!(window.accept(10 + REPLAY_WINDOW + 500));
        assert!(!window.accept(11), "вне окна");
    }

    #[test]
    fn replay_window_shift_keeps_bits_across_word_boundaries() {
        let mut window = ReplayWindow::default();
        assert!(window.accept(0));
        assert!(window.accept(70));
        assert!(!window.accept(0), "бит 70 позиций назад сохранён");
        assert!(!window.accept(70));
        assert!(window.accept(69));
    }

    #[test]
    fn server_identity_signs_and_fingerprint_matches_public_key() {
        let identity = ServerIdentity::ephemeral().unwrap();
        let transcript = [4u8; 32];
        let signature = identity.sign(&transcript);
        assert!(verify_accept(
            &identity.public_key(),
            &transcript,
            &signature
        ));
        assert!(!verify_accept(
            &identity.public_key(),
            &[5u8; 32],
            &signature
        ));
        assert_eq!(
            identity.fingerprint(),
            fingerprint_of(&identity.public_key())
        );
        assert_eq!(identity.fingerprint_hex().len(), 64);
    }

    #[test]
    fn identity_file_is_created_once_and_reloaded() {
        let dir = std::env::temp_dir().join(format!("rve-net-key-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("server.key");
        let _ = std::fs::remove_file(&path);

        let created = ServerIdentity::load_or_create(&path).unwrap();
        let reloaded = ServerIdentity::load_or_create(&path).unwrap();
        assert_eq!(created.public_key(), reloaded.public_key());
        assert_eq!(std::fs::read(&path).unwrap().len(), 32);

        std::fs::write(&path, [1u8; 5]).unwrap();
        assert!(matches!(
            ServerIdentity::load_or_create(&path),
            Err(NetError::Config(_))
        ));
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn scratch_dir(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("rve-net-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn entries(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(dir)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    #[test]
    fn key_publication_never_overwrites_and_leaves_no_temp_file() {
        let dir = scratch_dir("publish");
        let path = dir.join("server.key");
        std::fs::write(&path, b"existing").unwrap();
        let err = publish_new_file(&path, &[7u8; 32]).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::AlreadyExists);
        assert_eq!(std::fs::read(&path).unwrap(), b"existing");
        assert_eq!(entries(&dir), vec!["server.key".to_owned()]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Регрессия: файловая система без hard link (например, FAT) не должна мешать созданию ключа.
    #[test]
    fn key_is_written_directly_when_hard_links_are_unsupported() {
        let dir = scratch_dir("fallback");
        let path = dir.join("server.key");
        let unsupported =
            |_: &Path, _: &Path| Err(std::io::Error::from(std::io::ErrorKind::Unsupported));
        publish_with(&path, &[9u8; 32], unsupported).unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), [9u8; 32]);
        assert_eq!(entries(&dir), vec!["server.key".to_owned()]);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Запасной путь не перезаписывает существующий ключ и не оставляет временных файлов.
    #[test]
    fn fallback_never_overwrites_an_existing_key() {
        let dir = scratch_dir("fallback-existing");
        let path = dir.join("server.key");
        std::fs::write(&path, b"existing").unwrap();
        let denied =
            |_: &Path, _: &Path| Err(std::io::Error::from(std::io::ErrorKind::PermissionDenied));
        let err = publish_with(&path, &[9u8; 32], denied).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::AlreadyExists);
        assert_eq!(std::fs::read(&path).unwrap(), b"existing");
        assert_eq!(entries(&dir), vec!["server.key".to_owned()]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Уже существующий ключ при публикации не переходит на запасной путь: файл не создаётся.
    #[test]
    fn existing_key_is_reported_without_fallback() {
        let dir = scratch_dir("exists");
        let path = dir.join("server.key");
        let exists =
            |_: &Path, _: &Path| Err(std::io::Error::from(std::io::ErrorKind::AlreadyExists));
        let err = publish_with(&path, &[9u8; 32], exists).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::AlreadyExists);
        assert!(!path.exists(), "запасной путь не должен был писать ключ");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Устаревшие временные файлы ключа удаляются при старте; свежие и чужие файлы остаются.
    #[test]
    fn stale_key_temp_files_are_removed_and_fresh_ones_kept() {
        let dir = scratch_dir("stale");
        let path = dir.join("server.key");
        let now = SystemTime::now();
        let old_age = now - Duration::from_secs(7200);
        let stale = dir.join(".server.key.0123456789abcdef.tmp");
        let fresh = dir.join(".server.key.fedcba9876543210.tmp");
        let other = dir.join(".other.key.0123456789abcdef.tmp");
        for file in [&stale, &fresh, &other] {
            std::fs::write(file, b"partial").unwrap();
        }
        for file in [&stale, &other] {
            std::fs::File::open(file)
                .unwrap()
                .set_modified(old_age)
                .unwrap();
        }
        remove_stale_temps(&path, now);
        let mut expected = vec![
            ".other.key.0123456789abcdef.tmp".to_owned(),
            ".server.key.fedcba9876543210.tmp".to_owned(),
        ];
        expected.sort();
        assert_eq!(entries(&dir), expected);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn created_key_is_the_only_file_and_is_private() {
        let dir = scratch_dir("create");
        let path = dir.join("server.key");
        let created = ServerIdentity::load_or_create(&path).unwrap();
        assert_eq!(entries(&dir), vec!["server.key".to_owned()]);
        assert_eq!(
            ServerIdentity::load_or_create(&path).unwrap().public_key(),
            created.public_key()
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn group_or_world_access_to_the_key_is_detected() {
        assert!(!has_group_or_world_access(0o100600));
        assert!(has_group_or_world_access(0o100640), "группа");
        assert!(has_group_or_world_access(0o100604), "остальные");
    }

    #[test]
    fn reject_session_depends_on_shared_secret_and_transcript_only() {
        let mut ours = reject_session(&[1u8; 32], &[2u8; 32]);
        let mut same = reject_session(&[1u8; 32], &[2u8; 32]);
        let (_, datagram) = ours.seal(5, b"no").unwrap();
        assert!(same.open(&datagram).is_some());
        let mut other = reject_session(&[1u8; 32], &[3u8; 32]);
        assert!(other.open(&datagram).is_none(), "другой транскрипт");
    }
}
