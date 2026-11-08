//! Датаграммный транспорт: трейт сокета, UDP-реализация и детерминированная сеть в памяти для тестов.

use std::collections::{HashMap, VecDeque};
use std::io;
use std::net::{SocketAddr, UdpSocket};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use crate::NetError;
use crate::packet::MAX_DATAGRAM;

/// Ожидание датаграммы в UDP-сокете: поток возвращается к командам примерно раз в 2 мс.
const RECV_TIMEOUT: Duration = Duration::from_millis(2);

/// Датаграммный сокет. `recv_from` ждёт не дольше своего таймаута и возвращает `None`,
/// если датаграммы нет или её размер недопустим.
pub trait DatagramSocket: Send {
    fn send_to(&mut self, addr: SocketAddr, data: &[u8]) -> io::Result<()>;
    fn recv_from(&mut self) -> io::Result<Option<(SocketAddr, Vec<u8>)>>;
}

/// Что делать с ошибкой приёма.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RecvAction {
    /// Ошибка относится к одной датаграмме или таймауту; приём продолжается.
    Ignore,
    /// Сокет не пригоден; поток останавливается.
    Fatal,
}

/// Классифицирует ошибку приёма. ICMP-сброс (`ConnectionReset`, на Windows `WSAECONNRESET`)
/// относится к чужой датаграмме, поэтому не останавливает сокет.
pub(crate) fn classify_recv_error(kind: io::ErrorKind, os_code: Option<i32>) -> RecvAction {
    const WSAECONNRESET: i32 = 10054;
    match kind {
        io::ErrorKind::WouldBlock
        | io::ErrorKind::TimedOut
        | io::ErrorKind::Interrupted
        | io::ErrorKind::ConnectionReset => RecvAction::Ignore,
        _ if os_code == Some(WSAECONNRESET) => RecvAction::Ignore,
        _ => RecvAction::Fatal,
    }
}

/// UDP-сокет на `std::net::UdpSocket`.
pub struct UdpTransport {
    socket: UdpSocket,
    buffer: Vec<u8>,
}

impl UdpTransport {
    pub fn bind(addr: SocketAddr) -> Result<Self, NetError> {
        let socket = UdpSocket::bind(addr)
            .map_err(|err| NetError::Io(format!("не удалось открыть {addr}: {err}")))?;
        socket
            .set_read_timeout(Some(RECV_TIMEOUT))
            .map_err(|err| NetError::Io(err.to_string()))?;
        Ok(Self {
            socket,
            // Лишний байт отличает допустимую датаграмму от слишком большой.
            buffer: vec![0; MAX_DATAGRAM + 1],
        })
    }
}

impl DatagramSocket for UdpTransport {
    fn send_to(&mut self, addr: SocketAddr, data: &[u8]) -> io::Result<()> {
        self.socket.send_to(data, addr).map(|_| ())
    }

    fn recv_from(&mut self) -> io::Result<Option<(SocketAddr, Vec<u8>)>> {
        match self.socket.recv_from(&mut self.buffer) {
            Ok((len, from)) if len <= MAX_DATAGRAM => Ok(Some((from, self.buffer[..len].to_vec()))),
            Ok(_) => Ok(None),
            Err(err) => match classify_recv_error(err.kind(), err.raw_os_error()) {
                RecvAction::Ignore => Ok(None),
                RecvAction::Fatal => Err(err),
            },
        }
    }
}

/// Искажения доставки в сети в памяти. Вероятности заданы в тысячных долях.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Faults {
    pub loss_permille: u32,
    pub duplicate_permille: u32,
    /// Вставлять датаграмму в случайное место очереди получателя вместо конца.
    pub reorder: bool,
}

struct NetworkState {
    inboxes: HashMap<SocketAddr, VecDeque<(SocketAddr, Vec<u8>)>>,
    faults: Faults,
    random: u64,
}

impl NetworkState {
    fn next_random(&mut self) -> u64 {
        // xorshift64: детерминированно для заданного seed.
        let mut x = self.random;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.random = x;
        x
    }

    fn roll(&mut self, permille: u32) -> bool {
        permille > 0 && self.next_random() % 1000 < u64::from(permille)
    }
}

/// Сеть в памяти: адреса с очередями, управляемые потери, дубли и перестановки.
#[derive(Clone)]
pub struct MemoryNetwork {
    state: Arc<Mutex<NetworkState>>,
}

impl MemoryNetwork {
    /// `seed` задаёт последовательность искажений; одинаковый seed даёт одинаковый результат.
    pub fn new(seed: u64, faults: Faults) -> Self {
        Self {
            state: Arc::new(Mutex::new(NetworkState {
                inboxes: HashMap::new(),
                faults,
                random: seed | 1,
            })),
        }
    }

