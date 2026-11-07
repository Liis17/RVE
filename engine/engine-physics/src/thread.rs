use std::collections::HashSet;
use std::fmt;
use std::sync::Arc;

use flume::{Receiver, RecvError, Selector, Sender};

use crate::Pose;
use crate::query::{QueryError, QuerySnapshot};
use crate::scene::{PhysicsSceneBinding, PhysicsSceneDesc};
use crate::world::{
    BodyChange, BodyHandle, PhysicsCommand, PhysicsEvent, PhysicsWorld, PhysicsWorldError,
    StepDriver, StepResult,
};

#[derive(Clone, Debug)]
pub enum PhysicsRequest {
    Init(PhysicsSceneDesc),
    Step {
        sequence: u64,
        dt: f32,
        changes: Vec<BodyChange>,
        commands: Vec<PhysicsCommand>,
    },
    Shutdown,
}

#[derive(Clone)]
pub struct PhysicsStepResult {
    pub sequence: u64,
    pub substeps: usize,
    pub poses: Vec<(BodyHandle, Pose)>,
    pub events: Vec<PhysicsEvent>,
    pub snapshot: Arc<QuerySnapshot>,
}

#[derive(Clone)]
pub struct PhysicsInitialization {
    pub result: PhysicsStepResult,
    pub bindings: Vec<PhysicsSceneBinding>,
}

impl PhysicsInitialization {
    /// Builds sequence zero from an already prepared driver, without starting a worker.
    pub fn from_prebuilt(
        driver: &StepDriver,
        bindings: Vec<PhysicsSceneBinding>,
    ) -> Result<Self, PhysicsServerError> {
        let result = build_initial_result(driver)
            .map_err(|error| PhysicsServerError::Initialization(error.to_string()))?;
        Ok(Self { result, bindings })
    }
}

pub enum PhysicsReply {
    Initialized(PhysicsInitialization),
    Step(PhysicsStepResult),
    Failed { sequence: u64, message: String },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PhysicsServerError {
    RequestChannelDisconnected,
    ReplyChannelDisconnected,
    ExpectedInit,
    RepeatedInit,
    InvalidSequence { expected: u64, actual: u64 },
    SequenceOverflow,
    Initialization(String),
    Step(String),
}

impl fmt::Display for PhysicsServerError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::RequestChannelDisconnected => {
                formatter.write_str("physics request channel disconnected")
            }
            Self::ReplyChannelDisconnected => {
                formatter.write_str("physics reply channel disconnected")
            }
            Self::ExpectedInit => {
                formatter.write_str("physics server expected Init as its first request")
            }
            Self::RepeatedInit => {
                formatter.write_str("physics server received Init after initialization")
            }
            Self::InvalidSequence { expected, actual } => {
                write!(
                    formatter,
                    "physics request sequence {actual} received; expected {expected}"
                )
            }
            Self::SequenceOverflow => formatter.write_str("physics request sequence overflow"),
            Self::Initialization(message) => {
                write!(formatter, "physics initialization failed: {message}")
            }
            Self::Step(message) => write!(formatter, "physics step failed: {message}"),
        }
    }
}

impl std::error::Error for PhysicsServerError {}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PhysicsClientError {
    AlreadyInitialized,
    NotInitialized,
    StepOutstanding,
    InvalidBody { body: BodyHandle, message: String },
    RequestChannelDisconnected,
    ReplyChannelDisconnected,
    ControlChannelDisconnected,
    UnexpectedReply,
    SequenceMismatch { expected: u64, actual: u64 },
    SequenceOverflow,
    RemoteFailure(String),
}

impl fmt::Display for PhysicsClientError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::AlreadyInitialized => {
                formatter.write_str("physics client is already initialized")
            }
            Self::NotInitialized => formatter.write_str("physics client is not initialized"),
            Self::StepOutstanding => {
                formatter.write_str("physics client already has an outstanding Step")
            }
            Self::InvalidBody { body, message } => {
                write!(formatter, "physics body {} is invalid: {message}", body.0)
            }
            Self::RequestChannelDisconnected => {
                formatter.write_str("physics request channel disconnected")
            }
            Self::ReplyChannelDisconnected => {
                formatter.write_str("physics reply channel disconnected")
            }
            Self::ControlChannelDisconnected => {
                formatter.write_str("physics control channel disconnected")
            }
            Self::UnexpectedReply => {
                formatter.write_str("physics client received an unexpected reply")
            }
            Self::SequenceMismatch { expected, actual } => {
                write!(
                    formatter,
                    "physics reply sequence {actual} received; expected {expected}"
                )
            }
            Self::SequenceOverflow => formatter.write_str("physics client sequence overflow"),
            Self::RemoteFailure(message) => write!(formatter, "physics server failed: {message}"),
        }
    }
}

