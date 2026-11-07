//! Связка сцены и звуковой подсистемы: источники объектов, запросы скриптов, позиции и слушатель.
//!
//! Используется и runtime, и редактором в Play-режиме. Мир передаётся по ссылке в каждый вызов;
//! объектный id бридж хранит как [`ObjectHandle`], поэтому после замены мира его нужно создать заново.

use crate::{AudioError, AudioEvent, AudioHandle, Distances, PlayRequest};
use engine_scene::world::ObjectHandle;
use engine_scene::{AudioRequest, AudioSourceComponent, VoiceId, World};
use std::collections::{HashMap, HashSet};

/// Расстояния клипов скриптов, запущенных с позицией.
pub const SCRIPT_DISTANCES: Distances = Distances {
    min: 1.0,
    max: 30.0,
};

pub struct AudioBridge {
    handle: AudioHandle,
    /// `[[assets]]` сцены; клип вне набора проигрывается, но получает предупреждение.
    declared: HashSet<String>,
    /// Текущий голос каждого источника.
    sources: HashMap<ObjectHandle, VoiceId>,
    /// Громкость, заданная `set_sound_volume`; применяется к текущему и следующим запускам.
    volumes: HashMap<ObjectHandle, f32>,
    /// Позиции излучателей, переданные звуковому потоку в последнем `sync_positions`.
    emitters: HashMap<ObjectHandle, [f32; 3]>,
    /// Команды, которые очередь звука отклонила за текущий вызов; сворачиваются в одно предупреждение.
    rejected: Vec<String>,
    /// Пути вне `[[assets]]`, о которых уже предупреждали: предупреждение один раз на путь.
    undeclared_warned: HashSet<String>,
}

impl AudioBridge {
    pub fn new(handle: AudioHandle) -> Self {
        Self {
            handle,
            declared: HashSet::new(),
            sources: HashMap::new(),
            volumes: HashMap::new(),
            emitters: HashMap::new(),
            rejected: Vec::new(),
            undeclared_warned: HashSet::new(),
        }
    }

    /// Запоминает `[[assets]]`, предзагружает клипы источников и запускает `play_on_start`.
    /// Позиции берутся из мировых матриц: вызывающий пересчитывает их до `start`.
    pub fn start(&mut self, world: &World) {
        self.declared = world
            .asset_paths()
            .iter()
            .map(|path| path.as_str().to_owned())
            .collect();
        let mut clips: Vec<String> = world
            .objects()
            .iter()
            .filter_map(|object| object.audio_source.as_ref())
            .map(|audio| audio.clip.as_str().to_owned())
            .collect();
        clips.sort();
        clips.dedup();
        if let Err(error) = self.handle.preload(clips) {
            log::warn!("предзагрузка звука: {error}");
        }
        let starts: Vec<AudioRequest> = world
            .objects()
            .iter()
            .filter(|object| {
                object
                    .audio_source
                    .as_ref()
                    .is_some_and(|audio| audio.play_on_start)
            })
            .map(|object| AudioRequest::PlaySource {
                object: object.handle,
                voice: VoiceId::allocate(),
            })
            .collect();
        self.apply(starts, world);
    }

    /// Применяет запросы скриптов за кадр.
    pub fn apply(&mut self, requests: impl IntoIterator<Item = AudioRequest>, world: &World) {
        self.poll_events();
        for request in requests {
            match request {
                AudioRequest::PlaySource { object, voice } => {
                    self.play_source(object, voice, world)
                }
                AudioRequest::StopSource { object } => {
                    if let Some(&voice) = self.sources.get(&object)
                        && self.stop_voice(voice, "остановка источника")
                    {
                        self.sources.remove(&object);
                    }
                }
                AudioRequest::SetSourceVolume { object, volume } => {
                    self.volumes.insert(object, volume);
                    if let Some(&voice) = self.sources.get(&object) {
                        let result = self.handle.set_voice_volume(voice, volume);
                        self.note(result, "громкость источника");
                    }
                }
                AudioRequest::PlayClip {
                    voice,
                    path,
                    params,
                } => {
                    if !self.declared.contains(path.as_str())
                        && self.undeclared_warned.insert(path.as_str().to_owned())
                    {
                        log::warn!(
                            "клип /{} не объявлен в [[assets]]: в release он не будет упакован",
                            path.as_str()
                        );
                    }
                    let request = PlayRequest {
                        group: params.group,
                        volume: params.volume,
                        looping: params.looping,
                        spatial: params.position.map(|_| SCRIPT_DISTANCES),
                    };
                    let result = match params.position {
                        Some(position) => {
                            self.handle
                                .play_at(voice, path.as_str(), request, position)
                        }
                        None => self.handle.play(voice, path.as_str(), request),
                    };
                    self.note(result, &format!("воспроизведение /{}", path.as_str()));
                }
                AudioRequest::StopVoice { voice } => {
                    self.stop_voice(voice, "остановка голоса");
                }
                AudioRequest::SetGroupVolume { group, volume } => {
                    let result = self.handle.set_group_volume(group, volume);
                    self.note(result, "громкость группы");
                }
            }
        }
        self.flush_rejected();
    }

