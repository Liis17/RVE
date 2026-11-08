//! Сетевой Play редактора: выбор Offline, Host или Client, проверка параметров и мост к миру Play.
//!
//! Введённые поля и пароль живут только в `EditorState`: в настройки пользователя, layout и проект
//! они не попадают. Ключ сервера в Play эфемерный, его отпечаток выводится в Console.

use engine_network::{
    BridgeConfig, NetRole, NetworkBridge, NetworkConfig, NetworkEngine, NetworkHandle, Password,
    ServerIdentity, SpawnedRoot, WorldSink,
};
use engine_project::NetworkSettings;
use engine_scene::network::{NetEvent, NetId, NetMode, PeerId};
use engine_scene::{ObjectHandle, SpawnOptions, World};
use engine_script::ScriptHost;
use std::fmt;
use std::net::SocketAddr;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

/// Подсказка для Host и Client, когда проект выключил сеть.
pub const NETWORK_DISABLED_HINT: &str = "Задайте [network].enabled = true в project.rveproject";

/// Режим сети, выбранный для следующего Play.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum PlayNetworkMode {
    #[default]
    Offline,
    Host,
    Client,
}

impl PlayNetworkMode {
    pub const ALL: [Self; 3] = [Self::Offline, Self::Host, Self::Client];

    pub fn label(self) -> &'static str {
        match self {
            Self::Offline => "Offline",
            Self::Host => "Host",
            Self::Client => "Client",
        }
    }
}

/// Параметры сетевого Play, как их ввёл пользователь. Пустое поле порта означает порт проекта.
#[derive(Default)]
pub struct PlayNetworkForm {
    pub mode: PlayNetworkMode,
    pub port: String,
    /// Адрес сервера для Client, `адрес:порт`.
    pub address: String,
    /// Отпечаток сервера для Client: 64 hex-символа. Пусто — отпечаток не закреплён.
    pub fingerprint: String,
    /// Подключение без закреплённого отпечатка; взаимоисключает `fingerprint`.
    pub insecure_no_pin: bool,
    /// Пароль комнаты; пусто — без пароля.
    pub password: String,
}

impl fmt::Debug for PlayNetworkForm {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PlayNetworkForm")
            .field("mode", &self.mode)
            .field("port", &self.port)
            .field("address", &self.address)
            .field("fingerprint", &self.fingerprint)
            .field("insecure_no_pin", &self.insecure_no_pin)
            .field("password", &"***")
            .finish()
    }
}

/// Проверенные настройки сети для одного запуска Play.
pub struct PlayNetworkPlan {
    config: NetworkConfig,
    bridge: BridgeConfig,
    /// Строка для Console: режим, порт или адрес и для Host — отпечаток ключа сервера.
    pub summary: String,
}

impl PlayNetworkForm {
    /// Проверяет параметры и готовит настройки потока. `Offline` не требует ничего и даёт `None`.
    /// Ошибки возвращаются до любого открытия сокета.
    pub fn plan(
        &self,
        settings: &NetworkSettings,
        protocol_id: [u8; 32],
        scene_hash: [u8; 32],
    ) -> Result<Option<PlayNetworkPlan>, String> {
        match self.mode {
            PlayNetworkMode::Offline => Ok(None),
            _ if !settings.enabled => Err(NETWORK_DISABLED_HINT.to_owned()),
            PlayNetworkMode::Host => self.host_plan(settings, protocol_id, scene_hash).map(Some),
            PlayNetworkMode::Client => self
                .client_plan(settings, protocol_id, scene_hash)
                .map(Some),
        }
    }

    fn host_plan(
        &self,
        settings: &NetworkSettings,
        protocol_id: [u8; 32],
        scene_hash: [u8; 32],
    ) -> Result<PlayNetworkPlan, String> {
        let port = parse_port(&self.port, settings.port)?;
        let identity = ServerIdentity::ephemeral().map_err(|error| error.to_string())?;
        let summary = format!(
            "Сеть: Host, порт {port}, отпечаток ключа сервера {}",
            identity.fingerprint_hex()
        );
        Ok(PlayNetworkPlan {
            config: NetworkConfig {
                role: NetRole::Server {
                    bind: SocketAddr::from(([0, 0, 0, 0], port)),
                    max_clients: settings.max_clients as usize,
                    identity,
                    password: self.password(),
                },
                protocol_id,
                scene_hash,
                timeout: Duration::from_millis(u64::from(settings.timeout_ms)),
            },
            bridge: BridgeConfig {
                mode: NetMode::Host,
                tick_hz: settings.tick_hz,
            },
            summary,
        })
    }

