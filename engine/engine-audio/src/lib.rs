//! Воспроизведение и управление игровым звуком.

use std::sync::mpsc::Receiver;

#[derive(Default)]
pub struct AudioEngine;

impl AudioEngine {
    pub fn new() -> Self {
        Self
    }

    pub fn run(self, stop: Receiver<()>) {
        let _ = stop.recv();
    }
}

#[cfg(test)]
mod tests {
    use super::AudioEngine;
    use std::sync::mpsc;
    use std::thread;

    #[test]
    fn run_returns_after_stop_signal() {
        let (stop_tx, stop_rx) = mpsc::channel();
        let (done_tx, done_rx) = mpsc::channel();
        let worker = thread::spawn(move || {
            AudioEngine::new().run(stop_rx);
            done_tx.send(()).unwrap();
        });

        stop_tx.send(()).unwrap();
        done_rx.recv().unwrap();
        worker.join().unwrap();
    }

    #[test]
    fn run_returns_when_stop_sender_is_dropped() {
        let (stop_tx, stop_rx) = mpsc::channel();
        let (done_tx, done_rx) = mpsc::channel();
        let worker = thread::spawn(move || {
            AudioEngine::new().run(stop_rx);
            done_tx.send(()).unwrap();
        });

        drop(stop_tx);
        done_rx.recv().unwrap();
        worker.join().unwrap();
    }
}