impl std::error::Error for PhysicsClientError {}

pub enum PhysicsReceive<T> {
    Step(PhysicsStepResult),
    Interrupted(T),
}

pub struct PhysicsServer;

impl PhysicsServer {
    pub fn run(
        requests: Receiver<PhysicsRequest>,
        replies: Sender<PhysicsReply>,
    ) -> Result<(), PhysicsServerError> {
        let initial = requests
            .recv()
            .map_err(|_| PhysicsServerError::RequestChannelDisconnected)?;
        let PhysicsRequest::Init(desc) = initial else {
            return Err(PhysicsServerError::ExpectedInit);
        };
        let (driver, bindings) = match desc.build() {
            Ok(scene) => scene,
            Err(error) => {
                let message = error.to_string();
                send_failure(&replies, 0, message.clone())?;
                return Err(PhysicsServerError::Initialization(message));
            }
        };
        Self::run_initialized(requests, replies, driver, bindings)
    }

    /// Runs steps after the caller has already created sequence zero.
    pub fn run_preinitialized(
        requests: Receiver<PhysicsRequest>,
        replies: Sender<PhysicsReply>,
        driver: StepDriver,
    ) -> Result<(), PhysicsServerError> {
        Self::run_steps(requests, replies, driver)
    }

    fn run_initialized(
        requests: Receiver<PhysicsRequest>,
        replies: Sender<PhysicsReply>,
        driver: StepDriver,
        bindings: Vec<PhysicsSceneBinding>,
    ) -> Result<(), PhysicsServerError> {
        let initial_result = match build_initial_result(&driver) {
            Ok(result) => result,
            Err(error) => {
                let message = error.to_string();
                send_failure(&replies, 0, message.clone())?;
                return Err(PhysicsServerError::Initialization(message));
            }
        };
        replies
            .send(PhysicsReply::Initialized(PhysicsInitialization {
                result: initial_result,
                bindings,
            }))
            .map_err(|_| PhysicsServerError::ReplyChannelDisconnected)?;

        Self::run_steps(requests, replies, driver)
    }

    fn run_steps(
        requests: Receiver<PhysicsRequest>,
        replies: Sender<PhysicsReply>,
        mut driver: StepDriver,
    ) -> Result<(), PhysicsServerError> {
        let mut expected_sequence = 1_u64;
        loop {
            let request = requests
                .recv()
                .map_err(|_| PhysicsServerError::RequestChannelDisconnected)?;
            match request {
                PhysicsRequest::Shutdown => return Ok(()),
                PhysicsRequest::Init(_) => {
                    send_failure(
                        &replies,
                        expected_sequence,
                        PhysicsServerError::RepeatedInit.to_string(),
                    )?;
                    return Err(PhysicsServerError::RepeatedInit);
                }
                PhysicsRequest::Step {
                    sequence,
                    dt,
                    changes,
                    commands,
                } => {
                    if sequence != expected_sequence {
                        let error = PhysicsServerError::InvalidSequence {
                            expected: expected_sequence,
                            actual: sequence,
                        };
                        send_failure(&replies, sequence, error.to_string())?;
                        return Err(error);
                    }
                    let result = match build_result(sequence, &mut driver, dt, changes, commands) {
                        Ok(result) => result,
                        Err(error) => {
                            let message = error.to_string();
                            send_failure(&replies, sequence, message.clone())?;
                            return Err(PhysicsServerError::Step(message));
                        }
                    };
                    if replies.send(PhysicsReply::Step(result)).is_err() {
                        return match requests.recv() {
                            Ok(PhysicsRequest::Shutdown) => Ok(()),
                            _ => Err(PhysicsServerError::ReplyChannelDisconnected),
                        };
                    }
                    expected_sequence = expected_sequence
                        .checked_add(1)
                        .ok_or(PhysicsServerError::SequenceOverflow)?;
                }
            }
        }
    }
}

pub struct PhysicsClient {
    requests: Sender<PhysicsRequest>,
    replies: Receiver<PhysicsReply>,
    initialized: bool,
    next_sequence: u64,
    outstanding: Option<u64>,
}

impl PhysicsClient {
    pub fn new(requests: Sender<PhysicsRequest>, replies: Receiver<PhysicsReply>) -> Self {
        Self {
            requests,
            replies,
            initialized: false,
            next_sequence: 1,
            outstanding: None,
        }
    }

