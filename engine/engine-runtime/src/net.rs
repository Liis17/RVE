//! Сетевой режим runtime: роль по флагам и настройкам проекта, мост с `ScriptHost`.
//!
//! Без сетевых флагов поток `rve-network` не создаётся и мост на scripts thread не подключается.

use crate::cli::NetworkArgs;
use engine_network::{
    BridgeConfig, NetError, NetRole, NetworkBridge, NetworkConfig, NetworkHandle, Password,
    ServerIdentity, SpawnedRoot, WorldSink,
};
use engine_project::ProjectSettings;
use engine_scene::network::{NetEvent, NetId, NetMode, PeerId};
use engine_scene::{ObjectHandle, SpawnOptions, World};
use engine_script::ScriptHost;
use std::net::SocketAddr;
use std::time::{Duration, Instant};

/// Переменная окружения с паролем комнаты. Значение не попадает в логи и ошибки.
const PASSWORD_ENV: &str = "RVE_NET_PASSWORD";

/// Настройки сетевого потока, готовые к `NetworkEngine::new`.
pub(crate) struct NetworkPlan {
    pub config: NetworkConfig,
    pub bridge: BridgeConfig,
}

/// Уже открытый сетевой поток и режим моста на scripts thread.
pub(crate) struct NetworkSession {
    pub handle: NetworkHandle,
    pub bridge: BridgeConfig,
}

/// Мост, которым scripts thread обменивается сетевыми сообщениями и объектами.
pub(crate) type ScriptNetwork = NetworkBridge<NetworkHandle>;

/// Идентификатор протокола: `blake3(длина id ‖ id ‖ version)`. Длина перед `id` не даёт разным парам
/// (`"ab"`, `"c"`) и (`"a"`, `"bc"`) совпасть.
pub(crate) fn protocol_id(id: &str, version: &str) -> [u8; 32] {
    let mut hasher = blake3::Hasher::new();
    hasher.update(&(id.len() as u64).to_le_bytes());
    hasher.update(id.as_bytes());
    hasher.update(version.as_bytes());
    *hasher.finalize().as_bytes()
}

/// Строит настройки сети по флагам и `[network]` проекта. Файлы ключа и пароля читаются здесь;
/// сокет не открывается.
pub(crate) fn prepare(
    args: &NetworkArgs,
    project: &ProjectSettings,
    scene_hash: [u8; 32],
    headless: bool,
) -> Result<NetworkPlan, String> {
    let settings = &project.network;
    if !settings.enabled {
        return Err(
            "сетевые флаги требуют [network].enabled = true в project.rveproject".to_owned(),
        );
    }
    let password = password(args)?;
    let (role, mode) = match args.connect {
        Some(server_addr) => (
            NetRole::Client {
                server_addr,
                fingerprint: args.fingerprint,
                insecure_no_pin: args.insecure_no_pin,
                password,
            },
            NetMode::Client,
        ),
        None => {
            let bind = args
                .host_addr
                .unwrap_or_else(|| SocketAddr::from(([0, 0, 0, 0], settings.port)));
            let identity = match &args.server_key {
                Some(path) => {
                    ServerIdentity::load_or_create(path).map_err(|error| error.to_string())?
                }
                None => ServerIdentity::ephemeral().map_err(|error| error.to_string())?,
            };
            let mode = if headless {
                NetMode::Server
            } else {
                NetMode::Host
            };
            (
                NetRole::Server {
                    bind,
                    max_clients: settings.max_clients as usize,
                    identity,
                    password,
                },
                mode,
            )
        }
    };
    Ok(NetworkPlan {
        config: NetworkConfig {
            role,
            protocol_id: protocol_id(&project.id, &project.version),
            scene_hash,
            timeout: Duration::from_millis(u64::from(settings.timeout_ms)),
        },
        bridge: BridgeConfig {
            mode,
            tick_hz: settings.tick_hz,
        },
    })
}

/// Пароль из `--password-file`; без файла — из `RVE_NET_PASSWORD`. Файл приоритетнее.
fn password(args: &NetworkArgs) -> Result<Option<Password>, String> {
    if let Some(path) = &args.password_file {
        let bytes = std::fs::read(path)
            .map_err(|error| format!("файл пароля {}: {error}", path.display()))?;
        return password_from_bytes(bytes, "файл пароля").map(Some);
    }
    match std::env::var_os(PASSWORD_ENV) {
        Some(value) => password_from_bytes(value.into_encoded_bytes(), PASSWORD_ENV).map(Some),
        None => Ok(None),
    }
}

