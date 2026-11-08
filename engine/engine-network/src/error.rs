//! Ошибки сетевого модуля.

use std::fmt;

/// Ошибка сетевого модуля. Текст выводится в лог и попадает в причины отключения.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum NetError {
    /// Конфигурация не допускает подключения, например без отпечатка сервера.
    Config(String),
    /// Очередь заполнена: команда или сообщение не принято, повторите позже.
    QueueFull,
    /// Сообщение больше допустимого размера.
    MessageTooLarge { size: usize, limit: usize },
    /// Значение сообщения не соответствует формату или лимитам.
    InvalidMessage(String),
    /// Участник сети неизвестен или уже отключён.
    UnknownPeer,
    /// Соединение не установлено.
    NotConnected,
    /// Сетевой поток остановлен.
    Stopped,
    /// Ошибка сокета.
    Io(String),
    /// Внутренняя ошибка, например генератор случайных чисел недоступен.
    Internal(String),
}

impl fmt::Display for NetError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Config(reason) => write!(f, "неверная настройка сети: {reason}"),
            Self::QueueFull => write!(f, "очередь сети заполнена"),
            Self::MessageTooLarge { size, limit } => {
                write!(f, "сообщение {size} байт больше лимита {limit} байт")
            }
            Self::InvalidMessage(reason) => write!(f, "некорректное сообщение: {reason}"),
            Self::UnknownPeer => write!(f, "участник сети неизвестен или отключён"),
            Self::NotConnected => write!(f, "соединение не установлено"),
            Self::Stopped => write!(f, "сетевой поток остановлен"),
            Self::Io(reason) => write!(f, "ошибка сокета: {reason}"),
            Self::Internal(reason) => write!(f, "внутренняя ошибка сети: {reason}"),
        }
    }
}

impl std::error::Error for NetError {}