    /// Creates a client after sequence zero was built by `PhysicsInitialization::from_prebuilt`.
    pub fn new_preinitialized(
        requests: Sender<PhysicsRequest>,
        replies: Receiver<PhysicsReply>,
    ) -> Self {
        Self {
            requests,
            replies,
            initialized: true,
            next_sequence: 1,
            outstanding: None,
        }
    }

    /// Sends Init and waits for sequence zero and the initial query snapshot.
    pub fn initialize(
        &mut self,
        desc: PhysicsSceneDesc,
    ) -> Result<PhysicsInitialization, PhysicsClientError> {
        if self.initialized {
            return Err(PhysicsClientError::AlreadyInitialized);
        }
        self.requests
            .send(PhysicsRequest::Init(desc))
            .map_err(|_| PhysicsClientError::RequestChannelDisconnected)?;
        self.receive_initialization()
    }

    fn receive_initialization(&mut self) -> Result<PhysicsInitialization, PhysicsClientError> {
        let reply = self
            .replies
            .recv()
            .map_err(|_| PhysicsClientError::ReplyChannelDisconnected)?;
        match reply {
            PhysicsReply::Initialized(initialization) => {
                if initialization.result.sequence != 0 {
                    return Err(PhysicsClientError::SequenceMismatch {
                        expected: 0,
                        actual: initialization.result.sequence,
                    });
                }
                self.initialized = true;
                Ok(initialization)
            }
            PhysicsReply::Failed { sequence, message } => {
                if sequence != 0 {
                    return Err(PhysicsClientError::SequenceMismatch {
                        expected: 0,
                        actual: sequence,
                    });
                }
                Err(PhysicsClientError::RemoteFailure(message))
            }
            PhysicsReply::Step(_) => Err(PhysicsClientError::UnexpectedReply),
        }
    }

    /// Queues one frame Step. A second Step is rejected until its result is received.
    pub fn step(
        &mut self,
        dt: f32,
        commands: Vec<PhysicsCommand>,
    ) -> Result<u64, PhysicsClientError> {
        self.step_with_changes(dt, Vec::new(), commands)
    }

    /// Queues one frame Step that applies `changes` before `commands`. Inserts are validated
    /// before sending, so an invalid body neither reaches the server nor consumes a sequence.
    /// Commands for bodies removed by this Step are dropped, and repeated removals are sent once.
    pub fn step_with_changes(
        &mut self,
        dt: f32,
        changes: Vec<BodyChange>,
        mut commands: Vec<PhysicsCommand>,
    ) -> Result<u64, PhysicsClientError> {
        if !self.initialized {
            return Err(PhysicsClientError::NotInitialized);
        }
        if self.outstanding.is_some() {
            return Err(PhysicsClientError::StepOutstanding);
        }
        let mut removed = HashSet::new();
        let mut sent_changes = Vec::with_capacity(changes.len());
        for change in changes {
            match &change {
                BodyChange::Insert { handle, desc } => {
                    desc.validate()
                        .map_err(|error| PhysicsClientError::InvalidBody {
                            body: *handle,
                            message: error.to_string(),
                        })?;
                }
                BodyChange::Remove { handle } => {
                    if !removed.insert(*handle) {
                        continue;
                    }
                }
            }
            sent_changes.push(change);
        }
        commands.retain(|command| !removed.contains(&command.body()));
        let next = self
            .next_sequence
            .checked_add(1)
            .ok_or(PhysicsClientError::SequenceOverflow)?;
        let sequence = self.next_sequence;
        self.requests
            .send(PhysicsRequest::Step {
                sequence,
                dt,
                changes: sent_changes,
                commands,
            })
            .map_err(|_| PhysicsClientError::RequestChannelDisconnected)?;
        self.outstanding = Some(sequence);
        self.next_sequence = next;
        Ok(sequence)
    }

