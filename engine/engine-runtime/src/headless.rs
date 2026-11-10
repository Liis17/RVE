//! Headless-путь `rve-server`: ответчик render-ресурсов вместо render thread и команда `quit`.
//!
//! Окна, renderer, GPU-проверка и звуковой вывод здесь отсутствуют. Остановка сервера: `--frames N`
//! (тиков скриптов), строка `quit` на stdin, фатальная ошибка потоков или Ctrl+C. Ctrl+C завершает
//! процесс без `on_destroy` скриптов.

use crate::pipeline::ScriptControl;
use crate::prefabs::{
    LoadedPrefab, MaterialReply, PrefabReady, PrefabReply, RenderResourceRequest,
};
use crate::scene_switch::SceneUploadReply;
use flume::{Receiver, RecvTimeoutError, Sender};
use std::io::BufRead;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

/// Период проверки флага остановки у ответчика, когда запросов нет.
const STOP_POLL: Duration = Duration::from_millis(50);

/// Ответ на загрузку тяжёлого префаба без GPU: уровней нет, CPU-часть та же, что у render thread.
/// Без ответа scripts thread ждал бы префаб вечно.
pub(crate) fn resource_reply(prefab: LoadedPrefab) -> PrefabReply {
    let LoadedPrefab { token, cpu, .. } = prefab;
    PrefabReply::Ready(Box::new(PrefabReady {
        token,
        gpu: Vec::new(),
        particles: Vec::new(),
        cpu,
    }))
}

/// Ответчик render-ресурсов: на `Upload` сразу шлёт `Ready`; `Particle` и `Release` принимает без
/// действия, потому что GPU нет. Работает, пока не выставлен `stop` или не закрыт канал запросов.
pub(crate) fn run_resource_responder(
    requests: Receiver<RenderResourceRequest>,
    script_tx: Sender<ScriptControl>,
    stop: Arc<AtomicBool>,
) {
    loop {
        match requests.recv_timeout(STOP_POLL) {
            Ok(RenderResourceRequest::Upload(prefab)) => {
                let reply = ScriptControl::Prefab(resource_reply(*prefab));
                if script_tx.send(reply).is_err() {
                    break;
                }
            }
            Ok(RenderResourceRequest::Material(request)) => {
                let reply = ScriptControl::Material(Box::new(MaterialReply {
                    object: request.object,
                    revision: request.revision,
                    result: Err("headless-сервер не создаёт GPU-материалы".to_owned()),
                }));
                if script_tx.send(reply).is_err() {
                    break;
                }
            }
            Ok(RenderResourceRequest::Scene(upload)) => {
                // Без GPU загрузка сцены не создаёт ресурсов: ответ пустой.
                let reply = ScriptControl::Scene(Box::new(SceneUploadReply {
                    generation: upload.generation,
                    result: Ok(Box::default()),
                }));
                if script_tx.send(reply).is_err() {
                    break;
                }
            }
            Ok(
                RenderResourceRequest::Particle(_)
                | RenderResourceRequest::Release { .. }
                | RenderResourceRequest::ReleaseMaterials { .. }
                | RenderResourceRequest::ReleaseScene { .. },
            ) => {}
            Err(RecvTimeoutError::Timeout) => {
                if stop.load(Ordering::Acquire) {
                    break;
                }
            }
            Err(RecvTimeoutError::Disconnected) => break,
        }
    }
}

/// Строка `quit` (с пробелами по краям допустима) останавливает скрипты штатно.
pub(crate) fn is_quit_command(line: &str) -> bool {
    line.trim() == "quit"
}

