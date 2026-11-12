//! Поток скриптов: запуск, цикл симуляции, отправка снимков и фоновый режим.

use super::*;

pub(crate) fn start_scripts(
    modules: &mut ModuleHost,
    world: World,
    config: ScriptThreadConfig,
) -> std::io::Result<ScriptThreadHandle> {
    let (ready_tx, ready_rx) = mpsc::sync_channel(1);
    let (stopped_tx, stopped_rx) = mpsc::sync_channel(1);
    let stop_control = config.control_tx.clone();
    let stop_interrupt = config.interrupt.clone();
    let status = Arc::clone(&config.status);
    let panic_interrupt = config.interrupt.clone();
    let panic_render_tx = config.render_tx.clone();
    modules.spawn(
        SCRIPTS_THREAD,
        move || {
            stop_interrupt.interrupt();
            let _ = stop_control.send(ScriptControl::Shutdown);
        },
        move || {
            let mut ready_tx = Some(ready_tx);
            let result = catch_unwind(AssertUnwindSafe(|| {
                scripts_thread(world, config, &mut ready_tx);
            }));
            if let Err(payload) = result {
                let message = format!(
                    "поток скриптов завершился паникой: {}",
                    panic_message(payload)
                );
                if let Some(ready_tx) = ready_tx.take() {
                    let _ = ready_tx.send(Err(ScriptStartupError::Panic(message)));
                } else {
                    status.record_failure(message);
                    panic_interrupt.interrupt();
                    let _ = panic_render_tx.send(RenderControl::Shutdown);
                }
            }
            let _ = stopped_tx.send(());
        },
    )?;
    Ok(ScriptThreadHandle {
        ready: ready_rx,
        stopped: stopped_rx,
    })
}