    /// Waits for the outstanding result or returns the caller's control message.
    /// With no outstanding Step it returns immediately and leaves the control channel untouched.
    pub fn receive<T: Send + 'static>(
        &mut self,
        control: &Receiver<T>,
    ) -> Result<Option<PhysicsReceive<T>>, PhysicsClientError> {
        let Some(expected_sequence) = self.outstanding else {
            return Ok(None);
        };
        enum Selected<T> {
            Reply(Result<PhysicsReply, RecvError>),
            Control(Result<T, RecvError>),
        }
        match Selector::new()
            .recv(&self.replies, Selected::Reply)
            .recv(control, Selected::Control)
            .wait()
        {
            Selected::Control(Ok(message)) => Ok(Some(PhysicsReceive::Interrupted(message))),
            Selected::Control(Err(_)) => Err(PhysicsClientError::ControlChannelDisconnected),
            Selected::Reply(Err(_)) => Err(PhysicsClientError::ReplyChannelDisconnected),
            Selected::Reply(Ok(PhysicsReply::Step(result))) => {
                if result.sequence != expected_sequence {
                    self.outstanding = None;
                    return Err(PhysicsClientError::SequenceMismatch {
                        expected: expected_sequence,
                        actual: result.sequence,
                    });
                }
                self.outstanding = None;
                Ok(Some(PhysicsReceive::Step(result)))
            }
            Selected::Reply(Ok(PhysicsReply::Failed { sequence, message })) => {
                if sequence != expected_sequence {
                    self.outstanding = None;
                    return Err(PhysicsClientError::SequenceMismatch {
                        expected: expected_sequence,
                        actual: sequence,
                    });
                }
                self.outstanding = None;
                Err(PhysicsClientError::RemoteFailure(message))
            }
            Selected::Reply(Ok(PhysicsReply::Initialized(_))) => {
                self.outstanding = None;
                Err(PhysicsClientError::UnexpectedReply)
            }
        }
    }

    pub fn shutdown(&self) -> Result<(), PhysicsClientError> {
        self.requests
            .send(PhysicsRequest::Shutdown)
            .map_err(|_| PhysicsClientError::RequestChannelDisconnected)
    }
}

fn build_result(
    sequence: u64,
    driver: &mut StepDriver,
    dt: f32,
    changes: Vec<BodyChange>,
    commands: Vec<PhysicsCommand>,
) -> Result<PhysicsStepResult, PhysicsServerStepError> {
    let step = driver.advance_step_with_changes(dt, changes, commands)?;
    snapshot_result(sequence, step, driver.world())
}

fn build_initial_result(driver: &StepDriver) -> Result<PhysicsStepResult, PhysicsServerStepError> {
    snapshot_result(0, driver.initial_result(), driver.world())
}

fn snapshot_result(
    sequence: u64,
    step: StepResult,
    world: &PhysicsWorld,
) -> Result<PhysicsStepResult, PhysicsServerStepError> {
    let snapshot = world.query_snapshot()?;
    Ok(PhysicsStepResult {
        sequence,
        substeps: step.substeps,
        poses: step.poses,
        events: step.events,
        snapshot,
    })
}

fn send_failure(
    replies: &Sender<PhysicsReply>,
    sequence: u64,
    message: String,
) -> Result<(), PhysicsServerError> {
    replies
        .send(PhysicsReply::Failed { sequence, message })
        .map_err(|_| PhysicsServerError::ReplyChannelDisconnected)
}

#[derive(Debug)]
enum PhysicsServerStepError {
    World(PhysicsWorldError),
    Query(QueryError),
}

impl fmt::Display for PhysicsServerStepError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::World(error) => error.fmt(formatter),
            Self::Query(error) => error.fmt(formatter),
        }
    }
}

impl From<PhysicsWorldError> for PhysicsServerStepError {
    fn from(value: PhysicsWorldError) -> Self {
        Self::World(value)
    }
}

impl From<QueryError> for PhysicsServerStepError {
    fn from(value: QueryError) -> Self {
        Self::Query(value)
    }
}

#[cfg(test)]
mod tests {
    use super::{
        PhysicsClient, PhysicsClientError, PhysicsInitialization, PhysicsReceive, PhysicsReply,
        PhysicsRequest, PhysicsServer, PhysicsServerError, PhysicsStepResult,
    };
    use crate::scene::PhysicsSceneDesc;
    use crate::{
        BodyChange, BodyDesc, BodyHandle, PhysicsCommand, PhysicsEvent, PhysicsWorld, Pose, Ray,
        Shape,
    };
    use engine_scene::{SceneDocument, World};
    use flume::Receiver;
    use glam::Vec3;
    use std::sync::{Arc, Barrier};

    const CAMERA: &str = "[[objects]]\nid='camera'\n[objects.camera]\n";

    fn scene_with_dynamic_sphere() -> (World, PhysicsSceneDesc) {
        let world = World::from_document(
            &SceneDocument::parse(
                "thread-test.rvescene",
                &format!(
                    "{CAMERA}\n[[objects]]\nid='ball'\n\
                     [objects.collider]\nshape='sphere'\nradius=0.5\n\
                     [objects.rigid_body]\nkind='dynamic'\nmass=1.0\n"
                ),
            )
            .unwrap(),
        )
        .unwrap();
        let desc = PhysicsSceneDesc::from_world(&world, &[], Vec3::ZERO, 60, 8).unwrap();
        (world, desc)
    }

