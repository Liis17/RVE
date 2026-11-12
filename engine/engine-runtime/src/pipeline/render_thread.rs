//! Render-поток: цикл отрисовки снимков, запросы ресурсов и отложенное освобождение.

use super::*;

pub(crate) fn start_render(
    modules: &mut ModuleHost,
    config: RenderThreadConfig,
) -> std::io::Result<()> {
    let stop_control = config.control_tx.clone();
    let stop_interrupt = config.interrupt.clone();
    modules.spawn(
        RENDER_THREAD,
        move || {
            stop_interrupt.interrupt();
            let _ = stop_control.send(RenderControl::Shutdown);
        },
        move || render_thread(config),
    )
}

pub(crate) fn render_thread(config: RenderThreadConfig) {
    let RenderThreadConfig {
        mut backend,
        width,
        height,
        frame_limit,
        frame_rx,
        resources,
        control_rx,
        script_tx,
        interrupt,
        status,
        resource_stats,
        stopped_tx,
        exit,
        ..
    } = config;
    let mut profiler =
        PhaseWindow::new(SOURCE_RENDER, Arc::clone(&status.profile), render_groups())
            .with_resources(resource_stats);
    let result = catch_unwind(AssertUnwindSafe(|| {
        render_loop(
            &mut *backend,
            width,
            height,
            frame_limit,
            &frame_rx,
            &resources,
            &control_rx,
            &script_tx,
            &mut profiler,
        )
    }));
    let failed = match result {
        Ok(Ok(())) => false,
        Ok(Err(message)) => {
            status.record_failure(message);
            true
        }
        Err(payload) => {
            status.record_failure(format!(
                "поток рендера завершился паникой: {}",
                panic_message(payload)
            ));
            true
        }
    };
    if failed {
        interrupt.interrupt();
    }
    let _ = script_tx.send(ScriptControl::Shutdown);
    #[cfg(feature = "dev-console")]
    if let Err(error) = backend.close_console() {
        status.record_failure(format!("ошибка освобождения UI консоли: {error}"));
    }
    if let Err(payload) = catch_unwind(AssertUnwindSafe(|| backend.release_scene())) {
        status.record_failure(format!(
            "ошибка освобождения ресурсов сцены: {}",
            panic_message(payload)
        ));
        interrupt.interrupt();
        let _ = script_tx.send(ScriptControl::Shutdown);
    }
    if let Err(payload) = catch_unwind(AssertUnwindSafe(|| backend.wait_idle())) {
        status.record_failure(format!(
            "ошибка ожидания renderer при завершении: {}",
            panic_message(payload)
        ));
        interrupt.interrupt();
        let _ = script_tx.send(ScriptControl::Shutdown);
    }
    if let Err(payload) = catch_unwind(AssertUnwindSafe(|| drop(backend))) {
        status.record_failure(format!(
            "паника при уничтожении renderer: {}",
            panic_message(payload)
        ));
        interrupt.interrupt();
        let _ = script_tx.send(ScriptControl::Shutdown);
    }
    status.mark_render_stopped();
    let _ = stopped_tx.try_send(());
    if let Err(payload) = catch_unwind(AssertUnwindSafe(|| exit.request_exit())) {
        status.record_failure(format!(
            "паника при запросе закрытия окна: {}",
            panic_message(payload)
        ));
    }
}

pub(crate) enum RenderSelection {
    Frame(Result<Arc<FrameSnapshot>, flume::RecvError>),
    Control(Result<RenderControl, flume::RecvError>),
    Resource(Result<RenderResourceRequest, flume::RecvError>),
}

/// Освобождение, которое ждёт, пока render thread обработает снимки с `seq ≥ last_seq`.
pub(crate) struct PendingRelease {
    pub(crate) target: ReleaseTarget,
    pub(crate) last_seq: u64,
}

