use engine_window::WindowProxy;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

pub(crate) trait FocusRequester: Send + Sync {
    fn request_focus(&self);
}

struct WindowFocus(WindowProxy);

impl FocusRequester for WindowFocus {
    fn request_focus(&self) {
        self.0.request_focus();
    }
}

#[derive(Default)]
struct ActivationState {
    pending: bool,
    proxy: Option<Arc<dyn FocusRequester>>,
}

/// Carries activation requests received before the event loop attaches its window proxy.
#[derive(Clone, Default)]
pub(crate) struct PendingActivation {
    state: Arc<Mutex<ActivationState>>,
}

impl PendingActivation {
    pub fn request(&self) {
        let proxy = {
            let mut state = self.state.lock().expect("activation state poisoned");
            match state.proxy.clone() {
                Some(proxy) => Some(proxy),
                None => {
                    state.pending = true;
                    None
                }
            }
        };
        if let Some(proxy) = proxy {
            proxy.request_focus();
        }
    }

    pub fn attach(&self, proxy: WindowProxy) {
        self.attach_requester(Arc::new(WindowFocus(proxy)));
    }

    fn attach_requester(&self, proxy: Arc<dyn FocusRequester>) {
        let pending = {
            let mut state = self.state.lock().expect("activation state poisoned");
            state.proxy = Some(Arc::clone(&proxy));
            std::mem::take(&mut state.pending)
        };
        if pending {
            proxy.request_focus();
        }
    }
}

pub(crate) enum ActivationFailure {
    Timeout,
    Other(String),
}

pub(crate) enum InstanceOutcome<L> {
    Continue(L),
    Activated,
    Failure { code: u8, message: String },
}

/// Applies the single-instance protocol, including one lock retry after activation timeout.
pub(crate) fn acquire_or_activate<L>(
    mut acquire: impl FnMut() -> Result<Option<L>, String>,
    mut activate: impl FnMut(Instant) -> Result<(), ActivationFailure>,
    timeout: Duration,
) -> InstanceOutcome<L> {
    match acquire() {
        Ok(Some(lock)) => InstanceOutcome::Continue(lock),
        Err(error) => InstanceOutcome::Failure {
            code: 1,
            message: format!("не удалось захватить блокировку экземпляра: {error}"),
        },
        Ok(None) => match activate(Instant::now() + timeout) {
            Ok(()) => InstanceOutcome::Activated,
            Err(ActivationFailure::Other(error)) => InstanceOutcome::Failure {
                code: 4,
                message: error,
            },
            Err(ActivationFailure::Timeout) => match acquire() {
                Ok(Some(lock)) => InstanceOutcome::Continue(lock),
                Ok(None) => InstanceOutcome::Failure {
                    code: 4,
                    message: "экземпляр не подтвердил активацию за 5 секунд".to_owned(),
                },
                Err(error) => InstanceOutcome::Failure {
                    code: 1,
                    message: format!(
                        "экземпляр не подтвердил активацию; повторный захват блокировки не удался: {error}"
                    ),
                },
            },
        },
    }
}

#[cfg(test)]
mod tests {
    use super::{FocusRequester, PendingActivation};
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };
    use std::time::Duration;

    #[derive(Default)]
    struct FakeFocus(AtomicUsize);

    impl FocusRequester for FakeFocus {
        fn request_focus(&self) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }

    #[test]
    fn request_before_attach_is_delivered_once_when_proxy_arrives() {
        let activation = PendingActivation::default();
        let target = Arc::new(FakeFocus::default());
        activation.request();
        activation.attach_requester(target.clone());
        assert_eq!(target.0.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn request_after_attach_is_forwarded_immediately() {
        let activation = PendingActivation::default();
        let target = Arc::new(FakeFocus::default());
        activation.attach_requester(target.clone());
        activation.request();
        assert_eq!(target.0.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn activation_timeout_retries_acquire_once_and_can_start_as_owner() {
        use super::{ActivationFailure, InstanceOutcome, acquire_or_activate};
        use std::cell::Cell;

        let acquire_calls = Cell::new(0);
        let outcome = acquire_or_activate(
            || {
                let call = acquire_calls.get();
                acquire_calls.set(call + 1);
                Ok((call == 1).then_some("lock"))
            },
            |_| Err(ActivationFailure::Timeout),
            Duration::from_secs(5),
        );
        assert!(matches!(outcome, InstanceOutcome::Continue("lock")));
        assert_eq!(acquire_calls.get(), 2);
    }

    #[test]
    fn activation_success_exits_without_a_second_acquire() {
        use super::{InstanceOutcome, acquire_or_activate};
        use std::cell::Cell;

        let acquire_calls = Cell::new(0);
        let outcome = acquire_or_activate(
            || {
                acquire_calls.set(acquire_calls.get() + 1);
                Ok::<Option<()>, String>(None)
            },
            |_| Ok(()),
            Duration::from_secs(5),
        );
        assert!(matches!(outcome, InstanceOutcome::Activated));
        assert_eq!(acquire_calls.get(), 1);
    }

    #[test]
    fn activation_timeout_without_a_lock_retry_exits_with_code_four() {
        use super::{ActivationFailure, InstanceOutcome, acquire_or_activate};
        use std::cell::Cell;

        let acquire_calls = Cell::new(0);
        let outcome = acquire_or_activate(
            || {
                acquire_calls.set(acquire_calls.get() + 1);
                Ok::<Option<()>, String>(None)
            },
            |_| Err(ActivationFailure::Timeout),
            Duration::from_secs(5),
        );
        assert!(matches!(outcome, InstanceOutcome::Failure { code: 4, .. }));
        assert_eq!(acquire_calls.get(), 2);
    }

    #[test]
    fn operating_system_acquire_errors_use_exit_code_one() {
        use super::{InstanceOutcome, acquire_or_activate};

        let outcome = acquire_or_activate(
            || Err::<Option<()>, _>("OS lock failure".to_owned()),
            |_| panic!("activation must not run after an acquire error"),
            Duration::from_secs(5),
        );
        assert!(matches!(outcome, InstanceOutcome::Failure { code: 1, .. }));
    }

    #[test]
    fn activation_errors_use_exit_code_four() {
        use super::{ActivationFailure, InstanceOutcome, acquire_or_activate};

        let outcome = acquire_or_activate(
            || Ok::<Option<()>, String>(None),
            |_| Err(ActivationFailure::Other("activation failed".to_owned())),
            Duration::from_secs(5),
        );
        assert!(matches!(outcome, InstanceOutcome::Failure { code: 4, .. }));
    }

    #[test]
    fn operating_system_error_on_timeout_retry_uses_exit_code_one() {
        use super::{ActivationFailure, InstanceOutcome, acquire_or_activate};
        use std::cell::Cell;

        let acquire_calls = Cell::new(0);
        let outcome = acquire_or_activate(
            || {
                let call = acquire_calls.get();
                acquire_calls.set(call + 1);
                if call == 0 {
                    Ok::<Option<()>, String>(None)
                } else {
                    Err("OS lock failure".to_owned())
                }
            },
            |_| Err(ActivationFailure::Timeout),
            Duration::from_secs(5),
        );
        assert!(matches!(outcome, InstanceOutcome::Failure { code: 1, .. }));
        assert_eq!(acquire_calls.get(), 2);
    }
}