    fn empty_scene() -> PhysicsSceneDesc {
        let world =
            World::from_document(&SceneDocument::parse("thread-test.rvescene", CAMERA).unwrap())
                .unwrap();
        PhysicsSceneDesc::from_world(&world, &[], Vec3::ZERO, 60, 8).unwrap()
    }

    fn fake_initialization() -> PhysicsInitialization {
        PhysicsInitialization {
            result: PhysicsStepResult {
                sequence: 0,
                substeps: 0,
                poses: Vec::new(),
                events: Vec::new(),
                snapshot: PhysicsWorld::new(Vec3::ZERO)
                    .unwrap()
                    .query_snapshot()
                    .unwrap(),
            },
            bindings: Vec::new(),
        }
    }

    fn select_result(receiver: &Receiver<PhysicsReply>) -> Result<PhysicsReply, flume::RecvError> {
        receiver.recv()
    }

    #[test]
    fn init_returns_sequence_zero_and_each_step_is_lockstep_including_zero_substeps() {
        let (request_tx, request_rx) = flume::unbounded();
        let (reply_tx, reply_rx) = flume::unbounded();
        let server = std::thread::spawn(move || PhysicsServer::run(request_rx, reply_tx));
        let mut client = PhysicsClient::new(request_tx, reply_rx);
        let (_, desc) = scene_with_dynamic_sphere();
        let initial = client.initialize(desc).unwrap();
        assert_eq!(initial.result.sequence, 0);
        assert!(initial.result.events.is_empty());
        assert_eq!(initial.result.substeps, 0);

        let (control_tx, control_rx) = flume::unbounded::<()>();
        assert!(client.receive(&control_rx).unwrap().is_none());
        let body = initial.bindings[0].body;
        assert_eq!(
            client
                .step(
                    0.0,
                    vec![PhysicsCommand::AddImpulse {
                        body,
                        impulse: Vec3::X,
                        point: None,
                    }]
                )
                .unwrap(),
            1
        );
        assert_eq!(
            client.step(1.0 / 60.0, Vec::new()),
            Err(PhysicsClientError::StepOutstanding)
        );
        let Some(PhysicsReceive::Step(result)) = client.receive(&control_rx).unwrap() else {
            panic!("expected step result")
        };
        assert_eq!(result.sequence, 1);
        assert_eq!(result.substeps, 0);
        let ray = Ray::new(Vec3::new(-2.0, 0.0, 0.0), Vec3::X, 4.0).unwrap();
        let hit = result
            .snapshot
            .raycast(&ray, Default::default())
            .unwrap()
            .unwrap();
        assert_eq!(hit.body, body);
        assert_eq!(hit.body_velocity, Vec3::X);
        assert!(client.receive(&control_rx).unwrap().is_none());
        assert_eq!(client.step(1.0 / 60.0, Vec::new()).unwrap(), 2);
        let Some(PhysicsReceive::Step(next_result)) = client.receive(&control_rx).unwrap() else {
            panic!("expected second step result")
        };
        assert_eq!(next_result.sequence, 2);
        assert_eq!(next_result.substeps, 1);
        let next_hit = next_result
            .snapshot
            .raycast(&ray, Default::default())
            .unwrap()
            .unwrap();
        assert_eq!(next_hit.body_velocity, Vec3::X);
        client.shutdown().unwrap();
        drop(control_tx);
        assert!(server.join().unwrap().is_ok());
    }

    #[test]
    fn preinitialized_server_steps_after_caller_builds_sequence_zero() {
        let (request_tx, request_rx) = flume::unbounded();
        let (reply_tx, reply_rx) = flume::unbounded();
        let (_, desc) = scene_with_dynamic_sphere();
        let (driver, bindings) = desc.build().unwrap();
        let body = bindings[0].body;
        let initial = PhysicsInitialization::from_prebuilt(&driver, bindings).unwrap();
        assert_eq!(initial.result.sequence, 0);
        assert!(initial.result.snapshot.body_state(body).is_some());
        let server = std::thread::spawn(move || {
            PhysicsServer::run_preinitialized(request_rx, reply_tx, driver)
        });
        let mut client = PhysicsClient::new_preinitialized(request_tx, reply_rx);

        let (control_tx, control_rx) = flume::unbounded::<()>();
        client.step(1.0 / 60.0, Vec::new()).unwrap();
        let Some(PhysicsReceive::Step(result)) = client.receive(&control_rx).unwrap() else {
            panic!("expected preinitialized server step result")
        };
        assert_eq!(result.sequence, 1);
        client.shutdown().unwrap();
        drop(control_tx);
        server.join().unwrap().unwrap();
    }

