//! Эмиттеры частиц префабов и шаг частиц между кадрами.

use super::*;

/// Создаёт текстуры эмиттеров тяжёлого префаба и их описания. Объекты с одним путём текстуры
/// делят её. При ошибке созданные текстуры уничтожаются.
pub(crate) fn create_prefab_particles(
    renderer: &mut Renderer,
    textures: &HashMap<String, TextureData>,
    particles: &[Option<ParticleSource>],
) -> Result<PrefabParticles, RenderError> {
    let mut out = PrefabParticles::default();
    let mut by_path = HashMap::<String, TextureId>::new();
    let mut descs = Vec::with_capacity(particles.len());
    let built = (|| -> Result<(), RenderError> {
        for source in particles {
            let Some(source) = source else {
                descs.push(None);
                continue;
            };
            let texture = match &source.texture {
                Some(path) => match by_path.get(path) {
                    Some(id) => Some(*id),
                    None => {
                        let data = textures.get(path).ok_or(RenderError::InvalidState(
                            "CPU-текстура частиц отсутствует в таблице подготовленных ресурсов",
                        ))?;
                        let id = renderer.create_texture_with_color_space(
                            data.width,
                            data.height,
                            &data.rgba,
                            TextureColorSpace::Srgb,
                        )?;
                        out.textures.push(id);
                        by_path.insert(path.clone(), id);
                        Some(id)
                    }
                },
                None => None,
            };
            descs.push(Some(particle_emitter_desc(&source.emitter, texture)));
        }
        Ok(())
    })();
    match built {
        Ok(()) => {
            out.descs = Some(descs);
            Ok(out)
        }
        Err(error) => {
            release_prefab_particles(renderer, out.textures);
            Err(error)
        }
    }
}

pub(crate) fn release_prefab_particles(renderer: &mut Renderer, textures: Vec<TextureId>) {
    for texture in textures {
        if let Err(error) = renderer.destroy_texture(texture) {
            log::error!("текстура эмиттера частиц не освобождена: {error}");
        }
    }
}

/// Применяет заявку эмиттеров к таблице по хендлам объектов. Create заменяет прежний эмиттер
/// объекта; ошибка создания оставляет объект без эмиттера. Destroy неизвестного хендла ничего
/// не делает, поэтому повтор и поздняя заявка безопасны.
pub(crate) fn apply_emitter_request<E: Copy, R>(
    table: &mut HashMap<ObjectHandle, E>,
    renderer: &mut R,
    request: ParticleRequest,
    mut create: impl FnMut(&mut R, ParticleEmitterDesc) -> Result<E, String>,
    mut destroy: impl FnMut(&mut R, E),
) {
    match request {
        ParticleRequest::Create { object, desc } => {
            if let Some(previous) = table.remove(&object) {
                destroy(renderer, previous);
            }
            match create(renderer, *desc) {
                Ok(id) => {
                    table.insert(object, id);
                }
                Err(error) => log::error!("эмиттер частиц объекта {object:?} не создан: {error}"),
            }
        }
        ParticleRequest::Destroy(objects) => {
            for object in objects {
                if let Some(id) = table.remove(&object) {
                    destroy(renderer, id);
                }
            }
        }
    }
}

/// Заявки создания для объектов поддерева: объект `i` получает описание шаблона `i`; объекты
/// без эмиттера пропускаются.
pub(crate) fn spawn_particle_requests(
    objects: &[ObjectHandle],
    descs: &[Option<ParticleEmitterDesc>],
) -> Vec<ParticleRequest> {
    objects
        .iter()
        .zip(descs)
        .filter_map(|(&object, desc)| {
            Some(ParticleRequest::Create {
                object,
                desc: Box::new(desc.clone()?),
            })
        })
        .collect()
}

/// Переводит команды скриптов в действия над эмиттерами: `None` — перезапуск, `Some(n)` — выброс.
/// Команда объекта без эмиттера пропускается, как и заявки, пришедшие после уничтожения объекта.
pub(crate) fn resolve_particle_commands<E: Copy>(
    emitters: &HashMap<ObjectHandle, E>,
    commands: &[ParticleCommand],
) -> Vec<(E, Option<u32>)> {
    commands
        .iter()
        .filter_map(|command| {
            let (object, burst) = match *command {
                ParticleCommand::Restart { object } => (object, None),
                ParticleCommand::Burst { object, count } => (object, Some(count)),
            };
            Some((*emitters.get(&object)?, burst))
        })
        .collect()
}

/// Шаг симуляции частиц: сколько игрового времени прошло с последнего отправленного кадра.
/// Первый кадр шага не получает, чтобы время загрузки не превратилось в скачок.
pub(crate) fn particle_step(sim_time: f64, last_submitted: Option<f64>) -> f32 {
    last_submitted.map_or(0.0, |last| (sim_time - last).max(0.0) as f32)
}