    fn client_plan(
        &self,
        settings: &NetworkSettings,
        protocol_id: [u8; 32],
        scene_hash: [u8; 32],
    ) -> Result<PlayNetworkPlan, String> {
        let server_addr = self.address.trim().parse::<SocketAddr>().map_err(|_| {
            "адрес сервера должен быть в виде адрес:порт, например 127.0.0.1:7777".to_owned()
        })?;
        let fingerprint = match self.fingerprint.trim() {
            "" => None,
            text => Some(
                engine_network::parse_fingerprint(text)
                    .ok_or_else(|| "отпечаток должен содержать 64 hex-символа".to_owned())?,
            ),
        };
        match (fingerprint.is_some(), self.insecure_no_pin) {
            (true, true) => return Err("отпечаток и insecure-no-pin взаимоисключающие".to_owned()),
            (false, false) => {
                return Err("укажите отпечаток сервера или включите insecure-no-pin".to_owned());
            }
            _ => {}
        }
        Ok(PlayNetworkPlan {
            config: NetworkConfig {
                role: NetRole::Client {
                    server_addr,
                    fingerprint,
                    insecure_no_pin: self.insecure_no_pin,
                    password: self.password(),
                },
                protocol_id,
                scene_hash,
                timeout: Duration::from_millis(u64::from(settings.timeout_ms)),
            },
            bridge: BridgeConfig {
                mode: NetMode::Client,
                tick_hz: settings.tick_hz,
            },
            summary: format!("Сеть: Client к {server_addr}"),
        })
    }

    fn password(&self) -> Option<Password> {
        (!self.password.is_empty()).then(|| Password::new(self.password.as_bytes().to_vec()))
    }
}

/// Порт Host: пустое поле — порт из `[network]`, иначе число в диапазоне 1..=65535.
fn parse_port(text: &str, default: u16) -> Result<u16, String> {
    let text = text.trim();
    if text.is_empty() {
        return Ok(default);
    }
    text.parse::<u16>()
        .ok()
        .filter(|port| *port != 0)
        .ok_or_else(|| "порт должен быть в диапазоне 1..=65535".to_owned())
}

/// Поток `rve-network` и мост Play. Поток принадлежит сессии: `shutdown` завершает его и ждёт выхода.
pub struct PlayNetwork {
    bridge: NetworkBridge<NetworkHandle>,
    handle: NetworkHandle,
    thread: Option<JoinHandle<()>>,
}

impl PlayNetwork {
    /// Открывает сокет и подключает мост к миру и скриптам, затем запускает поток. Ошибка сокета
    /// возвращается до запуска потока. Вызывается до первого `update`, чтобы `on_start` видел режим.
    pub fn start(
        plan: PlayNetworkPlan,
        world: &mut World,
        scripts: &mut ScriptHost,
    ) -> Result<Self, String> {
        let (engine, handle) =
            NetworkEngine::new(plan.config).map_err(|error| error.to_string())?;
        let bridge = NetworkBridge::attach(
            handle.clone(),
            plan.bridge,
            world,
            &mut ScriptNetSink::new(scripts),
            Instant::now(),
        )
        .map_err(|error| error.to_string())?;
        let thread = std::thread::Builder::new()
            .name("rve-network".to_owned())
            .spawn(move || engine.run())
            .map_err(|error| format!("не удалось запустить сетевой поток: {error}"))?;
        Ok(Self {
            bridge,
            handle,
            thread: Some(thread),
        })
    }

    /// Входящие сообщения и события до обновления скриптов.
    pub fn receive(&mut self, world: &mut World, scripts: &mut ScriptHost) {
        self.bridge
            .receive(world, &mut ScriptNetSink::new(scripts), Instant::now());
    }

    /// Запросы скриптов (`rve.net`, методы объектов) после обновления, до spawn и despawn.
    pub fn apply(&mut self, world: &mut World, scripts: &mut ScriptHost) {
        let requests = scripts.drain_network_requests();
        self.bridge
            .apply(requests, world, &mut ScriptNetSink::new(scripts));
    }