    #[test]
    fn abandoned_step_reply_is_graceful_when_host_shuts_server_down() {
        let (_, desc) = scene_with_dynamic_sphere();
        let (driver, _) = desc.build().unwrap();
        let (request_tx, request_rx) = flume::unbounded();
        let (reply_tx, reply_rx) = flume::unbounded();
        // Нет получателя ответа: отправка StepResult провалится до того, как сервер его отдаст.
        drop(reply_rx);
        request_tx
            .send(PhysicsRequest::Step {
                sequence: 1,
                dt: 0.0,
                changes: Vec::new(),
                commands: Vec::new(),
            })
            .unwrap();
        request_tx.send(PhysicsRequest::Shutdown).unwrap();
        let server = std::thread::spawn(move || {
            PhysicsServer::run_preinitialized(request_rx, reply_tx, driver)
        });

        server.join().unwrap().unwrap();
    }

    #[test]
    fn abandoned_step_reply_without_shutdown_reports_disconnected_host() {
        let (_, desc) = scene_with_dynamic_sphere();
        let (driver, _) = desc.build().unwrap();
        let (request_tx, request_rx) = flume::unbounded();
        let (reply_tx, reply_rx) = flume::unbounded();
        drop(reply_rx);
        request_tx
            .send(PhysicsRequest::Step {
                sequence: 1,
                dt: 0.0,
                changes: Vec::new(),
                commands: Vec::new(),
            })
            .unwrap();
        // Все отправители пропали без Shutdown: сервер не должен ждать вечно.
        drop(request_tx);
        let server = std::thread::spawn(move || {
            PhysicsServer::run_preinitialized(request_rx, reply_tx, driver)
        });

        assert!(matches!(
            server.join().unwrap(),
            Err(PhysicsServerError::ReplyChannelDisconnected)
        ));
    }

    #[test]
    fn init_has_no_events_and_first_zero_substep_step_reports_initial_overlap() {
        let scene = World::from_document(
            &SceneDocument::parse(
                "thread-overlap-test.rvescene",
                &format!(
                    "{CAMERA}\n[[objects]]\nid='trigger'\n\
                     [objects.collider]\nshape='sphere'\nradius=0.5\ntrigger=true\n\
                     [[objects]]\nid='ball'\n\
                     [objects.collider]\nshape='sphere'\nradius=0.5\n\
                     [objects.rigid_body]\nkind='dynamic'\n"
                ),
            )
            .unwrap(),
        )
        .unwrap();
        let desc = PhysicsSceneDesc::from_world(&scene, &[], Vec3::ZERO, 60, 8).unwrap();
        let (request_tx, request_rx) = flume::unbounded();
        let (reply_tx, reply_rx) = flume::unbounded();
        let server = std::thread::spawn(move || PhysicsServer::run(request_rx, reply_tx));
        let mut client = PhysicsClient::new(request_tx, reply_rx);
        let initial = client.initialize(desc).unwrap();
        assert!(initial.result.events.is_empty());
        assert_eq!(initial.result.poses.len(), 2);
        let (_control_tx, control_rx) = flume::unbounded::<()>();
        client.step(0.0, Vec::new()).unwrap();
        let Some(PhysicsReceive::Step(first_step)) = client.receive(&control_rx).unwrap() else {
            panic!("expected first step")
        };
        assert_eq!(first_step.substeps, 0);
        assert_eq!(first_step.events.len(), 1);
        assert!(matches!(
            first_step.events[0],
            PhysicsEvent::TriggerEnter { .. }
        ));
        client.shutdown().unwrap();
        assert!(server.join().unwrap().is_ok());
    }

    #[test]
    fn client_does_not_wait_or_consume_control_without_an_outstanding_step() {
        let (request_tx, _request_rx) = flume::unbounded();
        let (reply_tx, reply_rx) = flume::unbounded();
        reply_tx
            .send(PhysicsReply::Initialized(fake_initialization()))
            .unwrap();
        let mut client = PhysicsClient::new(request_tx, reply_rx);
        client.initialize(empty_scene()).unwrap();
        let (control_tx, control_rx) = flume::unbounded();
        control_tx.send(17_u8).unwrap();
        assert!(client.receive(&control_rx).unwrap().is_none());
        assert_eq!(control_rx.try_recv().unwrap(), 17);
    }