    fn play_source(&mut self, object: ObjectHandle, voice: VoiceId, world: &World) {
        let Some(audio) = world.audio_source(object) else {
            log::warn!("объект без audio_source не может воспроизвести звук");
            return;
        };
        if let Some(&current) = self.sources.get(&object) {
            if audio.looping {
                return;
            }
            // Старый голос остаётся в таблице, пока не остановлен: иначе два голоса звучали бы разом.
            if !self.stop_voice(current, "перезапуск источника") {
                return;
            }
            self.sources.remove(&object);
        }
        let volume = self.volumes.get(&object).copied().unwrap_or(audio.volume);
        let request = PlayRequest {
            group: audio.group,
            volume,
            looping: audio.looping,
            spatial: spatial_distances(audio),
        };
        let result = match source_position(world, object, audio) {
            Some(position) => self
                .handle
                .play_at(voice, audio.clip.as_str(), request, position),
            None => self.handle.play(voice, audio.clip.as_str(), request),
        };
        if result.is_ok() {
            self.sources.insert(object, voice);
        }
        self.note(result, &format!("воспроизведение источника {}", audio.clip.as_str()));
    }

    /// Останавливает голос. При отказе очереди голос остаётся в таблице и помечается отказом.
    fn stop_voice(&mut self, voice: VoiceId, action: &str) -> bool {
        let result = self.handle.stop(voice);
        let stopped = result.is_ok();
        self.note(result, action);
        stopped
    }

    /// Запоминает отказ очереди; предупреждение выводится одно на вызов `apply`/`sync_positions`.
    fn note(&mut self, result: Result<(), AudioError>, action: &str) {
        if let Err(error) = result {
            self.rejected.push(format!("{action}: {error}"));
        }
    }

    fn flush_rejected(&mut self) {
        let Some(first) = self.rejected.first() else {
            return;
        };
        log::warn!(
            "звуковых команд отклонено: {}; первая: {first}",
            self.rejected.len()
        );
        self.rejected.clear();
    }

    /// Пишет позиции излучателей и слушателя по мировым матрицам. Исчезнувший объект останавливается.
    /// Вызывать после пересчёта мировых матриц.
    pub fn sync_positions(&mut self, world: &World) {
        self.poll_events();
        if let Some(camera) = world.world_matrix(world.active_camera()) {
            let position = camera.w_axis.truncate();
            let right = camera.x_axis.truncate().normalize_or_zero();
            if position.is_finite() && right.is_finite() && right.length_squared() > 0.0 {
                self.handle
                    .set_listener(position.to_array(), right.to_array());
            }
        }

        let mut stale = Vec::new();
        for (&object, &voice) in &self.sources {
            let Some(audio) = world.audio_source(object) else {
                stale.push(object);
                continue;
            };
            if !audio.spatial {
                continue;
            }
            match world.world_matrix(object) {
                Some(matrix) => {
                    let position = matrix.w_axis.truncate();
                    if position.is_finite() {
                        self.handle.set_emitter(voice, position.to_array());
                        self.emitters.insert(object, position.to_array());
                    }
                }
                None => stale.push(object),
            }
        }
        for object in stale {
            self.emitters.remove(&object);
            if let Some(&voice) = self.sources.get(&object)
                && self.stop_voice(voice, "остановка исчезнувшего источника")
            {
                self.sources.remove(&object);
            }
        }
        self.flush_rejected();
    }

    /// Последняя позиция излучателя, переданная звуковому потоку. Нужна отладке и тестам Play,
    /// которые не должны зависеть от момента обработки команд потоком.
    pub fn emitter_of(&self, object: ObjectHandle) -> Option<[f32; 3]> {
        self.emitters.get(&object).copied()
    }

    fn poll_events(&mut self) {
        for event in self.handle.poll_events() {
            let AudioEvent::VoiceEnded(voice) = event;
            self.sources.retain(|_, current| *current != voice);
        }
    }
}

/// Позиция пространственного источника по последним мировым матрицам; `None` для неспециальных
/// источников и для матриц, которые не удалось прочитать.
fn source_position(
    world: &World,
    object: ObjectHandle,
    audio: &AudioSourceComponent,
) -> Option<[f32; 3]> {
    if !audio.spatial {
        return None;
    }
    let position = world.world_matrix(object)?.w_axis.truncate();
    position.is_finite().then(|| position.to_array())
}

fn spatial_distances(audio: &AudioSourceComponent) -> Option<Distances> {
    audio.spatial.then_some(Distances {
        min: audio.min_distance,
        max: audio.max_distance,
    })
}

#[cfg(test)]
mod tests;