pub(crate) fn scripts_thread(
    mut world: World,
    config: ScriptThreadConfig,
    ready_tx: &mut Option<mpsc::SyncSender<Result<(), ScriptStartupError>>>,
) {
    let ScriptThreadConfig {
        scene_name,
        save_path,
        enabled_scenes,
        sources,
        input_settings,
        scripts_library,
        #[cfg(test)]
        script_module,
        interrupt,
        clock,
        background_tick_hz,
        gamepad_devices,
        control_rx,
        frame_tx,
        render_tx,
        status,
        input,
        cursor,
        #[cfg(feature = "dev-console")]
        command_catalog,
        physics_client,
        physics_initialization,
        audio,
        prefabs,
        physics_geometry,
        prefab_loader,
        render_resources,
        network,
        frame_limit,
        ui,
        ui_text_focus,
        scene_loader,
        gpu_render,
        platform,
        ..
    } = config;
    let library = match load_script_library(
        scripts_library.as_deref(),
        #[cfg(test)]
        script_module,
    ) {
        Ok(library) => library,
        Err(error) => {
            if let Some(ready_tx) = ready_tx.take() {
                let _ = ready_tx.send(Err(ScriptStartupError::Load(error)));
            }
            return;
        }
    };
    let pad_tuning = gamepad_tuning(&input_settings.gamepad);
    let mut scripts = match ScriptHost::load_with_options(
        &world,
        &scene_name,
        &sources,
        library.as_ref(),
        ScriptLoadOptions {
            interrupt: interrupt.clone(),
            call_time_limit: Some(SCRIPT_CALL_TIME_LIMIT),
            input: input_settings,
            prefabs: prefabs.catalog.clone(),
        },
    ) {
        Ok(scripts) => scripts,
        Err(error) => {
            if let Some(ready_tx) = ready_tx.take() {
                let _ = ready_tx.send(Err(ScriptStartupError::Load(error.to_string())));
            }
            return;
        }
    };
    if let Some(path) = save_path
        && let Err(error) = scripts.load_save_file(&path)
    {
        log::warn!("save data is unavailable at {}: {error}", path.display());
    }
    if let Err(error) = scripts.set_enabled_scenes(&enabled_scenes) {
        if let Some(ready_tx) = ready_tx.take() {
            let _ = ready_tx.send(Err(ScriptStartupError::Load(error)));
        }
        return;
    }
    for (path, template) in &prefabs.templates {
        if let Err(error) = scripts.register_prefab(&world, path, Arc::clone(template), &sources) {
            if let Some(ready_tx) = ready_tx.take() {
                let _ = ready_tx.send(Err(ScriptStartupError::Load(error.to_string())));
            }
            return;
        }
    }
    let mut ui_registry = UiRegistry::default();
    if let Err(error) = ui_registry.register(&mut scripts, &ui) {
        if let Some(ready_tx) = ready_tx.take() {
            let _ = ready_tx.send(Err(ScriptStartupError::Load(error)));
        }
        return;
    }
    let mut ui_input = UiInputBridge::new(ui_text_focus);
    let mut spawn_physics = SpawnPhysics {
        geometry: physics_geometry,
        ..SpawnPhysics::default()
    };
    let (fixed_hz, max_substeps) = physics_initialization
        .as_ref()
        .map_or((60, 8), |initialization| {
            (initialization.fixed_hz, initialization.max_substeps)
        });
    if let Err(error) = scripts.set_fixed_step_config(fixed_hz, max_substeps) {
        if let Some(ready_tx) = ready_tx.take() {
            let _ = ready_tx.send(Err(ScriptStartupError::Physics(error)));
        }
        return;
    }
    let mut physics_client = physics_client;
    let mut physics_sync = None;
    match (physics_client.as_mut(), physics_initialization) {
        (Some(_), Some(initialization)) => {
            let sync = match PhysicsSceneSync::new(&world, initialization.bindings) {
                Ok(sync) => sync,
                Err(error) => {
                    if let Some(ready_tx) = ready_tx.take() {
                        let _ = ready_tx.send(Err(ScriptStartupError::Physics(error.to_string())));
                    }
                    return;
                }
            };
            scripts.set_physics_snapshot(initialization.result.snapshot, sync.bindings());
            physics_sync = Some(sync);
        }
        (None, None) => {}
        _ => {
            if let Some(ready_tx) = ready_tx.take() {
                let _ = ready_tx.send(Err(ScriptStartupError::Physics(
                    "physics client and initialization must be provided together".to_owned(),
                )));
            }
            return;
        }
    }
    // Мост подключается до `start`: `on_start` уже видит режим сети и сетевые id объектов сцены.
    let mut network: Option<ScriptNetwork> = match network {
        Some(session) => match net::attach(session, &mut world, &mut scripts) {
            Ok(bridge) => Some(bridge),
            Err(error) => {
                if let Some(ready_tx) = ready_tx.take() {
                    let _ = ready_tx.send(Err(ScriptStartupError::Network(error.to_string())));
                }
                return;
            }
        },
        None => None,
    };
    if let Some(ready_tx) = ready_tx.take()
        && ready_tx.send(Ok(())).is_err()
    {
        return;
    }

    let mut background = false;
    let mut applied_background = false;
    #[cfg(feature = "dev-console")]
    let mut pending_console_commands = PendingConsoleCommands::default();
    let material_resources = render_resources.clone();
    let mut prefab_loads = PrefabLoads::new(prefab_loader, render_resources);
    let mut pending_prefab = VecDeque::new();
    let mut material_sync = MaterialSync::default();
    let mut frame_latency: Option<Arc<dyn FrameLatency>> = None;
    let gpu_scene = loop {
        match control_rx.recv() {
            Ok(ScriptControl::Latency(latency)) => frame_latency = Some(latency),
            Ok(ScriptControl::Go(scene)) => break Some(*scene),
            Ok(ScriptControl::Shutdown) | Err(_) => break None,
            Ok(ScriptControl::Background(value)) => {
                background = value;
                sync_input_background(&input, background, &mut applied_background);
            }
            #[cfg(feature = "dev-console")]
            Ok(ScriptControl::Console(command)) => pending_console_commands.push_back(command),
            Ok(ScriptControl::Prefab(reply)) => {
                pending_prefab.push_back(ScriptReply::Prefab(reply))
            }
            Ok(ScriptControl::Scene(reply)) => {
                pending_prefab.push_back(ScriptReply::Scene(reply));
            }
            Ok(ScriptControl::Material(reply)) => {
                pending_prefab.push_back(ScriptReply::Material(reply))
            }
        }
    };
    let Some(mut gpu_scene) = gpu_scene else {
        scripts.destroy(&mut world, ScriptTime::default());
        print_script_events(scripts.drain_events());
        return;
    };

    sync_input_background(&input, background, &mut applied_background);

    let mut timings = PhaseWindow::new(
        SOURCE_SCRIPTS,
        Arc::clone(&status.profile),
        scripts_groups(),
    );
    let mut scene_switch = SceneSwitch::new(SceneGpuIds::of_table(&gpu_scene));
    let mut frame_number = 0_u64;
    let mut last_tick = clock.now();
    let mut sim_time = 0.0_f64;
    let mut unscaled_sim_time = 0.0_f64;
    let start_time = ScriptTime {
        elapsed: 0.0,
        frame: frame_number,
    };
    if let Some(handle) = audio.as_ref() {
        preload_prefab_sounds(handle, &prefabs.templates);
    }
    let mut published_cursor = None;
    set_initial_cursor(&mut scripts, &world, &cursor, &mut published_cursor);
    scripts.start(&mut world, start_time);
    let mut particle_commands = scripts.drain_particle_commands();
    print_script_events(scripts.drain_events());
    publish_script_cursor(&scripts, &cursor, &mut published_cursor);
    let mut audio = audio.map(engine_audio::AudioBridge::new);
    if let Some(audio) = audio.as_mut() {
        // Матрицы до первого кадра не посчитаны; play_on_start берёт позиции из них.
        world.recalculate_world_matrices();
        audio.start(&world);
        audio.apply(scripts.drain_audio_requests(), &world);
    }
    let mut animation = build_animation(&world, &gpu_scene);
    // История выбора LOD активной камеры для гистерезиса между кадрами.
    let mut lod_selector = LodSelector::default();
    drop_mismatched_skins(&world, &mut gpu_scene, &animation);
    animation.apply(scripts.drain_animation_requests());
    let mut spawned_roots = apply_world_changes(
        &mut world,
        &mut scripts,
        physics_sync.as_mut(),
        &mut spawn_physics,
        &mut gpu_scene,
        &mut prefab_loads,
        audio.as_mut(),
    );
    net::tick(&mut network, &mut world, &mut scripts, &mut spawned_roots);
    #[cfg(feature = "dev-console")]
    let mut published_catalog_generation = u64::MAX;
    #[cfg(feature = "dev-console")]
    publish_command_catalog(
        &scripts,
        &command_catalog,
        &mut published_catalog_generation,
    );
    let mut gamepads = if gamepad_devices {
        engine_input::GilrsBackend::new(pad_tuning)
    } else {
        engine_input::GilrsBackend::without_devices(pad_tuning)
    };
    let interval = Duration::from_secs_f64(1.0 / background_tick_hz.max(1) as f64);
    let now = clock.now();
    let mut next_background_tick =
        now.saturating_add(if background { interval } else { Duration::ZERO });
    loop {
        timings.publish_if_due(Instant::now());
        if frame_limit.is_some_and(|limit| frame_number >= limit) {
            break;
        }
        let mut stop_after_physics = false;
        if let (Some(client), Some(sync)) = (&mut physics_client, &mut physics_sync) {
            let physics_started = Instant::now();
            let received =
                receive_physics_interruptibly(client, &control_rx, |control| match control {
                    ScriptControl::Shutdown => true,
                    ScriptControl::Background(value) => {
                        background = value;
                        if value {
                            next_background_tick = clock.now().saturating_add(interval);
                        }
                        false
                    }
                    ScriptControl::Latency(latency) => {
                        frame_latency = Some(latency);
                        false
                    }
                    ScriptControl::Go(_) => false,
                    ScriptControl::Prefab(reply) => {
                        pending_prefab.push_back(ScriptReply::Prefab(reply));
                        false
                    }
                    ScriptControl::Scene(reply) => {
                        pending_prefab.push_back(ScriptReply::Scene(reply));
                        false
                    }
                    ScriptControl::Material(reply) => {
                        pending_prefab.push_back(ScriptReply::Material(reply));
                        false
                    }
                    #[cfg(feature = "dev-console")]
                    ScriptControl::Console(command) => {
                        pending_console_commands.push_back(command);
                        false
                    }
                });
            timings.record(
                script_phase::LOOP,
                script_phase::PHYSICS_WAIT,
                physics_started.elapsed(),
            );
            match received {
                Ok(PhysicsWait::Idle) => {}
                Ok(PhysicsWait::Step(result)) => {
                    if let Err(error) = apply_physics_step(&mut scripts, &mut world, sync, result) {
                        status.record_failure(format!("ошибка синхронизации физики: {error}"));
                        let _ = render_tx.send(RenderControl::Shutdown);
                        stop_after_physics = true;
                    }
                }
                Ok(PhysicsWait::Shutdown) => {
                    stop_after_physics = true;
                }
                Err(error) => {
                    status.record_failure(format!("ошибка потока физики: {error}"));
                    let _ = render_tx.send(RenderControl::Shutdown);
                    stop_after_physics = true;
                }
            }
        }
        if stop_after_physics {
            break;
        }
        sync_input_background(&input, background, &mut applied_background);
        if background {
            ui_input.suspend(&mut scripts);
            if !wait_background_tick(
                &control_rx,
                clock.as_ref(),
                &mut background,
                next_background_tick,
                #[cfg(feature = "dev-console")]
                &mut pending_console_commands,
                #[cfg(not(feature = "dev-console"))]
                &mut (),
                &mut pending_prefab,
            ) {
                break;
            }
            if !background {
                continue;
            }
        }
        if !background {
            let mut should_stop = false;
            let mut entered_background = false;
            loop {
                match control_rx.try_recv() {
                    Ok(ScriptControl::Shutdown) | Err(TryRecvError::Disconnected) => {
                        should_stop = true;
                        break;
                    }
                    Ok(ScriptControl::Background(true)) => {
                        background = true;
                        next_background_tick = clock.now().saturating_add(interval);
                        entered_background = true;
                        break;
                    }
                    Ok(ScriptControl::Latency(latency)) => frame_latency = Some(latency),
                    Ok(ScriptControl::Background(false)) | Ok(ScriptControl::Go(_)) => {}
                    Ok(ScriptControl::Prefab(reply)) => {
                        pending_prefab.push_back(ScriptReply::Prefab(reply))
                    }
                    Ok(ScriptControl::Scene(reply)) => {
                        pending_prefab.push_back(ScriptReply::Scene(reply));
                    }
                    Ok(ScriptControl::Material(reply)) => {
                        pending_prefab.push_back(ScriptReply::Material(reply))
                    }
                    #[cfg(feature = "dev-console")]
                    Ok(ScriptControl::Console(command)) => {
                        pending_console_commands.push_back(command);
                    }
                    Err(TryRecvError::Empty) => break,
                }
            }
            if should_stop {
                break;
            }
            if entered_background {
                continue;
            }
        }

        // Reflex: сон и маркеры только в интерактивном режиме; в background кадры без Reflex.
        let reflex = frame_latency.as_deref().filter(|_| !background);
        let next_frame = frame_number.saturating_add(1);
        if let Some(latency) = reflex {
            latency.sleep();
            latency.marker(next_frame, LatencyMarker::SimulationStart);
        }
        let now = clock.now();
        let unscaled_dt = now.saturating_sub(last_tick).as_secs_f32();
        last_tick = now;
        frame_number = frame_number.saturating_add(1);
        let frame_time = ScriptFrameTime::advance(
            unscaled_dt,
            scripts.time_scale(),
            sim_time,
            unscaled_sim_time,
            scripts.fixed_dt(),
            frame_number,
        );
        sim_time = frame_time.elapsed;
        unscaled_sim_time = frame_time.unscaled_elapsed;
        let dt = frame_time.dt;
        let time = ScriptTime {
            elapsed: frame_time.elapsed,
            frame: frame_time.frame,
        };
        let console_started = Instant::now();
        #[cfg(feature = "dev-console")]
        {
            for (name, error) in
                execute_console_commands(&mut scripts, &mut world, &mut pending_console_commands)
            {
                log::error!(target: "console", "command {name} failed: {error}");
            }
            spawned_roots.extend(apply_world_changes(
                &mut world,
                &mut scripts,
                physics_sync.as_mut(),
                &mut spawn_physics,
                &mut gpu_scene,
                &mut prefab_loads,
                audio.as_mut(),
            ));
            publish_script_cursor(&scripts, &cursor, &mut published_cursor);
            publish_command_catalog(
                &scripts,
                &command_catalog,
                &mut published_catalog_generation,
            );
        }
        timings.record(
            script_phase::LOOP,
            script_phase::CONSOLE_COMMANDS,
            console_started.elapsed(),
        );
        // Ответы префабов регистрируют шаблон и ставят отложенные spawn в очередь; их применяет
        // ближайший `update`, поэтому обработка стоит прямо перед ним.
        let prefab_started = Instant::now();
        drain_prefab_replies(
            &mut pending_prefab,
            &mut scripts,
            &mut world,
            &mut gpu_scene,
            &mut spawn_physics,
            &mut prefab_loads,
            &mut material_sync,
            &mut ui_registry,
            &mut scene_switch,
        );
        material_sync.apply_replies(&world, &mut gpu_scene, &material_resources, frame_number);
        timings.record(
            script_phase::LOOP,
            script_phase::PREFAB_REPLIES,
            prefab_started.elapsed(),
        );
        if let Some(latency) = reflex {
            latency.marker(frame_number, LatencyMarker::InputSample);
        }
        net::receive(&mut network, &mut world, &mut scripts);
        gamepads.poll();
        let (input_frame, accepting_input, text, screen) = {
            let mut accumulator = lock_input(&input);
            let accepting = accumulator.set_pads(&gamepads.snapshot());
            (
                accumulator.take_frame(),
                accepting,
                accumulator.take_text(),
                accumulator.window_size(),
            )
        };
        if !accepting_input {
            gamepads.stop_rumble();
        }
        let ui_started = Instant::now();
        let pointer = Some(input_frame.cursor());
        let input_frame = ui_input.tick(
            &mut scripts,
            input_frame,
            UiTick {
                accepting: accepting_input,
                pointer,
                screen_px: screen,
                text,
            },
            time.elapsed,
        );
        timings.record(script_phase::LOOP, script_phase::UI, ui_started.elapsed());
        scripts.set_input(input_frame);
        platform.before_update(&mut scripts);
        let update_started = Instant::now();
        scripts.update_frame(&mut world, frame_time);
        platform.after_update(&mut scripts);
        if scripts.take_quit_request() {
            break;
        }
        if let Some(path) = scripts.take_scene_request() {
            if network.is_some() {
                scripts.report_scene_failure(
                    &path,
                    "смена сцены при активной сети пока не поддерживается",
                );
            } else {
                match scene_switch.request(scene_loader.as_ref(), &path) {
                    Ok(upload) if gpu_render => {
                        prefab_loads.send_render(RenderResourceRequest::Scene(Box::new(upload)));
                    }
                    Ok(upload) => scene_switch.accept(SceneUploadReply {
                        generation: upload.generation,
                        result: Ok(Box::default()),
                    }),
                    Err(error) => scripts.report_scene_failure(&path, &error),
                }
            }
        }
        let rumble = scripts.take_rumble_requests();
        if accepting_input {
            gamepads.rumble_frame(
                rumble
                    .iter()
                    .map(|request| (request.pad, request.strong, request.weak, request.seconds)),
            );
        }
        publish_script_cursor(&scripts, &cursor, &mut published_cursor);
        timings.record(
            script_phase::LOOP,
            script_phase::SCRIPTS_UPDATE,
            update_started.elapsed(),
        );
        record_update_profile(&mut timings, scripts.last_update_profile());
        print_script_events(scripts.drain_events());
        if let Some(audio) = audio.as_mut() {
            audio.apply(scripts.drain_audio_requests(), &world);
        }
        animation.apply(scripts.drain_animation_requests());
        particle_commands.extend(scripts.drain_particle_commands());
        let finished = animation.advance(dt);
        scripts.queue_animation_events(
            &world,
            finished.into_iter().map(|event| match event {
                AnimationEvent::Finished { object, motion } => (object, motion),
            }),
        );
        log_animation_warnings(&mut animation);
        let world_changes_started = Instant::now();
        net::apply(&mut network, &mut world, &mut scripts);
        for (path, error) in scene_switch.apply(SceneCommit {
            world: &mut world,
            scripts: &mut scripts,
            gpu_scene: &mut gpu_scene,
            spawn_physics: &mut spawn_physics,
            physics: physics_sync.as_mut(),
            prefabs: &mut prefab_loads,
            audio: audio.as_mut(),
            animation: &mut animation,
            ui_registry: &mut ui_registry,
            time: ScriptTime {
                elapsed: clock.now().as_secs_f64(),
                frame: frame_number,
            },
            sequence: frame_number,
        }) {
            scripts.report_scene_failure(&path, &error);
        }
        print_script_events(scripts.drain_events());
        spawned_roots.extend(apply_world_changes(
            &mut world,
            &mut scripts,
            physics_sync.as_mut(),
            &mut spawn_physics,
            &mut gpu_scene,
            &mut prefab_loads,
            audio.as_mut(),
        ));
        net::tick(&mut network, &mut world, &mut scripts, &mut spawned_roots);
        material_sync.request_changes(&mut world, &gpu_scene, &material_resources);
        timings.record(
            script_phase::LOOP,
            script_phase::WORLD_CHANGES,
            world_changes_started.elapsed(),
        );
        #[cfg(feature = "dev-console")]
        publish_command_catalog(
            &scripts,
            &command_catalog,
            &mut published_catalog_generation,
        );
        let sync_started = Instant::now();
        if let (Some(client), Some(sync)) = (&mut physics_client, &mut physics_sync) {
            let mut commands = scripts.drain_physics_commands();
            match sync.sync_world_commands(&mut world) {
                Ok(world_commands) => {
                    commands.extend(world_commands);
                    for warning in sync.take_warnings() {
                        log::warn!("[physics] привязка не обновлена: {warning}");
                    }
                }
                Err(error) => {
                    status.record_failure(format!("ошибка синхронизации сцены с физикой: {error}"));
                    let _ = render_tx.send(RenderControl::Shutdown);
                    break;
                }
            }
            commands.append(&mut spawn_physics.velocities);
            let changes = std::mem::take(&mut spawn_physics.changes);
            if let Err(error) = client.step_with_changes(dt, changes, commands) {
                // Остановка выставляет interrupt до Shutdown физики: шаг, отправленный уже
                // после выхода сервера, — ожидаемое завершение, а не сбой.
                if !interrupt.is_interrupted() {
                    status.record_failure(format!("ошибка шага физики: {error}"));
                }
                let _ = render_tx.send(RenderControl::Shutdown);
                break;
            }
        }
        timings.record(
            script_phase::LOOP,
            script_phase::PHYSICS_SYNC,
            sync_started.elapsed(),
        );
        if background {
            next_background_tick = now.saturating_add(interval);
            continue;
        }

        let build_started = Instant::now();
        let (scene_frame, warnings) = world.build_frame();
        timings.record(
            script_phase::LOOP,
            script_phase::BUILD_FRAME,
            build_started.elapsed(),
        );
        for warning in warnings {
            log::warn!(
                "[scene] {}: объект {}: {}",
                warning.source,
                warning.object_id,
                warning.message
            );
        }
        if let Some(audio) = audio.as_mut() {
            audio.sync_positions(&world);
        }
        let ui_started = Instant::now();
        let ui_paint = scripts.ui_mut().paint();
        timings.record(script_phase::LOOP, script_phase::UI, ui_started.elapsed());
        let snapshot_started = Instant::now();
        let made = make_snapshot(
            scene_frame,
            &world,
            &gpu_scene,
            &animation,
            &mut lod_selector,
        )
        .map(|mut snapshot| {
            snapshot.sim_time = sim_time;
            snapshot.particle_commands = std::mem::take(&mut particle_commands);
            snapshot.ui = ui_paint;
            snapshot.screen_shaders.time = screen_shader_time(sim_time);
            snapshot
        });
        lod_selector.finish_frame();
        timings.record(
            script_phase::LOOP,
            script_phase::MAKE_SNAPSHOT,
            snapshot_started.elapsed(),
        );
        let snapshot = match made {
            Ok(mut snapshot) => {
                snapshot.sequence = frame_number;
                if let Some(latency) = reflex {
                    latency.marker(frame_number, LatencyMarker::SimulationEnd);
                }
                Arc::new(snapshot)
            }
            Err(error) => {
                status.record_failure(error);
                break;
            }
        };
        let send_started = Instant::now();
        let sent = send_snapshot(
            &frame_tx,
            &control_rx,
            snapshot,
            &mut background,
            #[cfg(feature = "dev-console")]
            &mut pending_console_commands,
            #[cfg(not(feature = "dev-console"))]
            &mut (),
            &mut pending_prefab,
        );
        timings.record(
            script_phase::LOOP,
            script_phase::SNAPSHOT_SEND_WAIT,
            send_started.elapsed(),
        );
        match sent {
            SendOutcome::Sent => prefab_loads.set_last_sent_seq(frame_number),
            SendOutcome::Background => {
                next_background_tick = clock.now().saturating_add(interval);
                continue;
            }
            SendOutcome::Shutdown | SendOutcome::Disconnected => break,
        }
    }

    scripts.destroy(
        &mut world,
        ScriptTime {
            elapsed: clock.now().as_secs_f64(),
            frame: frame_number,
        },
    );
    print_script_events(scripts.drain_events());
    #[cfg(feature = "dev-console")]
    publish_command_catalog(
        &scripts,
        &command_catalog,
        &mut published_catalog_generation,
    );
    let _ = render_tx.send(RenderControl::Shutdown);
}