    /// Регистрирует адрес и возвращает его сокет.
    pub fn endpoint(&self, addr: SocketAddr) -> MemoryEndpoint {
        self.lock().inboxes.entry(addr).or_default();
        MemoryEndpoint {
            network: self.clone(),
            addr,
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, NetworkState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn deliver(&self, from: SocketAddr, to: SocketAddr, data: &[u8]) {
        let mut state = self.lock();
        let faults = state.faults;
        if state.roll(faults.loss_permille) {
            return;
        }
        let copies = if state.roll(faults.duplicate_permille) {
            2
        } else {
            1
        };
        let reorder = faults.reorder;
        for _ in 0..copies {
            let slot = if reorder {
                let len = state.inboxes.get(&to).map_or(0, VecDeque::len);
                (state.next_random() % (len as u64 + 1)) as usize
            } else {
                usize::MAX
            };
            let Some(inbox) = state.inboxes.get_mut(&to) else {
                return;
            };
            let entry = (from, data.to_vec());
            if slot >= inbox.len() {
                inbox.push_back(entry);
            } else {
                inbox.insert(slot, entry);
            }
        }
    }

    fn take(&self, addr: SocketAddr) -> Option<(SocketAddr, Vec<u8>)> {
        self.lock().inboxes.get_mut(&addr)?.pop_front()
    }
}

/// Сокет в сети в памяти.
pub struct MemoryEndpoint {
    network: MemoryNetwork,
    addr: SocketAddr,
}

impl DatagramSocket for MemoryEndpoint {
    fn send_to(&mut self, addr: SocketAddr, data: &[u8]) -> io::Result<()> {
        self.network.deliver(self.addr, addr, data);
        Ok(())
    }

    fn recv_from(&mut self) -> io::Result<Option<(SocketAddr, Vec<u8>)>> {
        Ok(self.network.take(self.addr))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timeouts_and_resets_are_ignored_while_other_errors_stop_the_socket() {
        assert_eq!(
            classify_recv_error(io::ErrorKind::WouldBlock, None),
            RecvAction::Ignore
        );
        assert_eq!(
            classify_recv_error(io::ErrorKind::TimedOut, None),
            RecvAction::Ignore
        );
        assert_eq!(
            classify_recv_error(io::ErrorKind::ConnectionReset, None),
            RecvAction::Ignore
        );
        assert_eq!(
            classify_recv_error(io::ErrorKind::Other, Some(10054)),
            RecvAction::Ignore
        );
        assert_eq!(
            classify_recv_error(io::ErrorKind::PermissionDenied, None),
            RecvAction::Fatal
        );
        assert_eq!(
            classify_recv_error(io::ErrorKind::Other, Some(10013)),
            RecvAction::Fatal
        );
    }

    fn addr(port: u16) -> SocketAddr {
        SocketAddr::from(([127, 0, 0, 1], port))
    }

    #[test]
    fn memory_endpoints_deliver_in_order_without_faults() {
        let network = MemoryNetwork::new(1, Faults::default());
        let mut a = network.endpoint(addr(1));
        let mut b = network.endpoint(addr(2));
        a.send_to(addr(2), b"one").unwrap();
        a.send_to(addr(2), b"two").unwrap();
        assert_eq!(b.recv_from().unwrap(), Some((addr(1), b"one".to_vec())));
        assert_eq!(b.recv_from().unwrap(), Some((addr(1), b"two".to_vec())));
        assert_eq!(b.recv_from().unwrap(), None);
    }

    #[test]
    fn same_seed_gives_the_same_faults() {
        let faults = Faults {
            loss_permille: 300,
            duplicate_permille: 300,
            reorder: true,
        };
        let run = || {
            let network = MemoryNetwork::new(42, faults);
            let mut a = network.endpoint(addr(1));
            let mut b = network.endpoint(addr(2));
            for index in 0..50u8 {
                a.send_to(addr(2), &[index]).unwrap();
            }
            let mut received = Vec::new();
            while let Some((_, bytes)) = b.recv_from().unwrap() {
                received.push(bytes);
            }
            received
        };
        assert_eq!(run(), run());
    }

    #[test]
    fn datagrams_to_unknown_addresses_are_dropped() {
        let network = MemoryNetwork::new(1, Faults::default());
        let mut a = network.endpoint(addr(1));
        a.send_to(addr(9), b"lost").unwrap();
        assert_eq!(a.recv_from().unwrap(), None);
    }
}
