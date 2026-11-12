use super::{ParticleRequest, apply_emitter_request, spawn_particle_requests};
use crate::scene::particle_emitter_desc;
use engine_render::ParticleEmitterDesc;
use engine_scene::{ObjectHandle, ParticleEmitterComponent, SceneDocument, World};
use std::collections::HashMap;

fn handles() -> [ObjectHandle; 3] {
    let document = SceneDocument::parse(
            "particles.rvescene",
            "[[objects]]\nid=\"camera\"\n[objects.camera]\n[[objects]]\nid=\"a\"\n[[objects]]\nid=\"b\"\n[[objects]]\nid=\"c\"\n",
        )
        .unwrap();
    let world = World::from_document(&document).unwrap();
    ["a", "b", "c"].map(|id| world.handle(id).unwrap())
}

fn desc(capacity: u32) -> ParticleEmitterDesc {
    particle_emitter_desc(
        &ParticleEmitterComponent {
            capacity,
            ..ParticleEmitterComponent::default()
        },
        None,
    )
}

/// Эмиттер в тесте — его ёмкость; уничтожение пишет id в журнал.
fn create(_: &mut Vec<u32>, desc: ParticleEmitterDesc) -> Result<u32, String> {
    if desc.capacity == 0 {
        return Err("ёмкость 0".to_owned());
    }
    Ok(desc.capacity)
}

fn destroy(log: &mut Vec<u32>, id: u32) {
    log.push(id);
}

fn apply(table: &mut HashMap<ObjectHandle, u32>, log: &mut Vec<u32>, request: ParticleRequest) {
    apply_emitter_request(table, log, request, create, destroy);
}

fn create_request(object: ObjectHandle, capacity: u32) -> ParticleRequest {
    ParticleRequest::Create {
        object,
        desc: Box::new(desc(capacity)),
    }
}

#[test]
fn spawn_requests_follow_template_order_and_skip_objects_without_emitter() {
    let [a, b, c] = handles();
    let requests = spawn_particle_requests(&[a, b, c], &[Some(desc(8)), None, Some(desc(16))]);
    let created: Vec<(ObjectHandle, u32)> = requests
        .into_iter()
        .map(|request| match request {
            ParticleRequest::Create { object, desc } => (object, desc.capacity),
            ParticleRequest::Destroy(_) => panic!("spawn creates only"),
        })
        .collect();
    assert_eq!(created, vec![(a, 8), (c, 16)]);
}

#[test]
fn create_inserts_the_emitter_of_the_object() {
    let [a, ..] = handles();
    let mut table = HashMap::new();
    let mut log = Vec::new();
    apply(&mut table, &mut log, create_request(a, 8));
    assert_eq!(table.get(&a), Some(&8));
    assert!(log.is_empty());
}

#[test]
fn create_for_an_object_with_an_emitter_destroys_the_previous_one() {
    let [a, ..] = handles();
    let mut table = HashMap::new();
    let mut log = Vec::new();
    apply(&mut table, &mut log, create_request(a, 8));
    apply(&mut table, &mut log, create_request(a, 16));
    assert_eq!(table.get(&a), Some(&16));
    assert_eq!(log, vec![8]);
}

#[test]
fn failed_create_leaves_the_object_without_an_emitter() {
    let [a, ..] = handles();
    let mut table = HashMap::new();
    let mut log = Vec::new();
    apply(&mut table, &mut log, create_request(a, 0));
    assert!(!table.contains_key(&a));
    assert!(log.is_empty());
}

#[test]
fn destroy_removes_known_objects_and_ignores_unknown_ones() {
    let [a, b, c] = handles();
    let mut table = HashMap::new();
    let mut log = Vec::new();
    apply(&mut table, &mut log, create_request(a, 8));
    apply(&mut table, &mut log, create_request(b, 16));
    apply(&mut table, &mut log, ParticleRequest::Destroy(vec![a, c]));
    assert!(!table.contains_key(&a));
    assert_eq!(table.get(&b), Some(&16));
    assert_eq!(log, vec![8]);
    apply(&mut table, &mut log, ParticleRequest::Destroy(vec![a]));
    assert_eq!(log, vec![8], "повторное уничтожение не трогает эмиттер");
}