pub(crate) enum ReleaseTarget {
    /// GPU-ресурсы сцены, которые больше не используются снимками.
    Scene(Box<SceneGpuIds>),
    Prefab(PrefabToken),
    Materials(Vec<MaterialId>),
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn render_loop(
    backend: &mut dyn RenderBackend,
    mut width: u32,
    mut height: u32,
    frame_limit: Option<u64>,
    frame_rx: &Receiver<Arc<FrameSnapshot>>,
    resources: &Receiver<RenderResourceRequest>,
    control_rx: &Receiver<RenderControl>,
    script_tx: &Sender<ScriptControl>,
    profiler: &mut PhaseWindow,
) -> Result<(), String> {
    let mut occluded = false;
    let mut paused = false;
    let mut presented = 0_u64;
    let mut motion_history = MotionHistory::default();
    #[cfg(feature = "dev-console")]
    let mut debug_view_dirty = false;
    let mut latest_snapshot: Option<Arc<FrameSnapshot>> = None;
    // Номер последнего снимка, принятого из канала кадров.
    let mut taken_seq = 0_u64;
    let mut releases: Vec<PendingRelease> = Vec::new();
    // Команды частиц принятых снимков, которые ещё не отрисованы. Снимок копирует их при приёме,
    // поэтому перерисовка того же снимка и пауза не применяют и не теряют их.
    let mut particle_queue: Vec<engine_scene::world::ParticleCommand> = Vec::new();
    let mut resources_open = true;
    #[cfg(feature = "dev-console")]
    let mut ui_frame = ConsoleUiFrame::default();
    #[cfg(feature = "dev-console")]
    let mut console_active = false;
    loop {
        profiler.publish_if_due(Instant::now());
        #[cfg(feature = "dev-console")]
        let mut should_render = std::mem::take(&mut debug_view_dirty);
        #[cfg(not(feature = "dev-console"))]
        let mut should_render = false;
        loop {
            match control_rx.try_recv() {
                Ok(RenderControl::Resize(new_width, new_height)) => {
                    width = new_width;
                    height = new_height;
                    backend.resize(width, height);
                    #[cfg(feature = "dev-console")]
                    {
                        should_render = true;
                    }
                }
                Ok(RenderControl::Occluded(value)) => {
                    occluded = value;
                    #[cfg(feature = "dev-console")]
                    {
                        should_render = true;
                    }
                }
                #[cfg(feature = "dev-console")]
                Ok(RenderControl::DebugView(value)) => {
                    backend.set_debug_view(value);
                    should_render = true;
                }
                #[cfg(feature = "dev-console")]
                Ok(RenderControl::ConsoleUiChanged) => should_render = true,
                Ok(RenderControl::Shutdown) | Err(TryRecvError::Disconnected) => return Ok(()),
                Err(TryRecvError::Empty) => break,
            }
        }

        // Ресурсы префабов принимаются и на паузе: загрузка и освобождение не ждут новых снимков.
        let resources_started = Instant::now();
        if resources_open {
            loop {
                match resources.try_recv() {
                    Ok(request) => {
                        handle_resource_request(backend, request, &mut releases, script_tx);
                    }
                    Err(TryRecvError::Empty) => break,
                    Err(TryRecvError::Disconnected) => {
                        resources_open = false;
                        break;
                    }
                }
            }
        }
        finish_releases(backend, &mut releases, taken_seq, &mut latest_snapshot);
        if backend.has_pending_prefab_uploads() {
            for reply in backend.advance_prefab_uploads(PREFAB_UPLOAD_BUDGET) {
                send_prefab_reply(script_tx, reply);
            }
        }
        profiler.record(
            render_phase::LOOP,
            render_phase::RESOURCE_REQUESTS,
            resources_started.elapsed(),
        );

        #[cfg(feature = "dev-console")]
        if let Some(frame) = backend.take_console_ui() {
            ui_frame.replace_with(frame);
            should_render = true;
            if ui_frame.visible {
                console_active = true;
            } else if console_active {
                ui_frame.textures_delta.clear();
                backend.close_console()?;
                console_active = false;
            }
        }

        let next_paused = occluded || width == 0 || height == 0;
        if next_paused != paused {
            paused = next_paused;
            let _ = script_tx.send(ScriptControl::Background(paused));
            #[cfg(feature = "dev-console")]
            {
                should_render = true;
            }
        }
        if paused {
            #[cfg(feature = "dev-console")]
            {
                backend.discard_console_geometry();
                ui_frame.primitives.clear();
            }
            should_render = false;
        }

        if should_render
            && let Some(snapshot) = latest_snapshot.as_mut()
            && let Some(outcome) = render_snapshot(
                backend,
                Arc::make_mut(snapshot),
                &mut motion_history,
                &mut particle_queue,
                paused,
                width,
                height,
                #[cfg(feature = "dev-console")]
                &mut ui_frame,
                profiler,
            )?
            && outcome.status == FrameStatus::Presented
        {
            presented = presented.saturating_add(1);
            if presented == 1 {
                log_startup_profile(profiler.board());
            }
            if frame_limit.is_some_and(|limit| presented >= limit) {
                return Ok(());
            }
        }

        let mut selector = Selector::new()
            .recv(frame_rx, RenderSelection::Frame)
            .recv(control_rx, RenderSelection::Control);
        if resources_open {
            selector = selector.recv(resources, RenderSelection::Resource);
        }
        // Пока загрузка не закончена, цикл не блокируется: следующая итерация продолжит её.
        let wait_started = Instant::now();
        let selection = if backend.has_pending_prefab_uploads() {
            selector.wait_timeout(Duration::ZERO).ok()
        } else {
            Some(selector.wait())
        };
        profiler.record(
            render_phase::LOOP,
            render_phase::FRAME_WAIT,
            wait_started.elapsed(),
        );
        let Some(selection) = selection else {
            continue;
        };
        match selection {
            RenderSelection::Resource(Ok(request)) => {
                let started = Instant::now();
                handle_resource_request(backend, request, &mut releases, script_tx);
                profiler.record(
                    render_phase::LOOP,
                    render_phase::RESOURCE_REQUESTS,
                    started.elapsed(),
                );
            }
            RenderSelection::Resource(Err(_)) => resources_open = false,
            RenderSelection::Control(Ok(RenderControl::Resize(new_width, new_height))) => {
                width = new_width;
                height = new_height;
                backend.resize(width, height);
            }
            RenderSelection::Control(Ok(RenderControl::Occluded(value))) => occluded = value,
            #[cfg(feature = "dev-console")]
            RenderSelection::Control(Ok(RenderControl::DebugView(value))) => {
                backend.set_debug_view(value);
                debug_view_dirty = true;
            }
            #[cfg(feature = "dev-console")]
            RenderSelection::Control(Ok(RenderControl::ConsoleUiChanged)) => {}
            RenderSelection::Control(Ok(RenderControl::Shutdown))
            | RenderSelection::Control(Err(_))
            | RenderSelection::Frame(Err(_)) => return Ok(()),
            RenderSelection::Frame(Ok(snapshot)) => {
                taken_seq = snapshot.sequence;
                particle_queue.extend_from_slice(&snapshot.particle_commands);
                latest_snapshot = Some(snapshot);
                let Some(snapshot) = latest_snapshot.as_mut() else {
                    continue;
                };
                let Some(outcome) = render_snapshot(
                    backend,
                    Arc::make_mut(snapshot),
                    &mut motion_history,
                    &mut particle_queue,
                    paused,
                    width,
                    height,
                    #[cfg(feature = "dev-console")]
                    &mut ui_frame,
                    profiler,
                )?
                else {
                    continue;
                };
                if outcome.status == FrameStatus::Presented {
                    presented = presented.saturating_add(1);
                    if presented == 1 {
                        log_startup_profile(profiler.board());
                    }
                    if frame_limit.is_some_and(|limit| presented >= limit) {
                        return Ok(());
                    }
                }
            }
        }
    }
}

pub(crate) fn handle_resource_request(
    backend: &mut dyn RenderBackend,
    request: RenderResourceRequest,
    releases: &mut Vec<PendingRelease>,
    script_tx: &Sender<ScriptControl>,
) {
    match request {
        RenderResourceRequest::Upload(prefab) => {
            if let Some(reply) = backend.submit_prefab(*prefab) {
                send_prefab_reply(script_tx, reply);
            }
        }
        RenderResourceRequest::Particle(request) => backend.apply_particle_request(request),
        RenderResourceRequest::Release { token, last_seq } => {
            releases.push(PendingRelease {
                target: ReleaseTarget::Prefab(token),
                last_seq,
            });
        }
        RenderResourceRequest::Material(request) => {
            let reply = backend.submit_material(*request);
            let _ = script_tx.send(ScriptControl::Material(Box::new(reply)));
        }
        RenderResourceRequest::ReleaseMaterials {
            materials,
            last_seq,
        } => {
            releases.push(PendingRelease {
                target: ReleaseTarget::Materials(materials),
                last_seq,
            });
        }
        RenderResourceRequest::Scene(upload) => {
            let reply = backend.submit_scene(*upload);
            let _ = script_tx.send(ScriptControl::Scene(Box::new(reply)));
        }
        RenderResourceRequest::ReleaseScene { ids, last_seq } => {
            releases.push(PendingRelease {
                target: ReleaseTarget::Scene(ids),
                last_seq,
            });
        }
    }
}

/// Уничтожает GPU-ресурсы освобождённых префабов, когда render thread принял снимок с
/// `seq ≥ last_seq`. Удерживаемый для перерисовки снимок с `seq ≤ last_seq` сбрасывается:
/// он может ссылаться на уничтожаемые ресурсы.
pub(crate) fn finish_releases(
    backend: &mut dyn RenderBackend,
    releases: &mut Vec<PendingRelease>,
    taken_seq: u64,
    latest: &mut Option<Arc<FrameSnapshot>>,
) {
    releases.retain(|release| {
        if taken_seq < release.last_seq {
            return true;
        }
        if latest
            .as_ref()
            .is_some_and(|snapshot| snapshot.sequence <= release.last_seq)
        {
            *latest = None;
        }
        match &release.target {
            ReleaseTarget::Prefab(token) => backend.release_prefab(token),
            ReleaseTarget::Materials(materials) => backend.release_materials(materials),
            ReleaseTarget::Scene(ids) => backend.release_scene_resources(ids),
        }
        false
    });
}

pub(crate) fn send_prefab_reply(script_tx: &Sender<ScriptControl>, reply: PrefabReply) {
    let _ = script_tx.send(ScriptControl::Prefab(reply));
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn render_snapshot(
    backend: &mut dyn RenderBackend,
    snapshot: &mut FrameSnapshot,
    motion_history: &mut MotionHistory,
    particle_queue: &mut Vec<engine_scene::world::ParticleCommand>,
    paused: bool,
    width: u32,
    height: u32,
    #[cfg(feature = "dev-console")] ui_frame: &mut ConsoleUiFrame,
    profiler: &mut PhaseWindow,
) -> Result<Option<FrameOutcome>, String> {
    if paused {
        return Ok(None);
    }
    if !particle_queue.is_empty() {
        let commands = std::mem::take(particle_queue);
        backend.apply_particle_commands(&commands);
    }
    motion_history.prepare(snapshot);
    let started = Instant::now();
    #[cfg(feature = "dev-console")]
    let outcome = if ui_frame.visible {
        backend.render_console(snapshot, width, height, ui_frame)?
    } else {
        backend.render(snapshot, width, height)?
    };
    #[cfg(not(feature = "dev-console"))]
    let outcome = backend.render(snapshot, width, height)?;
    profiler.record(
        render_phase::LOOP,
        render_phase::RENDER_BACKEND,
        started.elapsed(),
    );
    if let Some(frame) = backend.take_frame_profile() {
        record_frame_profile(profiler, &frame);
    }
    if outcome.submitted {
        motion_history.submitted(snapshot);
    }
    Ok(Some(outcome))
}