    /// Шаг сети после spawn и despawn; `roots` — корни, созданные с прошлого шага.
    pub fn tick(&mut self, world: &mut World, scripts: &mut ScriptHost, roots: &mut [SpawnedRoot]) {
        self.bridge.tick(
            world,
            &mut ScriptNetSink::new(scripts),
            roots,
            Instant::now(),
        );
    }

    /// Останавливает поток: он отправляет отключения и выходит. Повторный вызов безвреден.
    pub fn shutdown(&mut self) {
        self.handle.shutdown();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// Операции над миром, которые мост выполняет через `ScriptHost`: тот же путь, что у runtime.
struct ScriptNetSink<'a> {
    scripts: &'a mut ScriptHost,
}

impl<'a> ScriptNetSink<'a> {
    fn new(scripts: &'a mut ScriptHost) -> Self {
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

#[cfg(test)]
mod tests {
    use super::{PlayNetworkForm, PlayNetworkMode, parse_port};
    use engine_network::NetRole;
    use engine_project::NetworkSettings;
    use std::net::SocketAddr;

    const PROTOCOL: [u8; 32] = [9; 32];
    const SCENE: [u8; 32] = [7; 32];
    const FINGERPRINT: &str = "00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff";

    fn settings(enabled: bool) -> NetworkSettings {
        NetworkSettings {
            enabled,
            port: 9100,
            max_clients: 4,
            ..NetworkSettings::default()
        }
    }

    fn form(mode: PlayNetworkMode) -> PlayNetworkForm {
        PlayNetworkForm {
            mode,
            ..PlayNetworkForm::default()
        }
    }

    fn client(address: &str, fingerprint: &str, insecure_no_pin: bool) -> PlayNetworkForm {
        PlayNetworkForm {
            address: address.to_owned(),
            fingerprint: fingerprint.to_owned(),
            insecure_no_pin,
            ..form(PlayNetworkMode::Client)
        }
    }

    fn plan_error(form: &PlayNetworkForm, settings: &NetworkSettings) -> String {
        match form.plan(settings, PROTOCOL, SCENE) {
            Ok(_) => panic!("план должен быть отклонён"),
            Err(error) => error,
        }
    }

    #[test]
    fn offline_needs_no_network_settings_and_builds_no_plan() {
        let plan = form(PlayNetworkMode::Offline)
            .plan(&settings(false), PROTOCOL, SCENE)
            .unwrap();
        assert!(plan.is_none());
    }

    #[test]
    fn host_and_client_are_refused_while_the_project_disables_network() {
        for mode in [PlayNetworkMode::Host, PlayNetworkMode::Client] {
            let error = plan_error(&form(mode), &settings(false));
            assert!(error.contains("[network].enabled"), "{error}");
        }
    }

    #[test]
    fn host_uses_the_project_port_when_the_field_is_empty() {
        let plan = form(PlayNetworkMode::Host)
            .plan(&settings(true), PROTOCOL, SCENE)
            .unwrap()
            .unwrap();
        match plan.config.role {
            NetRole::Server {
                bind,
                max_clients,
                password,
                ..
            } => {
                assert_eq!(bind, SocketAddr::from(([0, 0, 0, 0], 9100)));
                assert_eq!(max_clients, 4);
                assert!(password.is_none());
            }
            NetRole::Client { .. } => panic!("Host должен быть сервером"),
        }
        assert_eq!(plan.config.protocol_id, PROTOCOL);
        assert_eq!(plan.config.scene_hash, SCENE);
        assert_eq!(plan.bridge.mode, engine_scene::network::NetMode::Host);
        assert_eq!(plan.bridge.tick_hz, 30);
    }

    #[test]
    fn host_port_field_overrides_the_project_and_prints_an_ephemeral_fingerprint() {
        let host = PlayNetworkForm {
            port: " 7000 ".to_owned(),
            ..form(PlayNetworkMode::Host)
        };
        let plan = host
            .plan(&settings(true), PROTOCOL, SCENE)
            .unwrap()
            .unwrap();
        match plan.config.role {
            NetRole::Server { bind, .. } => assert_eq!(bind.port(), 7000),
            NetRole::Client { .. } => panic!("Host должен быть сервером"),
        }
        let fingerprint = plan.summary.rsplit(' ').next().unwrap();
        assert_eq!(fingerprint.len(), 64, "{}", plan.summary);
        assert!(fingerprint.bytes().all(|byte| byte.is_ascii_hexdigit()));
    }

    #[test]
    fn host_port_must_be_a_non_zero_u16() {
        for text in ["0", "70000", "abc", "-1"] {
            assert!(parse_port(text, 7777).is_err(), "{text}");
        }
        assert_eq!(parse_port("", 7777), Ok(7777));
        assert_eq!(parse_port("65535", 7777), Ok(65535));
    }

    #[test]
    fn client_needs_an_address_in_socket_form() {
        let error = plan_error(&client("localhost", FINGERPRINT, false), &settings(true));
        assert!(error.contains("адрес:порт"), "{error}");
        let error = plan_error(&client("", FINGERPRINT, false), &settings(true));
        assert!(error.contains("адрес:порт"), "{error}");
    }

    #[test]
    fn client_needs_either_a_fingerprint_or_insecure_but_not_both() {
        let neither = plan_error(&client("127.0.0.1:7777", "", false), &settings(true));
        assert!(neither.contains("insecure-no-pin"), "{neither}");
        let both = plan_error(
            &client("127.0.0.1:7777", FINGERPRINT, true),
            &settings(true),
        );
        assert!(both.contains("взаимоисключающие"), "{both}");
    }

    #[test]
    fn client_fingerprint_must_be_64_hex_digits() {
        for text in [
            "00",
            &FINGERPRINT[..63],
            &format!("{FINGERPRINT}0"),
            &FINGERPRINT.replace('0', "g"),
        ] {
            let error = plan_error(&client("127.0.0.1:7777", text, false), &settings(true));
            assert!(error.contains("64 hex-символа"), "{text}: {error}");
        }
    }

    #[test]
    fn client_plan_pins_the_fingerprint_or_skips_the_pin_explicitly() {
        let pinned = client("127.0.0.1:7777", FINGERPRINT, false)
            .plan(&settings(true), PROTOCOL, SCENE)
            .unwrap()
            .unwrap();
        match pinned.config.role {
            NetRole::Client {
                server_addr,
                fingerprint,
                insecure_no_pin,
                password,
            } => {
                assert_eq!(server_addr, "127.0.0.1:7777".parse().unwrap());
                assert_eq!(fingerprint.map(|pin| pin[0]), Some(0x00));
                assert!(!insecure_no_pin);
                assert!(password.is_none());
            }
            NetRole::Server { .. } => panic!("Client должен быть клиентом"),
        }
        assert_eq!(pinned.bridge.mode, engine_scene::network::NetMode::Client);

        let insecure = client("127.0.0.1:7777", "", true)
            .plan(&settings(true), PROTOCOL, SCENE)
            .unwrap()
            .unwrap();
        match insecure.config.role {
            NetRole::Client {
                fingerprint,
                insecure_no_pin,
                ..
            } => {
                assert!(fingerprint.is_none());
                assert!(insecure_no_pin);
            }
            NetRole::Server { .. } => panic!("Client должен быть клиентом"),
        }
    }

    #[test]
    fn password_is_kept_only_when_set_and_never_shown_in_debug_output() {
        let secret = PlayNetworkForm {
            password: "hunter2".to_owned(),
            ..form(PlayNetworkMode::Host)
        };
        let plan = secret
            .plan(&settings(true), PROTOCOL, SCENE)
            .unwrap()
            .unwrap();
        match plan.config.role {
            NetRole::Server { password, .. } => assert!(password.is_some()),
            NetRole::Client { .. } => panic!("Host должен быть сервером"),
        }
        assert!(!format!("{secret:?}").contains("hunter2"));
        assert!(!plan.summary.contains("hunter2"));

        let open = form(PlayNetworkMode::Host)
            .plan(&settings(true), PROTOCOL, SCENE)
            .unwrap()
            .unwrap();
        match open.config.role {
            NetRole::Server { password, .. } => assert!(password.is_none()),
            NetRole::Client { .. } => panic!("Host должен быть сервером"),
        }
    }
}
