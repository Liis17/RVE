//! Сетевое состояние хоста скриптов: режим и участники, сетевые атрибуты объектов, лимиты входящих
//! сообщений по участнику и учёт ошибок net-callback.
//!
//! Транспорт и протокол находятся в `engine-network`. Мост на scripts thread заполняет это состояние
//! через [`ScriptHost`](crate::ScriptHost); скрипты читают его синхронно внутри вызова.

use crate::lite::EventQueue;
use crate::transaction::{MAX_MESSAGE_NAME_BYTES, MessageValueBudget};
use crate::types::{ScriptEvent, ScriptEventLevel};
use engine_scene::{NetId, NetMode, ObjectHandle, ParamValue, PeerId};
use std::collections::{HashMap, HashSet};

/// Наибольшее число сетевых запросов, которые скрипты ставят за кадр, включая уже зафиксированные.
pub(crate) const MAX_NET_REQUESTS_PER_FRAME: usize = 4096;
/// Входящих сообщений от одного участника, которые доставляются скриптам за один update.
pub(crate) const MAX_INBOUND_MESSAGES_PER_PEER: usize = 256;
/// Суммарный оценочный размер входящих сообщений одного участника за один update.
pub(crate) const MAX_INBOUND_BYTES_PER_PEER: usize = 512 * 1024;
/// Записей в журнал на один источник за секунду; остальные ошибки net-callback только считаются.
pub(crate) const MAX_NET_ERROR_LOGS_PER_SOURCE_PER_SECOND: u32 = 8;

/// Сетевой атрибут объекта, который заполняет мост: номер и текущий владелец.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct NetObjectState {
    pub(crate) net_id: NetId,
    pub(crate) owner: Option<PeerId>,
}

/// Режим, участники и атрибуты объектов. Общая с транзакциями через `Rc<RefCell<_>>`.
#[derive(Default)]
pub(crate) struct NetScriptState {
    mode: NetMode,
    local_peer: PeerId,
    peers: Vec<PeerId>,
    objects: HashMap<ObjectHandle, NetObjectState>,
}

impl NetScriptState {
    pub(crate) fn set_network(&mut self, mode: NetMode, local_peer: PeerId, peers: Vec<PeerId>) {
        self.mode = mode;
        self.local_peer = local_peer;
        self.peers = peers;
    }

    pub(crate) fn mode(&self) -> NetMode {
        self.mode
    }

    pub(crate) fn local_peer(&self) -> PeerId {
        self.local_peer
    }

    pub(crate) fn peers(&self) -> &[PeerId] {
        &self.peers
    }

    pub(crate) fn object(&self, handle: ObjectHandle) -> Option<NetObjectState> {
        self.objects.get(&handle).copied()
    }

    pub(crate) fn set_object(&mut self, handle: ObjectHandle, state: NetObjectState) {
        self.objects.insert(handle, state);
    }

    pub(crate) fn clear_object(&mut self, handle: ObjectHandle) {
        self.objects.remove(&handle);
    }
}

/// Бюджет входящих сообщений одного update. Счётчики сбрасываются, когда очередь уходит в скрипты.
#[derive(Default)]
pub(crate) struct InboundBudget {
    per_peer: HashMap<PeerId, (usize, usize)>,
    refused_peers: HashSet<PeerId>,
}

impl InboundBudget {
    /// Учитывает одно сообщение участника; `false`, если лимит количества или байт исчерпан.
    pub(crate) fn admit(&mut self, peer: PeerId, bytes: usize) -> bool {
        let (count, used) = self.per_peer.entry(peer).or_default();
        if *count >= MAX_INBOUND_MESSAGES_PER_PEER
            || used.saturating_add(bytes) > MAX_INBOUND_BYTES_PER_PEER
        {
            return false;
        }
        *count += 1;
        *used = used.saturating_add(bytes);
        true
    }

    /// `true` только при первом отказе участника за update: по нему пишется одно предупреждение.
    pub(crate) fn first_refusal(&mut self, peer: PeerId) -> bool {
        self.refused_peers.insert(peer)
    }

    pub(crate) fn reset(&mut self) {
        self.per_peer.clear();
        self.refused_peers.clear();
    }
}

/// Журнал ошибок net-callback с лимитом на источник в секунду. Сверх лимита ошибки только
/// считаются; при смене окна пишется одна сводная запись.
#[derive(Default)]
pub(crate) struct NetErrorLog {
    window_end: f64,
    per_source: HashMap<String, u32>,
    suppressed: u64,
}