/// Читает stdin и на `quit` штатно останавливает скрипты (`Shutdown`, без прерывания вызовов).
/// Закрытый stdin (например, `</dev/null`) не останавливает сервер: поток просто завершается.
pub(crate) fn spawn_quit_listener(script_tx: Sender<ScriptControl>) -> std::io::Result<()> {
    std::thread::Builder::new()
        .name("rve-stdin".to_owned())
        .spawn(move || {
            for line in std::io::stdin().lock().lines() {
                match line {
                    Ok(line) if is_quit_command(&line) => {
                        let _ = script_tx.send(ScriptControl::Shutdown);
                        return;
                    }
                    Ok(_) => {}
                    Err(_) => return,
                }
            }
        })
        .map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::{is_quit_command, resource_reply, run_resource_responder};
    use crate::pipeline::ScriptControl;
    use crate::prefabs::{
        LoadedPrefab, PrefabCpu, PrefabReply, PrefabToken, RenderResourceRequest,
    };
    use crate::scene::PreparedMaterialAssets;
    use engine_files::VirtualPath;
    use engine_scene::PrefabTemplate;
    use std::collections::BTreeMap;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    fn loaded(path: &str) -> LoadedPrefab {
        let template_path = VirtualPath::parse(path).unwrap();
        let template = PrefabTemplate::load(&template_path, &mut |_| {
            Ok("[[objects]]\nid = \"root\"\n".to_owned())
        })
        .unwrap();
        LoadedPrefab {
            token: PrefabToken {
                path: path.to_owned(),
                load_id: 7,
            },
            cpu: PrefabCpu {
                ui: Default::default(),
                template: Arc::new(template),
                sources: BTreeMap::new(),
                geometry: Vec::new(),
                material_models: Vec::new(),
            },
            material_assets: Arc::new(PreparedMaterialAssets::default()),
            levels: Vec::new(),
            particles: Vec::new(),
        }
    }

    #[test]
    fn resource_reply_is_ready_without_gpu_and_keeps_the_token() {
        match resource_reply(loaded("prefabs/ball.rveprefab")) {
            PrefabReply::Ready(ready) => {
                assert_eq!(ready.token.path, "prefabs/ball.rveprefab");
                assert_eq!(ready.token.load_id, 7);
                assert!(ready.gpu.is_empty());
                assert!(ready.particles.is_empty());
            }
            PrefabReply::Failed { .. } => panic!("headless-ответ должен быть Ready"),
        }
    }

    #[test]
    fn responder_answers_uploads_and_stops_on_flag() {
        let (request_tx, request_rx) = flume::unbounded();
        let (script_tx, script_rx) = flume::unbounded();
        let stop = Arc::new(AtomicBool::new(false));
        let worker = {
            let stop = Arc::clone(&stop);
            std::thread::spawn(move || run_resource_responder(request_rx, script_tx, stop))
        };
        request_tx
            .send(RenderResourceRequest::Upload(Box::new(loaded(
                "prefabs/ball.rveprefab",
            ))))
            .unwrap();
        request_tx
            .send(RenderResourceRequest::Release {
                token: PrefabToken {
                    path: "prefabs/ball.rveprefab".to_owned(),
                    load_id: 7,
                },
                last_seq: 0,
            })
            .unwrap();
        let reply = script_rx
            .recv_timeout(std::time::Duration::from_secs(2))
            .expect("ответ на Upload");
        match reply {
            ScriptControl::Prefab(PrefabReply::Ready(ready)) => {
                assert_eq!(ready.token.path, "prefabs/ball.rveprefab");
            }
            _ => panic!("ожидался Ready"),
        }
        stop.store(true, Ordering::Release);
        worker.join().unwrap();
        assert!(
            script_rx.try_recv().is_err(),
            "лишних ответов быть не должно"
        );
    }

    #[test]
    fn responder_stops_when_requests_channel_is_closed() {
        let (request_tx, request_rx) = flume::unbounded::<RenderResourceRequest>();
        let (script_tx, _script_rx) = flume::unbounded();
        drop(request_tx);
        run_resource_responder(request_rx, script_tx, Arc::new(AtomicBool::new(false)));
    }

    #[test]
    fn only_a_trimmed_quit_line_stops_the_server() {
        assert!(is_quit_command("quit"));
        assert!(is_quit_command("  quit\r"));
        assert!(!is_quit_command("quitting"));
        assert!(!is_quit_command(""));
    }
}
