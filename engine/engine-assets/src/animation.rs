//! Клипы FBX: локальные TRS-ключи узлов, полученные через `ufbx::bake_anim`.
//!
//! Ключи хранят время в секундах от начала клипа и имена узлов. Сопоставление
//! с костями скелета выполняется в `engine-animation`.

#[cfg(feature = "fbx")]
use crate::skin::SkeletonBuilder;

/// Предел числа ключей на все клипы модели, чтобы не допустить неограниченного роста памяти.
#[cfg(feature = "fbx")]
pub(crate) const KEY_LIMIT: usize = 4 * 1024 * 1024;

/// Ключ дорожки: время в секундах и значение.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ModelKey<T> {
    pub time: f32,
    pub value: T,
}

/// Дорожка одного узла; каналы без ключей берут позу узла из скелета.
#[derive(Clone, Debug, PartialEq)]
pub struct ModelTrack {
    /// Имя узла в FBX; сопоставляется с [`crate::ModelNode::name`] по имени.
    pub node: String,
    pub translation: Vec<ModelKey<[f32; 3]>>,
    /// Кватернионы `[x, y, z, w]`.
    pub rotation: Vec<ModelKey<[f32; 4]>>,
    pub scale: Vec<ModelKey<[f32; 3]>>,
}

/// Именованный маркер клипа с временем в секундах от начала клипа.
#[derive(Clone, Debug, PartialEq)]
pub struct ModelMarker {
    pub name: String,
    pub time: f32,
}

/// Именованный клип с длительностью в секундах.
#[derive(Clone, Debug, PartialEq)]
pub struct ModelAnimation {
    pub name: String,
    pub duration: f32,
    pub tracks: Vec<ModelTrack>,
    pub markers: Vec<ModelMarker>,
    /// Предупреждения уровня FBX-файла, общие для его клипов.
    pub file_marker_warnings: Vec<String>,
    /// Предупреждения, относящиеся к этому клипу.
    pub marker_warnings: Vec<String>,
}

#[derive(Default)]
struct ImportedMarkers {
    stack_names: Vec<String>,
    by_stack: Vec<Vec<ModelMarker>>,
    warnings_by_stack: Vec<Vec<String>>,
    file_warnings: Vec<String>,
}

#[derive(Debug)]
struct ParsedMarker {
    stack: Option<String>,
    name: String,
    time: f32,
}

#[derive(Debug)]
struct MarkerParseError {
    stack: Option<String>,
    message: String,
}