impl NetErrorLog {
    /// Записывает ошибку `message` от `source` в момент `now` (секунды времени скриптов).
    pub(crate) fn report(&mut self, now: f64, source: &str, message: String, events: &EventQueue) {
        if now >= self.window_end {
            if self.suppressed > 0 {
                events.borrow_mut().push(ScriptEvent {
                    level: ScriptEventLevel::Error,
                    source: source.to_owned(),
                    message: format!(
                        "за прошлую секунду подавлено ошибок net-callback: {}",
                        self.suppressed
                    ),
                });
            }
            self.per_source.clear();
            self.suppressed = 0;
            self.window_end = now + 1.0;
        }
        let count = self.per_source.entry(source.to_owned()).or_insert(0);
        if *count < MAX_NET_ERROR_LOGS_PER_SOURCE_PER_SECOND {
            *count += 1;
            events.borrow_mut().push(ScriptEvent {
                level: ScriptEventLevel::Error,
                source: source.to_owned(),
                message,
            });
        } else {
            self.suppressed = self.suppressed.saturating_add(1);
        }
    }
}

/// Проверяет имя и значение сетевого сообщения по общим лимитам `rve.send` и возвращает оценочный
/// размер, который учитывает бюджет очереди.
pub(crate) fn validate_message(name: &str, value: &ParamValue) -> Result<usize, String> {
    if name.len() > MAX_MESSAGE_NAME_BYTES {
        return Err(format!(
            "имя сообщения длиннее {MAX_MESSAGE_NAME_BYTES} байт"
        ));
    }
    let mut budget = MessageValueBudget::new();
    check_value(value, 0, &mut budget).map_err(str::to_owned)?;
    Ok(name.len().saturating_add(budget.estimated_size()))
}

fn check_value(
    value: &ParamValue,
    depth: usize,
    budget: &mut MessageValueBudget,
) -> Result<(), &'static str> {
    budget.enter(depth)?;
    match value {
        ParamValue::Float(number) if !number.is_finite() => {
            Err("message value contains a non-finite number")
        }
        ParamValue::String(text) => budget.add_string(text.len()),
        ParamValue::Array(values) => {
            for value in values {
                check_value(value, depth + 1, budget)?;
            }
            Ok(())
        }
        ParamValue::Bool(_) | ParamValue::Int(_) | ParamValue::Float(_) => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use engine_scene::MAX_MESSAGE_STRING_BYTES;

    #[test]
    fn budget_admits_up_to_the_message_count_and_then_refuses() {
        let mut budget = InboundBudget::default();
        for _ in 0..MAX_INBOUND_MESSAGES_PER_PEER {
            assert!(budget.admit(3, 1));
        }
        assert!(!budget.admit(3, 1));
        assert!(budget.admit(4, 1), "другой участник имеет свой лимит");
        assert!(budget.first_refusal(3));
        assert!(!budget.first_refusal(3));
        budget.reset();
        assert!(budget.admit(3, 1), "после сброса лимит снова свободен");
    }

    #[test]
    fn budget_refuses_by_bytes_even_below_the_count_limit() {
        let mut budget = InboundBudget::default();
        assert!(budget.admit(1, MAX_INBOUND_BYTES_PER_PEER));
        assert!(!budget.admit(1, 1));
    }

    #[test]
    fn error_log_limits_each_source_per_window_and_summarizes_the_rest() {
        let events: EventQueue = Default::default();
        let mut log = NetErrorLog::default();
        for index in 0..MAX_NET_ERROR_LOGS_PER_SOURCE_PER_SECOND + 5 {
            log.report(0.5, "scene/a", format!("error {index}"), &events);
        }
        log.report(0.5, "scene/b", "other".to_owned(), &events);
        assert_eq!(
            events.borrow_mut().drain().len(),
            MAX_NET_ERROR_LOGS_PER_SOURCE_PER_SECOND as usize + 1,
            "источники лимитируются раздельно"
        );

        log.report(1.6, "scene/a", "next window".to_owned(), &events);
        let drained = events.borrow_mut().drain();
        assert_eq!(drained.len(), 2, "{drained:?}");
        assert!(
            drained[0]
                .message
                .contains("подавлено ошибок net-callback: 5")
        );
        assert_eq!(drained[1].message, "next window");
    }

    #[test]
    fn message_validation_applies_the_shared_limits() {
        assert!(validate_message("hit", &ParamValue::Int(1)).is_ok());
        assert!(
            validate_message(
                &"n".repeat(MAX_MESSAGE_NAME_BYTES + 1),
                &ParamValue::Bool(true)
            )
            .is_err()
        );
        assert!(validate_message("x", &ParamValue::Float(f64::NAN)).is_err());
        let mut value = ParamValue::Int(0);
        for _ in 0..=16 {
            value = ParamValue::Array(vec![value]);
        }
        assert!(validate_message("deep", &value).is_err());
        let text = ParamValue::String("a".repeat(MAX_MESSAGE_STRING_BYTES + 1));
        assert!(validate_message("big", &text).is_err());
    }
}