pub(crate) fn gamepad_tuning(
    settings: &engine_project::GamepadSettings,
) -> engine_input::GamepadTuning {
    engine_input::GamepadTuning {
        stick_deadzone: settings.stick_deadzone,
        trigger_threshold: settings.trigger_threshold,
    }
}

pub(crate) fn sync_input_background(
    input: &Mutex<InputAccumulator>,
    background: bool,
    applied_background: &mut bool,
) {
    if *applied_background != background {
        lock_input(input).set_background(background);
        *applied_background = background;
    }
}

pub(crate) fn load_script_library(
    path: Option<&Path>,
    #[cfg(test)] module: Option<&'static rve_sdk::abi::RveModule>,
) -> Result<Option<ScriptLibrary>, String> {
    #[cfg(test)]
    if let Some(module) = module {
        // SAFETY: test modules are generated by `rve_sdk::script_module!` and live for the process.
        return unsafe { ScriptLibrary::from_module(std::ptr::from_ref(module)) }
            .map(Some)
            .map_err(|error| error.to_string());
    }
    let Some(path) = path else {
        return Ok(None);
    };
    // SAFETY: script libraries are native code supplied by the project being run.
    unsafe { ScriptLibrary::load(path) }
        .map(Some)
        .map_err(|error| error.to_string())
}