    #[test]
    fn client_interrupts_wait_for_step_result_with_caller_control_message() {
        let (request_tx, request_rx) = flume::unbounded();
        let (reply_tx, reply_rx) = flume::unbounded();
        reply_tx
            .send(PhysicsReply::Initialized(fake_initialization()))
            .unwrap();
        let mut client = PhysicsClient::new(request_tx, reply_rx);
        client.initialize(empty_scene()).unwrap();
        client.step(0.0, Vec::new()).unwrap();
        assert!(matches!(
            request_rx.recv().unwrap(),
            PhysicsRequest::Init(_)
        ));
        assert!(matches!(
            request_rx.recv().unwrap(),
            PhysicsRequest::Step { sequence: 1, .. }
        ));

        let (control_tx, control_rx) = flume::unbounded();
        let barrier = Arc::new(Barrier::new(2));
        let worker_barrier = barrier.clone();
        let worker = std::thread::spawn(move || {
            worker_barrier.wait();
            let received = client.receive(&control_rx).unwrap().unwrap();
            assert!(matches!(received, PhysicsReceive::Interrupted("stop")));
            client.shutdown().unwrap();
        });
        barrier.wait();
        control_tx.send("stop").unwrap();
        worker.join().unwrap();
        assert!(matches!(
            request_rx.recv().unwrap(),
            PhysicsRequest::Shutdown
        ));
    }

    #[test]
    fn disconnected_reply_and_mismatched_sequence_are_reported() {
        let (request_tx, request_rx) = flume::unbounded();
        let (reply_tx, reply_rx) = flume::unbounded();
        reply_tx
            .send(PhysicsReply::Initialized(fake_initialization()))
            .unwrap();
        let mut client = PhysicsClient::new(request_tx, reply_rx);
        client.initialize(empty_scene()).unwrap();
        client.step(0.0, Vec::new()).unwrap();
        let _ = request_rx.recv().unwrap();
        drop(reply_tx);
        let (_control_tx, control_rx) = flume::unbounded::<()>();
        assert_eq!(
            client.receive(&control_rx).err().unwrap(),
            PhysicsClientError::ReplyChannelDisconnected
        );

        let (request_tx, request_rx) = flume::unbounded();
        let (reply_tx, reply_rx) = flume::unbounded();
        reply_tx
            .send(PhysicsReply::Initialized(fake_initialization()))
            .unwrap();
        let mut client = PhysicsClient::new(request_tx, reply_rx);
        client.initialize(empty_scene()).unwrap();
        client.step(0.0, Vec::new()).unwrap();
        let _ = request_rx.recv().unwrap();
        let mut wrong = fake_initialization().result;
        wrong.sequence = 9;
        reply_tx.send(PhysicsReply::Step(wrong)).unwrap();
        assert_eq!(
            client.receive(&control_rx).err().unwrap(),
            PhysicsClientError::SequenceMismatch {
                expected: 1,
                actual: 9,
            }
        );
        assert!(client.receive(&control_rx).unwrap().is_none());
    }

    #[test]
    fn disconnected_request_channel_is_reported() {
        let (request_tx, request_rx) = flume::unbounded();
        let (reply_tx, reply_rx) = flume::unbounded();
        reply_tx
            .send(PhysicsReply::Initialized(fake_initialization()))
            .unwrap();
        let mut client = PhysicsClient::new(request_tx, reply_rx);
        client.initialize(empty_scene()).unwrap();
        assert!(matches!(
            request_rx.recv().unwrap(),
            PhysicsRequest::Init(_)
        ));
        drop(request_rx);
        assert_eq!(
            client.step(0.0, Vec::new()).err().unwrap(),
            PhysicsClientError::RequestChannelDisconnected
        );
    }

    #[test]
    fn server_rejects_out_of_order_requests() {
        let (request_tx, request_rx) = flume::unbounded();
        let (reply_tx, reply_rx) = flume::unbounded();
        let server = std::thread::spawn(move || PhysicsServer::run(request_rx, reply_tx));
        let (_, desc) = scene_with_dynamic_sphere();
        request_tx.send(PhysicsRequest::Init(desc)).unwrap();
        assert!(matches!(
            select_result(&reply_rx).unwrap(),
            PhysicsReply::Initialized(_)
        ));
        request_tx
            .send(PhysicsRequest::Step {
                sequence: 4,
                dt: 0.0,
                changes: Vec::new(),
                commands: Vec::new(),
            })
            .unwrap();
        assert!(matches!(
            select_result(&reply_rx).unwrap(),
            PhysicsReply::Failed { sequence: 4, .. }
        ));
        assert!(server.join().unwrap().is_err());
    }

    fn dynamic_ball(mass: f32) -> BodyDesc {
        let mut desc = BodyDesc::dynamic(Shape::sphere(0.5).unwrap(), Pose::IDENTITY, 1.0);
        desc.mass = mass;
        desc
    }