/// Есть ли в файле стек с хотя бы одним ключом. Пустой стек, например `Take 001` статичной
/// модели, анимацией не считается.
#[cfg(feature = "fbx")]
pub(crate) fn has_animated_stack(scene: &ufbx::Scene) -> Result<bool, String> {
    let mut keys = usize::MAX;
    for stack in scene.anim_stacks.iter() {
        let baked = bake_stack(scene, stack, String::new(), &|_| true, &mut keys)?;
        if !baked.tracks.is_empty() {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Запекает стеки модели с ключами; в клип попадают только узлы скелета.
#[cfg(feature = "fbx")]
pub(crate) fn bake_model_animations(
    builder: &SkeletonBuilder,
    scene: &ufbx::Scene,
) -> Result<Vec<ModelAnimation>, String> {
    let mut keys = KEY_LIMIT;
    let imported = import_markers(scene);
    let mut baked_stacks = Vec::new();
    for (index, stack) in scene.anim_stacks.iter().enumerate() {
        let baked = bake_stack(
            scene,
            stack,
            stack.element.name.to_string(),
            &|typed_id| builder.contains(typed_id),
            &mut keys,
        )?;
        if !baked.tracks.is_empty() {
            baked_stacks.push((index, baked));
        }
    }
    Ok(attach_markers(baked_stacks, imported))
}

/// Запекает стеки файла с ключами как клипы; имена — stem файла, если стек один.
#[cfg(feature = "fbx")]
pub(crate) fn bake_clip_file(
    scene: &ufbx::Scene,
    stem: &str,
) -> Result<Vec<ModelAnimation>, String> {
    let mut keys = KEY_LIMIT;
    let single = scene.anim_stacks.len() == 1;
    let imported = import_markers(scene);
    let mut baked_stacks = Vec::new();
    for (index, stack) in scene.anim_stacks.iter().enumerate() {
        let name = if single {
            stem.to_owned()
        } else {
            format!("{stem}:{}", stack.element.name)
        };
        let baked = bake_stack(scene, stack, name, &|_| true, &mut keys)?;
        if !baked.tracks.is_empty() {
            baked_stacks.push((index, baked));
        }
    }
    Ok(attach_markers(baked_stacks, imported))
}

fn attach_markers(
    mut baked_stacks: Vec<(usize, ModelAnimation)>,
    mut imported: ImportedMarkers,
) -> Vec<ModelAnimation> {
    let retained: Vec<bool> = (0..imported.by_stack.len())
        .map(|index| baked_stacks.iter().any(|(stack, _)| *stack == index))
        .collect();

    for (index, has_clip) in retained.iter().copied().enumerate() {
        if has_clip {
            continue;
        }
        imported
            .file_warnings
            .append(&mut imported.warnings_by_stack[index]);
        if !imported.by_stack[index].is_empty() {
            imported.file_warnings.push(format!(
                "стек {:?} не содержит дорожек клипа; его маркеры отброшены",
                imported.stack_names[index]
            ));
        }
    }

    for (index, animation) in &mut baked_stacks {
        animation.markers = std::mem::take(&mut imported.by_stack[*index]);
        animation.marker_warnings = std::mem::take(&mut imported.warnings_by_stack[*index]);
        animation.file_marker_warnings = imported.file_warnings.clone();
    }
    baked_stacks
        .into_iter()
        .map(|(_, animation)| animation)
        .collect()
}

fn import_markers(scene: &ufbx::Scene) -> ImportedMarkers {
    let stack_names: Vec<String> = scene
        .anim_stacks
        .iter()
        .map(|stack| stack.element.name.to_string())
        .collect();
    let mut imported = ImportedMarkers {
        stack_names: stack_names.clone(),
        by_stack: vec![Vec::new(); stack_names.len()],
        warnings_by_stack: vec![Vec::new(); stack_names.len()],
        file_warnings: Vec::new(),
    };

    for (index, stack) in scene.anim_stacks.iter().enumerate() {
        if let Some(prop) = stack.element.props.find_prop("rve_markers") {
            import_marker_prop(prop, Some(index), &stack_names, &mut imported);
        }
    }
    for node in scene.nodes.iter() {
        if let Some(prop) = node.element.props.find_prop("rve_markers") {
            import_marker_prop(prop, None, &stack_names, &mut imported);
        }
    }
    imported
}

fn import_marker_prop(
    prop: &ufbx::Prop,
    source_stack: Option<usize>,
    stack_names: &[String],
    imported: &mut ImportedMarkers,
) {
    if prop.type_ != ufbx::PropType::String {
        if let Some(index) = source_stack.or_else(|| (stack_names.len() == 1).then_some(0)) {
            imported.warnings_by_stack[index]
                .push("rve_markers должно быть строковым свойством".to_owned());
        } else {
            imported
                .file_warnings
                .push("rve_markers должно быть строковым свойством".to_owned());
        }
        return;
    }
    import_marker_property(&prop.value_str, source_stack, stack_names, imported);
}

fn import_marker_property(
    value: &str,
    source_stack: Option<usize>,
    stack_names: &[String],
    imported: &mut ImportedMarkers,
) {
    for record in marker_records(value) {
        match parse_marker_record(record) {
            Ok(marker) => {
                if let Some(index) = marker_target(
                    marker.stack.as_deref(),
                    source_stack,
                    stack_names,
                    imported,
                    record,
                ) {
                    imported.by_stack[index].push(ModelMarker {
                        name: marker.name,
                        time: marker.time,
                    });
                }
            }
            Err(error) => {
                let warning = format!("rve_markers {:?}: {}", record, error.message);
                if let Some(index) = source_stack {
                    imported.warnings_by_stack[index].push(warning);
                } else {
                    let file_warning_count = imported.file_warnings.len();
                    if let Some(index) =
                        marker_target(error.stack.as_deref(), None, stack_names, imported, record)
                    {
                        imported.warnings_by_stack[index].push(warning);
                    } else if imported.file_warnings.len() > file_warning_count {
                        imported
                            .file_warnings
                            .last_mut()
                            .expect("marker_target added a file warning")
                            .push_str(&format!("; {}", error.message));
                    } else {
                        imported.file_warnings.push(warning);
                    }
                }
            }
        }
    }
}

fn marker_records(value: &str) -> impl Iterator<Item = &str> + '_ {
    value
        .split(';')
        .map(str::trim)
        .filter(|record| !record.is_empty())
}

fn marker_target(
    prefix: Option<&str>,
    source_stack: Option<usize>,
    stack_names: &[String],
    imported: &mut ImportedMarkers,
    record: &str,
) -> Option<usize> {
    if let Some(source) = source_stack {
        let Some(prefix) = prefix else {
            return Some(source);
        };
        if prefix == stack_names[source] {
            return Some(source);
        }
        if stack_names.iter().all(|name| name != prefix) {
            imported.file_warnings.push(format!(
                "rve_markers {:?}: неизвестный стек {:?}",
                record, prefix
            ));
        } else {
            imported.warnings_by_stack[source].push(format!(
                "rve_markers {:?}: префикс стека не совпадает с AnimStack {:?}",
                record, stack_names[source]
            ));
        }
        return None;
    }

    match prefix {
        Some(prefix) => match stack_names.iter().position(|name| name == prefix) {
            Some(index) => Some(index),
            None => {
                imported.file_warnings.push(format!(
                    "rve_markers {:?}: неизвестный стек {:?}",
                    record, prefix
                ));
                None
            }
        },
        None if stack_names.len() == 1 => Some(0),
        None => {
            imported.file_warnings.push(format!(
                "rve_markers {:?}: укажите имя стека, так как в файле несколько стеков",
                record
            ));
            None
        }
    }
}

fn parse_marker_record(record: &str) -> Result<ParsedMarker, MarkerParseError> {
    let Some((left, time_text)) = record.rsplit_once('@') else {
        return Err(MarkerParseError {
            stack: None,
            message: "ожидается формат [<stack>:]<marker>@<seconds>".to_owned(),
        });
    };
    let (stack, name) = match left.rsplit_once(':') {
        Some((stack, name)) => (Some(stack.to_owned()), name),
        None => (None, left),
    };
    if !is_marker_identifier(name) {
        return Err(MarkerParseError {
            stack,
            message: "имя маркера должно быть идентификатором ASCII".to_owned(),
        });
    }
    let time = time_text
        .parse::<f32>()
        .ok()
        .filter(|time| time.is_finite() && *time >= 0.0);
    let Some(time) = time else {
        return Err(MarkerParseError {
            stack,
            message: "время должно быть конечным числом не меньше нуля".to_owned(),
        });
    };
    Ok(ParsedMarker {
        stack,
        name: name.to_owned(),
        time,
    })
}

fn is_marker_identifier(name: &str) -> bool {
    let mut bytes = name.bytes();
    matches!(bytes.next(), Some(b'A'..=b'Z' | b'a'..=b'z' | b'_'))
        && bytes.all(|byte| matches!(byte, b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'_'))
}

#[cfg(feature = "fbx")]
fn bake_stack(
    scene: &ufbx::Scene,
    stack: &ufbx::AnimStack,
    name: String,
    include: &dyn Fn(u32) -> bool,
    keys: &mut usize,
) -> Result<ModelAnimation, String> {
    let baked = ufbx::bake_anim(scene, &stack.anim, ufbx::BakeOpts::default())
        .map_err(|error| format!("ошибка запекания анимации: {error:?}"))?;
    let begin = baked.playback_time_begin;
    let mut tracks = Vec::new();
    for node in baked.nodes.iter() {
        if !include(node.typed_id) {
            continue;
        }
        let count = node.translation_keys.len() + node.rotation_keys.len() + node.scale_keys.len();
        if count == 0 {
            continue;
        }
        *keys = keys
            .checked_sub(count)
            .ok_or_else(|| format!("анимации превышают предел памяти {KEY_LIMIT} ключей"))?;
        tracks.push(ModelTrack {
            node: scene.nodes[node.typed_id as usize].element.name.to_string(),
            translation: node
                .translation_keys
                .iter()
                .map(|key| ModelKey {
                    time: key_time(key.time, begin),
                    value: vec3(key.value),
                })
                .collect(),
            rotation: node
                .rotation_keys
                .iter()
                .map(|key| ModelKey {
                    time: key_time(key.time, begin),
                    value: [
                        key.value.x as f32,
                        key.value.y as f32,
                        key.value.z as f32,
                        key.value.w as f32,
                    ],
                })
                .collect(),
            scale: node
                .scale_keys
                .iter()
                .map(|key| ModelKey {
                    time: key_time(key.time, begin),
                    value: vec3(key.value),
                })
                .collect(),
        });
    }
    Ok(ModelAnimation {
        name,
        duration: baked.playback_duration as f32,
        tracks,
        markers: Vec::new(),
        file_marker_warnings: Vec::new(),
        marker_warnings: Vec::new(),
    })
}

#[cfg(feature = "fbx")]
fn key_time(time: f64, begin: f64) -> f32 {
    (time - begin).max(0.0) as f32
}

#[cfg(feature = "fbx")]
fn vec3(value: ufbx::Vec3) -> [f32; 3] {
    [value.x as f32, value.y as f32, value.z as f32]
}

#[cfg(test)]
mod tests {
    use super::{marker_records, parse_marker_record};

    #[test]
    fn parses_empty_and_whitespace_delimited_marker_properties() {
        assert_eq!(
            marker_records("  Start@0 ;  Hit@1.25  ").collect::<Vec<_>>(),
            ["Start@0", "Hit@1.25"]
        );
        assert_eq!(marker_records("").count(), 0);
        assert_eq!(marker_records("  ").count(), 0);
    }

    #[test]
    fn parses_multiple_records_and_optional_stack_prefix() {
        let first = parse_marker_record("Start@0").unwrap();
        assert_eq!(first.stack, None);
        assert_eq!(first.name, "Start");
        assert_eq!(first.time, 0.0);

        let second = parse_marker_record("Walk:FootDown@0.25").unwrap();
        assert_eq!(second.stack.as_deref(), Some("Walk"));
        assert_eq!(second.name, "FootDown");
        assert_eq!(second.time, 0.25);

        let records = marker_records("Start@0;Walk:FootDown@0.25")
            .map(parse_marker_record)
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        assert_eq!(records.len(), 2);
    }

    #[test]
    fn marker_stack_prefix_uses_last_colon() {
        let marker = parse_marker_record("Rig:Walk:FootDown@0.25").unwrap();
        assert_eq!(marker.stack.as_deref(), Some("Rig:Walk"));
        assert_eq!(marker.name, "FootDown");
    }

    #[test]
    fn rejects_invalid_marker_records() {
        for record in [
            "Hit@NaN",
            "Hit@inf",
            "Hit@abc",
            "Hit@-0.1",
            "Hit:0.5",
            "bad-name@1",
            "1Hit@1",
        ] {
            assert!(parse_marker_record(record).is_err(), "{record}");
        }
    }

    #[test]
    fn malformed_records_explain_non_numeric_time_and_missing_separator() {
        let non_numeric = parse_marker_record("Hit@abc").unwrap_err();
        assert!(
            non_numeric
                .message
                .contains("время должно быть конечным числом")
        );

        let missing_separator = parse_marker_record("Hit:0.5").unwrap_err();
        assert!(missing_separator.message.contains("ожидается формат"));
    }

    #[test]
    fn malformed_stack_record_keeps_parse_reason_on_its_stack() {
        use super::{ImportedMarkers, import_marker_property};

        let names = vec!["Bend".to_owned()];
        let mut imported = ImportedMarkers {
            stack_names: names.clone(),
            by_stack: vec![Vec::new()],
            warnings_by_stack: vec![Vec::new()],
            file_warnings: Vec::new(),
        };
        import_marker_property("Hit:0.5", Some(0), &names, &mut imported);

        assert!(imported.file_warnings.is_empty());
        assert_eq!(imported.warnings_by_stack[0].len(), 1);
        assert!(imported.warnings_by_stack[0][0].contains("ожидается формат"));
        assert!(!imported.warnings_by_stack[0][0].contains("неизвестный стек"));
    }

    #[test]
    fn malformed_node_record_keeps_parse_reason_without_a_false_unknown_stack() {
        use super::{ImportedMarkers, import_marker_property};

        let names = vec!["Bend".to_owned(), "Walk".to_owned()];
        let mut imported = ImportedMarkers {
            stack_names: names.clone(),
            by_stack: vec![Vec::new(); names.len()],
            warnings_by_stack: vec![Vec::new(); names.len()],
            file_warnings: Vec::new(),
        };
        import_marker_property("Hit@abc", None, &names, &mut imported);

        assert_eq!(imported.file_warnings.len(), 1);
        assert!(imported.file_warnings[0].contains("время должно быть конечным числом"));
        assert!(!imported.file_warnings[0].contains("неизвестный стек"));
    }

    #[test]
    fn unknown_stack_prefix_is_a_file_warning() {
        use super::{ImportedMarkers, marker_target};

        let names = vec!["Walk".to_owned()];
        let mut imported = ImportedMarkers {
            stack_names: names.clone(),
            by_stack: vec![Vec::new()],
            warnings_by_stack: vec![Vec::new()],
            file_warnings: Vec::new(),
        };
        assert_eq!(
            marker_target(
                Some("Missing"),
                None,
                &names,
                &mut imported,
                "Missing:Hit@0"
            ),
            None
        );
        assert_eq!(imported.file_warnings.len(), 1);
    }
}