pub(crate) enum SendOutcome {
    Sent,
    Background,
    Shutdown,
    Disconnected,
}

pub(crate) enum ScriptSelection {
    Sent(Result<(), flume::SendError<Arc<FrameSnapshot>>>),
    Control(Result<ScriptControl, flume::RecvError>),
}

pub(crate) fn send_snapshot(
    frame_tx: &Sender<Arc<FrameSnapshot>>,
    control_rx: &Receiver<ScriptControl>,
    snapshot: Arc<FrameSnapshot>,
    background: &mut bool,
    #[cfg(feature = "dev-console")] pending_console_commands: &mut PendingConsoleCommands,
    #[cfg(not(feature = "dev-console"))] _: &mut PendingConsoleCommands,
    pending_prefab: &mut VecDeque<ScriptReply>,
) -> SendOutcome {
    loop {
        let selection = Selector::new()
            .send(frame_tx, snapshot.clone(), ScriptSelection::Sent)
            .recv(control_rx, ScriptSelection::Control)
            .wait();
        match selection {
            ScriptSelection::Sent(Ok(())) => return SendOutcome::Sent,
            ScriptSelection::Sent(Err(_)) | ScriptSelection::Control(Err(_)) => {
                return SendOutcome::Disconnected;
            }
            ScriptSelection::Control(Ok(ScriptControl::Shutdown)) => return SendOutcome::Shutdown,
            ScriptSelection::Control(Ok(ScriptControl::Background(value))) => {
                *background = value;
                if value {
                    return SendOutcome::Background;
                }
            }
            #[cfg(feature = "dev-console")]
            ScriptSelection::Control(Ok(ScriptControl::Console(command))) => {
                pending_console_commands.push_back(command);
            }
            // Latency и Go приходят только до первого кадра, поэтому здесь они не нужны.
            ScriptSelection::Control(Ok(ScriptControl::Go(_) | ScriptControl::Latency(_))) => {}
            ScriptSelection::Control(Ok(ScriptControl::Prefab(reply))) => {
                pending_prefab.push_back(ScriptReply::Prefab(reply));
            }
            ScriptSelection::Control(Ok(ScriptControl::Scene(reply))) => {
                pending_prefab.push_back(ScriptReply::Scene(reply));
            }
            ScriptSelection::Control(Ok(ScriptControl::Material(reply))) => {
                pending_prefab.push_back(ScriptReply::Material(reply));
            }
        }
    }
}