    #[test]
    fn invalid_insert_is_not_sent_and_does_not_consume_a_sequence() {
        let (request_tx, request_rx) = flume::unbounded();
        let (_reply_tx, reply_rx) = flume::unbounded();
        let mut client = PhysicsClient::new_preinitialized(request_tx, reply_rx);
        let error = client
            .step_with_changes(
                0.0,
                vec![BodyChange::Insert {
                    handle: BodyHandle(1),
                    desc: dynamic_ball(0.0),
                }],
                Vec::new(),
            )
            .unwrap_err();
        assert!(matches!(
            error,
            PhysicsClientError::InvalidBody {
                body: BodyHandle(1),
                ..
            }
        ));
        assert!(request_rx.try_recv().is_err());
        assert_eq!(client.step(0.0, Vec::new()).unwrap(), 1);
    }

    #[test]
    fn repeated_removals_are_sent_once_and_commands_for_removed_bodies_are_dropped() {
        let (request_tx, request_rx) = flume::unbounded();
        let (_reply_tx, reply_rx) = flume::unbounded();
        let mut client = PhysicsClient::new_preinitialized(request_tx, reply_rx);
        let removed = BodyHandle(3);
        let kept = BodyHandle(4);
        client
            .step_with_changes(
                0.1,
                vec![
                    BodyChange::Remove { handle: removed },
                    BodyChange::Remove { handle: removed },
                ],
                vec![
                    PhysicsCommand::SetVelocity {
                        body: removed,
                        velocity: Vec3::X,
                    },
                    PhysicsCommand::Teleport {
                        body: removed,
                        pose: Pose::IDENTITY,
                    },
                    PhysicsCommand::SetVelocity {
                        body: kept,
                        velocity: Vec3::Y,
                    },
                ],
            )
            .unwrap();
        match request_rx.try_recv().unwrap() {
            PhysicsRequest::Step {
                sequence,
                changes,
                commands,
                ..
            } => {
                assert_eq!(sequence, 1);
                assert_eq!(changes, vec![BodyChange::Remove { handle: removed }]);
                assert_eq!(
                    commands,
                    vec![PhysicsCommand::SetVelocity {
                        body: kept,
                        velocity: Vec3::Y,
                    }]
                );
            }
            other => panic!("unexpected request {other:?}"),
        }
    }

    #[test]
    fn server_applies_removal_with_same_step_commands_and_keeps_running_after_rejected_insert() {
        let (_, desc) = scene_with_dynamic_sphere();
        let (request_tx, request_rx) = flume::unbounded();
        let (reply_tx, reply_rx) = flume::unbounded();
        let (_control_tx, control_rx) = flume::unbounded::<()>();
        let server = std::thread::spawn(move || PhysicsServer::run(request_rx, reply_tx));
        let mut client = PhysicsClient::new(request_tx, reply_rx);
        let initialization = client.initialize(desc).unwrap();
        let body = initialization.bindings[0].body;

        assert!(
            client
                .step_with_changes(
                    1.0 / 60.0,
                    vec![BodyChange::Insert {
                        handle: body,
                        desc: dynamic_ball(0.0),
                    }],
                    Vec::new(),
                )
                .is_err()
        );

        client
            .step_with_changes(
                1.0 / 60.0,
                vec![BodyChange::Remove { handle: body }],
                vec![
                    PhysicsCommand::SetVelocity {
                        body,
                        velocity: Vec3::X,
                    },
                    PhysicsCommand::Teleport {
                        body,
                        pose: Pose::IDENTITY,
                    },
                ],
            )
            .unwrap();
        let PhysicsReceive::Step(removed) = client.receive(&control_rx).unwrap().unwrap() else {
            panic!("expected the removal Step result")
        };
        assert_eq!(removed.sequence, 1);
        assert!(removed.poses.iter().all(|(handle, _)| *handle != body));

        client
            .step_with_changes(
                1.0 / 60.0,
                vec![BodyChange::Insert {
                    handle: BodyHandle(2),
                    desc: dynamic_ball(1.0),
                }],
                Vec::new(),
            )
            .unwrap();
        let PhysicsReceive::Step(inserted) = client.receive(&control_rx).unwrap().unwrap() else {
            panic!("expected the insertion Step result")
        };
        assert_eq!(inserted.sequence, 2);
        assert!(
            inserted
                .poses
                .iter()
                .any(|(handle, _)| *handle == BodyHandle(2))
        );

        client.shutdown().unwrap();
        assert!(server.join().unwrap().is_ok());
    }
}