/// Убирает переводы строки в конце; пустой пароль — ошибка, а не отсутствие пароля.
fn password_from_bytes(mut bytes: Vec<u8>, source: &str) -> Result<Password, String> {
    while matches!(bytes.last(), Some(b'\n' | b'\r')) {
        bytes.pop();
    }
    if bytes.is_empty() {
        return Err(format!("{source}: пароль пуст"));
    }
    Ok(Password::new(bytes))
}

/// Операции над миром, которые мост выполняет через `ScriptHost`: spawn, despawn и события идут
/// тем же путём, что у скриптов.
pub(crate) struct ScriptNetSink<'a> {
    scripts: &'a mut ScriptHost,
}

impl<'a> ScriptNetSink<'a> {
    pub fn new(scripts: &'a mut ScriptHost) -> Self {
        Self { scripts }
    }
}

impl WorldSink for ScriptNetSink<'_> {
    fn set_network_state(&mut self, mode: NetMode, local_peer: PeerId, peers: Vec<PeerId>) {
        self.scripts.set_network_state(mode, local_peer, peers);
    }

    fn set_object_net(&mut self, object: ObjectHandle, net_id: NetId, owner: Option<PeerId>) {
        self.scripts.set_object_net(object, net_id, owner);
    }

    fn spawn(
        &mut self,
        world: &mut World,
        prefab: &str,
        options: SpawnOptions,
    ) -> Result<ObjectHandle, String> {
        self.scripts.spawn_external(world, prefab, options)
    }

    fn despawn(&mut self, world: &mut World, root: ObjectHandle) -> bool {
        self.scripts.despawn_external(world, root)
    }

    fn queue_events(&mut self, world: &World, events: Vec<NetEvent>) {
        self.scripts.queue_network_events(world, events);
    }

    fn drain_peer_faults(&mut self) -> Vec<(PeerId, u32)> {
        self.scripts.drain_peer_faults()
    }
}

/// Подключает мост к миру. Вызывается до `ScriptHost::start`, чтобы `on_start` видел режим сети.
pub(crate) fn attach(
    session: NetworkSession,
    world: &mut World,
    scripts: &mut ScriptHost,
) -> Result<ScriptNetwork, NetError> {
    NetworkBridge::attach(
        session.handle,
        session.bridge,
        world,
        &mut ScriptNetSink::new(scripts),
        Instant::now(),
    )
}

/// Входящие сообщения и события сети до `ScriptHost::update`.
pub(crate) fn receive(
    network: &mut Option<ScriptNetwork>,
    world: &mut World,
    scripts: &mut ScriptHost,
) {
    if let Some(bridge) = network.as_mut() {
        bridge.receive(world, &mut ScriptNetSink::new(scripts), Instant::now());
    }
}

/// Запросы скриптов (`rve.net`, методы объектов) после `update`, до применения spawn и despawn.
pub(crate) fn apply(
    network: &mut Option<ScriptNetwork>,
    world: &mut World,
    scripts: &mut ScriptHost,
) {
    if let Some(bridge) = network.as_mut() {
        let requests = scripts.drain_network_requests();
        bridge.apply(requests, world, &mut ScriptNetSink::new(scripts));
    }
}

/// Шаг сети после применения spawn и despawn. `roots` — корни, созданные с прошлого шага; список
/// очищается всегда, даже без сети.
pub(crate) fn tick(
    network: &mut Option<ScriptNetwork>,
    world: &mut World,
    scripts: &mut ScriptHost,
    roots: &mut Vec<SpawnedRoot>,
) {
    if let Some(bridge) = network.as_mut() {
        bridge.tick(
            world,
            &mut ScriptNetSink::new(scripts),
            roots,
            Instant::now(),
        );
    }
    roots.clear();
}

#[cfg(test)]
mod tests {
    use super::{NetworkArgs, password_from_bytes, prepare, protocol_id};
    use engine_network::NetRole;
    use engine_project::ProjectSettings;
    use engine_scene::network::NetMode;
    use std::net::SocketAddr;
    use std::path::PathBuf;

    const PROJECT: &str = "id = \"demo\"\nname = \"Demo\"\nversion = \"1.2.0\"\n[[scenes]]\npath = \"start.rvescene\"\n";

    fn project(network: &str) -> ProjectSettings {
        ProjectSettings::parse(&format!("{PROJECT}{network}")).unwrap()
    }