pub(crate) fn wait_background_tick(
    control_rx: &Receiver<ScriptControl>,
    clock: &dyn Clock,
    background: &mut bool,
    deadline: Duration,
    #[cfg(feature = "dev-console")] pending_console_commands: &mut PendingConsoleCommands,
    #[cfg(not(feature = "dev-console"))] _: &mut PendingConsoleCommands,
    pending_prefab: &mut VecDeque<ScriptReply>,
) -> bool {
    while *background {
        let now = clock.now();
        if now >= deadline {
            return true;
        }
        match clock.wait_for_control(control_rx, deadline - now) {
            Ok(ScriptControl::Shutdown) | Err(RecvTimeoutError::Disconnected) => return false,
            Ok(ScriptControl::Background(value)) => {
                *background = value;
                if !*background {
                    return true;
                }
            }
            #[cfg(feature = "dev-console")]
            Ok(ScriptControl::Console(command)) => pending_console_commands.push_back(command),
            Ok(ScriptControl::Prefab(reply)) => {
                pending_prefab.push_back(ScriptReply::Prefab(reply))
            }
            Ok(ScriptControl::Scene(reply)) => {
                pending_prefab.push_back(ScriptReply::Scene(reply));
            }
            Ok(ScriptControl::Material(reply)) => {
                pending_prefab.push_back(ScriptReply::Material(reply))
            }
            Ok(ScriptControl::Go(_) | ScriptControl::Latency(_))
            | Err(RecvTimeoutError::Timeout) => {}
        }
    }
    true
}
