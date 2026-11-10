//! Платформенные запросы скриптов: окно (полноэкранный режим, скриншот) и системные диалоги файлов.
//!
//! Хост и транзакции делят [`PlatformState`]. Вызов скрипта ставит запросы и чтения готовых
//! результатов диалогов в транзакцию; успешный коммит переносит их в общее состояние, откуда
//! запросы забирает вызывающий код ([`crate::ScriptHost::take_platform_requests`]), а результаты
//! диалогов возвращает через [`crate::ScriptHost::complete_file_dialog`]. Отказавший вызов не
//! открывает диалогов и не теряет непрочитанных результатов.

use std::cell::RefCell;
use std::collections::HashMap;
use std::path::PathBuf;
use std::rc::Rc;

/// Наибольшее число платформенных запросов, ждущих вызывающего кода.
pub(crate) const MAX_PLATFORM_REQUESTS: usize = 16;
/// Наибольшее число диалогов, открытых скриптами и ещё не прочитанных через `poll`.
pub(crate) const MAX_OPEN_DIALOGS: usize = 8;

/// Вид системного диалога файлов.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FileDialogKind {
    /// Выбор одного существующего файла.
    OpenFile,
    /// Выбор нескольких существующих файлов.
    OpenFiles,
    /// Выбор пути для сохранения файла.
    SaveFile,
    /// Выбор папки.
    PickFolder,
}

impl FileDialogKind {
    /// Код вида в ABI скриптов.
    pub(crate) fn from_code(code: u32) -> Option<Self> {
        match code {
            rve_sdk::abi::RVE_FILE_DIALOG_OPEN_FILE => Some(Self::OpenFile),
            rve_sdk::abi::RVE_FILE_DIALOG_OPEN_FILES => Some(Self::OpenFiles),
            rve_sdk::abi::RVE_FILE_DIALOG_SAVE_FILE => Some(Self::SaveFile),
            rve_sdk::abi::RVE_FILE_DIALOG_PICK_FOLDER => Some(Self::PickFolder),
            _ => None,
        }
    }
}

/// Фильтр расширений диалога: подпись и расширения без точки.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileDialogFilter {
    pub name: String,
    pub extensions: Vec<String>,
}

/// Параметры системного диалога файлов. Пустые значения оставляют выбор системе.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileDialogRequest {
    pub kind: FileDialogKind,
    pub title: Option<String>,
    /// Начальная папка.
    pub directory: Option<PathBuf>,
    /// Предлагаемое имя файла.
    pub file_name: Option<String>,
    pub filters: Vec<FileDialogFilter>,
}

impl FileDialogRequest {
    /// Проверяет фильтры и убирает ведущие точки расширений.
    pub(crate) fn normalized(mut self) -> Result<Self, String> {
        for filter in &mut self.filters {
            if filter.name.trim().is_empty() {
                return Err("file dialog filter name must not be empty".to_owned());
            }
            if filter.extensions.is_empty() {
                return Err(format!(
                    "file dialog filter {:?} has no extensions",
                    filter.name
                ));
            }
            for extension in &mut filter.extensions {
                let trimmed = extension.trim_start_matches('.');
                if trimmed.is_empty() || trimmed.contains(['/', '\\']) {
                    return Err(format!(
                        "file dialog filter {:?} has invalid extension {extension:?}",
                        filter.name
                    ));
                }
                *extension = trimmed.to_owned();
            }
        }
        Ok(self)
    }
}

/// Запрос скрипта к окну или системе, который выполняет вызывающий код после кадра.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PlatformRequest {
    /// Включить или выключить полноэкранный режим без рамки.
    SetFullscreen(bool),
    /// Сохранить PNG-скриншот окна; `None` — папка скриншотов проекта по умолчанию.
    Screenshot(Option<PathBuf>),
    /// Открыть системный диалог; результат передаётся в `complete_file_dialog` с тем же `id`.
    FileDialog { id: u64, request: FileDialogRequest },
}

#[derive(Debug)]
enum DialogSlot {
    Pending,
    /// Выбранные пути; пустой список — диалог отменён.
    Done(Vec<PathBuf>),
}

