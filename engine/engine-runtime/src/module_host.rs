use std::panic::{AssertUnwindSafe, catch_unwind};
use std::thread::{self, JoinHandle};

struct ManagedThread {
    name: String,
    stop: Option<Box<dyn Fn() + Send>>,
    join: Option<JoinHandle<()>>,
}

/// Owns runtime threads and stops/joins them in reverse start order.
#[derive(Default)]
pub(crate) struct ModuleHost {
    threads: Vec<ManagedThread>,
    shutdown_started: bool,
}

impl ModuleHost {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn spawn(
        &mut self,
        name: &'static str,
        stop: impl Fn() + Send + 'static,
        task: impl FnOnce() + Send + 'static,
    ) -> std::io::Result<()> {
        let join = thread::Builder::new().name(name.to_owned()).spawn(task)?;
        self.threads.push(ManagedThread {
            name: name.to_owned(),
            stop: Some(Box::new(stop)),
            join: Some(join),
        });
        Ok(())
    }

    pub fn join_named(&mut self, name: &str) -> Result<(), String> {
        let Some(thread) = self.threads.iter_mut().find(|thread| thread.name == name) else {
            return Ok(());
        };
        if let Some(join) = thread.join.take() {
            join.join().map_err(|payload| {
                format!(
                    "поток {} завершился паникой: {}",
                    thread.name,
                    panic_message(payload)
                )
            })?;
        }
        Ok(())
    }

    /// Drops the join handle without waiting for the thread to finish.
    pub fn detach(&mut self, name: &str) -> bool {
        let Some(thread) = self.threads.iter_mut().find(|thread| thread.name == name) else {
            return false;
        };
        thread.join.take().is_some()
    }

    pub fn shutdown(&mut self) -> Vec<String> {
        if self.shutdown_started {
            return Vec::new();
        }
        self.shutdown_started = true;
        let mut errors = Vec::new();
        for thread in self.threads.iter_mut().rev() {
            if let Some(stop) = thread.stop.take()
                && catch_unwind(AssertUnwindSafe(stop)).is_err()
            {
                errors.push(format!(
                    "обработчик остановки потока {} завершился паникой",
                    thread.name
                ));
            }
        }
        for thread in self.threads.iter_mut().rev() {
            if let Some(join) = thread.join.take()
                && let Err(payload) = join.join()
            {
                errors.push(format!(
                    "поток {} завершился паникой: {}",
                    thread.name,
                    panic_message(payload)
                ));
            }
        }
        errors
    }
}

impl Drop for ModuleHost {
    fn drop(&mut self) {
        let _ = self.shutdown();
    }
}

pub(crate) fn panic_message(payload: Box<dyn std::any::Any + Send>) -> String {
    if let Some(message) = payload.downcast_ref::<&str>() {
        (*message).to_owned()
    } else if let Some(message) = payload.downcast_ref::<String>() {
        message.clone()
    } else {
        "неизвестная причина паники".to_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::ModuleHost;
    use std::sync::{Arc, Mutex, mpsc};

    #[test]
    fn starts_and_stops_threads_and_joins_them() {
        let (stop_tx, stop_rx) = mpsc::channel();
        let (done_tx, done_rx) = mpsc::channel();
        let mut host = ModuleHost::new();
        host.spawn(
            "module",
            move || {
                let _ = stop_tx.send(());
            },
            move || {
                let _ = stop_rx.recv();
                let _ = done_tx.send(());
            },
        )
        .unwrap();

        assert!(host.shutdown().is_empty());
        done_rx.recv().unwrap();
    }

    #[test]
    fn stops_modules_in_reverse_start_order() {
        let stopped = Arc::new(Mutex::new(Vec::new()));
        let mut host = ModuleHost::new();
        for name in ["first", "second"] {
            let (stop_tx, stop_rx) = mpsc::channel();
            let stopped = Arc::clone(&stopped);
            host.spawn(
                name,
                move || {
                    stopped.lock().unwrap().push(name);
                    let _ = stop_tx.send(());
                },
                move || {
                    let _ = stop_rx.recv();
                },
            )
            .unwrap();
        }
        assert!(host.shutdown().is_empty());
        assert_eq!(*stopped.lock().unwrap(), ["second", "first"]);
    }

    #[test]
    fn collects_thread_panics() {
        let mut host = ModuleHost::new();
        host.spawn("panics", || {}, || panic!("test panic"))
            .unwrap();
        let errors = host.shutdown();
        assert_eq!(errors.len(), 1);
        assert!(errors[0].contains("panics"));
        assert!(errors[0].contains("test panic"));
    }

    #[test]
    fn detached_thread_is_not_joined_during_shutdown() {
        let (started_tx, started_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let (stopped_tx, stopped_rx) = mpsc::channel();
        let mut host = ModuleHost::new();
        host.spawn(
            "render",
            move || {
                let _ = stopped_tx.send(());
            },
            move || {
                let _ = started_tx.send(());
                let _ = release_rx.recv();
            },
        )
        .unwrap();
        started_rx.recv().unwrap();
        assert!(host.detach("render"));
        assert!(host.shutdown().is_empty());
        stopped_rx.recv().unwrap();
        release_tx.send(()).unwrap();
    }

    #[test]
    fn shutting_down_before_any_thread_is_safe() {
        let mut host = ModuleHost::new();
        assert!(host.shutdown().is_empty());
    }
}