    fn address(text: &str) -> SocketAddr {
        text.parse().unwrap()
    }

    #[test]
    fn protocol_id_depends_on_project_id_and_version_without_ambiguity() {
        assert_eq!(protocol_id("demo", "1"), protocol_id("demo", "1"));
        assert_ne!(protocol_id("demo", "1"), protocol_id("demo", "2"));
        assert_ne!(protocol_id("demo", "1"), protocol_id("other", "1"));
        assert_ne!(protocol_id("ab", "c"), protocol_id("a", "bc"));
    }

    #[test]
    fn refuses_network_flags_when_project_disables_network() {
        let args = NetworkArgs {
            host: true,
            ..NetworkArgs::default()
        };
        let error = prepare(&args, &project(""), [0; 32], false)
            .err()
            .expect("сеть выключена проектом");
        assert!(error.contains("[network].enabled"), "{error}");
    }

    #[test]
    fn host_uses_project_port_and_tick_and_mode_follows_headless() {
        let settings =
            project("[network]\nenabled = true\nport = 9100\ntick_hz = 20\nmax_clients = 4\n");
        let args = NetworkArgs {
            host: true,
            ..NetworkArgs::default()
        };
        let player = prepare(&args, &settings, [7; 32], false).unwrap();
        assert_eq!(player.bridge.mode, NetMode::Host);
        assert_eq!(player.bridge.tick_hz, 20);
        assert_eq!(player.config.scene_hash, [7; 32]);
        assert_eq!(player.config.protocol_id, protocol_id("demo", "1.2.0"));
        match player.config.role {
            NetRole::Server {
                bind,
                max_clients,
                password,
                ..
            } => {
                assert_eq!(bind, address("0.0.0.0:9100"));
                assert_eq!(max_clients, 4);
                assert!(password.is_none());
            }
            NetRole::Client { .. } => panic!("host должен быть сервером"),
        }

        let server = prepare(&args, &settings, [0; 32], true).unwrap();
        assert_eq!(server.bridge.mode, NetMode::Server);
    }

    #[test]
    fn host_address_overrides_project_port() {
        let settings = project("[network]\nenabled = true\n");
        let args = NetworkArgs {
            host: true,
            host_addr: Some(address("127.0.0.1:7000")),
            ..NetworkArgs::default()
        };
        match prepare(&args, &settings, [0; 32], false)
            .unwrap()
            .config
            .role
        {
            NetRole::Server { bind, .. } => assert_eq!(bind, address("127.0.0.1:7000")),
            NetRole::Client { .. } => panic!("host должен быть сервером"),
        }
    }

    #[test]
    fn connect_builds_client_with_pin_and_client_mode() {
        let settings = project("[network]\nenabled = true\n");
        let args = NetworkArgs {
            connect: Some(address("127.0.0.1:7777")),
            fingerprint: Some([3; 32]),
            ..NetworkArgs::default()
        };
        let plan = prepare(&args, &settings, [0; 32], false).unwrap();
        assert_eq!(plan.bridge.mode, NetMode::Client);
        match plan.config.role {
            NetRole::Client {
                server_addr,
                fingerprint,
                insecure_no_pin,
                ..
            } => {
                assert_eq!(server_addr, address("127.0.0.1:7777"));
                assert_eq!(fingerprint, Some([3; 32]));
                assert!(!insecure_no_pin);
            }
            NetRole::Server { .. } => panic!("connect должен быть клиентом"),
        }
    }

    #[test]
    fn missing_password_file_is_reported_with_its_path() {
        let settings = project("[network]\nenabled = true\n");
        let args = NetworkArgs {
            host: true,
            password_file: Some(PathBuf::from("/nonexistent/rve-test-password")),
            ..NetworkArgs::default()
        };
        let error = prepare(&args, &settings, [0; 32], false).err().unwrap();
        assert!(error.contains("rve-test-password"), "{error}");
    }

    #[test]
    fn password_drops_trailing_newlines_and_rejects_empty_values() {
        assert!(password_from_bytes(b"secret\r\n".to_vec(), "файл").is_ok());
        assert!(password_from_bytes(b"\n".to_vec(), "файл").is_err());
        let error = password_from_bytes(Vec::new(), "RVE_NET_PASSWORD")
            .err()
            .unwrap();
        assert_eq!(error, "RVE_NET_PASSWORD: пароль пуст");
    }
}