#[derive(Debug, Default)]
pub(crate) struct PlatformState {
    /// Состояние окна, сообщённое хосту вызывающим кодом.
    pub(crate) fullscreen: bool,
    next_dialog: u64,
    dialogs: HashMap<u64, DialogSlot>,
    requests: Vec<PlatformRequest>,
}

pub(crate) type SharedPlatformState = Rc<RefCell<PlatformState>>;

/// Чтение результата диалога.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum DialogPoll {
    Pending,
    Done(Vec<PathBuf>),
}

impl PlatformState {
    /// Новый идентификатор диалога; идентификаторы начинаются с 1 и не повторяются.
    pub(crate) fn allocate_dialog(&mut self) -> u64 {
        self.next_dialog += 1;
        self.next_dialog
    }

    pub(crate) fn queued_requests(&self) -> usize {
        self.requests.len()
    }

    pub(crate) fn open_dialogs(&self) -> usize {
        self.dialogs.len()
    }

    pub(crate) fn poll(&self, id: u64) -> Result<DialogPoll, String> {
        match self.dialogs.get(&id) {
            Some(DialogSlot::Pending) => Ok(DialogPoll::Pending),
            Some(DialogSlot::Done(paths)) => Ok(DialogPoll::Done(paths.clone())),
            None => Err(format!("unknown file dialog id {id}")),
        }
    }

    /// Применяет запросы и прочитанные результаты успешного вызова.
    pub(crate) fn commit(&mut self, requests: Vec<PlatformRequest>, consumed: &[u64]) {
        for id in consumed {
            self.dialogs.remove(id);
        }
        for request in requests {
            if let PlatformRequest::FileDialog { id, .. } = &request {
                self.dialogs.insert(*id, DialogSlot::Pending);
            }
            self.requests.push(request);
        }
    }

    pub(crate) fn take_requests(&mut self) -> Vec<PlatformRequest> {
        std::mem::take(&mut self.requests)
    }

    /// Сохраняет результат ожидающего диалога; неизвестный или завершённый `id` игнорируется.
    pub(crate) fn complete(&mut self, id: u64, paths: Vec<PathBuf>) {
        if let Some(slot @ DialogSlot::Pending) = self.dialogs.get_mut(&id) {
            *slot = DialogSlot::Done(paths);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dialog(kind: FileDialogKind) -> FileDialogRequest {
        FileDialogRequest {
            kind,
            title: None,
            directory: None,
            file_name: None,
            filters: Vec::new(),
        }
    }

    #[test]
    fn dialog_result_is_pending_until_completed_and_removed_once_consumed() {
        let mut state = PlatformState::default();
        let id = state.allocate_dialog();
        assert_eq!(id, 1);
        assert!(state.poll(id).is_err());

        state.commit(
            vec![PlatformRequest::FileDialog {
                id,
                request: dialog(FileDialogKind::OpenFile),
            }],
            &[],
        );
        assert_eq!(state.poll(id), Ok(DialogPoll::Pending));
        assert_eq!(state.take_requests().len(), 1);
        assert!(state.take_requests().is_empty());

        state.complete(id, vec![PathBuf::from("a.txt")]);
        state.complete(id, vec![PathBuf::from("ignored.txt")]);
        assert_eq!(
            state.poll(id),
            Ok(DialogPoll::Done(vec![PathBuf::from("a.txt")]))
        );
        state.commit(Vec::new(), &[id]);
        assert!(state.poll(id).is_err());
        assert_eq!(state.open_dialogs(), 0);
    }

    #[test]
    fn filters_drop_leading_dots_and_reject_empty_values() {
        let mut request = dialog(FileDialogKind::SaveFile);
        request.filters.push(FileDialogFilter {
            name: "Images".to_owned(),
            extensions: vec![".png".to_owned(), "jpg".to_owned()],
        });
        let normalized = request.clone().normalized().unwrap();
        assert_eq!(normalized.filters[0].extensions, ["png", "jpg"]);

        request.filters[0].extensions = vec![".".to_owned()];
        assert!(request.clone().normalized().is_err());
        request.filters[0].extensions.clear();
        assert!(request.clone().normalized().is_err());
        request.filters[0].extensions = vec!["png".to_owned()];
        request.filters[0].name = " ".to_owned();
        assert!(request.normalized().is_err());
    }
}
