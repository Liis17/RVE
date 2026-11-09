use mlua::{Function, Thread};

pub(crate) const MAX_SCRIPT_TIMERS: usize = 256;
pub(crate) const MAX_SCRIPT_COROUTINES: usize = 256;

#[derive(Clone)]
pub(crate) enum TimerCallback {
    Lua(Function),
    Abi(String),
}

pub(crate) struct DueTimer {
    pub(crate) id: u64,
    pub(crate) owner: u64,
    pub(crate) repeating: bool,
    pub(crate) callback: TimerCallback,
}

struct ScriptTimer {
    id: u64,
    owner: u64,
    due: f64,
    repeat: Option<f64>,
    callback: TimerCallback,
    eligible: bool,
}

#[derive(Clone, Copy)]
pub(crate) enum CoroutineWait {
    Frame(u64),
    Time(f64),
}

pub(crate) struct ScriptCoroutine {
    pub(crate) id: u64,
    pub(crate) owner: u64,
    pub(crate) thread: Thread,
    wait: CoroutineWait,
    eligible: bool,
}

impl ScriptCoroutine {
    pub(crate) fn new(id: u64, owner: u64, thread: Thread, resume_frame: u64) -> Self {
        Self {
            id,
            owner,
            thread,
            wait: CoroutineWait::Frame(resume_frame),
            eligible: false,
        }
    }

    pub(crate) fn waiting(id: u64, owner: u64, thread: Thread, wait: CoroutineWait) -> Self {
        Self {
            id,
            owner,
            thread,
            wait,
            eligible: false,
        }
    }
}

pub(crate) struct TimelineOp {
    kind: TimelineOpKind,
}

enum TimelineOpKind {
    AddTimer(ScriptTimer),
    CancelTimer { id: u64, owner: u64 },
    AddCoroutine(ScriptCoroutine),
    RemoveCoroutine { id: u64, owner: u64 },
}

#[derive(Default)]
pub(crate) struct ScriptTimeline {
    next_id: u64,
    timers: Vec<ScriptTimer>,
    coroutines: Vec<ScriptCoroutine>,
}

impl ScriptTimeline {
    pub(crate) fn allocate_id(&mut self) -> Result<u64, &'static str> {
        let id = self
            .next_id
            .checked_add(1)
            .ok_or("timer id space exhausted")?;
        self.next_id = id;
        Ok(id)
    }

    pub(crate) fn can_add_timer(&self, pending: &[TimelineOp]) -> bool {
        self.timers.len()
            + pending
                .iter()
                .filter(|op| matches!(&op.kind, TimelineOpKind::AddTimer(_)))
                .count()
            < MAX_SCRIPT_TIMERS
    }

    pub(crate) fn can_add_coroutine(&self, pending: &[TimelineOp]) -> bool {
        self.coroutines.len()
            + pending
                .iter()
                .filter(|op| matches!(&op.kind, TimelineOpKind::AddCoroutine(_)))
                .count()
            < MAX_SCRIPT_COROUTINES
    }

    pub(crate) fn timer_exists(&self, id: u64, owner: u64) -> bool {
        self.timers
            .iter()
            .any(|timer| timer.id == id && timer.owner == owner)
    }

    pub(crate) fn timer_pending(pending: &[TimelineOp], id: u64, owner: u64) -> bool {
        pending.iter().any(|operation| match &operation.kind {
            TimelineOpKind::AddTimer(timer) => timer.id == id && timer.owner == owner,
            TimelineOpKind::CancelTimer {
                id: canceled_id,
                owner: canceled_owner,
            } => *canceled_id == id && *canceled_owner == owner,
            _ => false,
        })
    }

    pub(crate) fn stage_timer(
        pending: &mut Vec<TimelineOp>,
        id: u64,
        owner: u64,
        due: f64,
        repeat: Option<f64>,
        callback: TimerCallback,
    ) {
        pending.push(TimelineOp {
            kind: TimelineOpKind::AddTimer(ScriptTimer {
                id,
                owner,
                due,
                repeat,
                callback,
                eligible: false,
            }),
        });
    }

    pub(crate) fn stage_cancel_timer(pending: &mut Vec<TimelineOp>, id: u64, owner: u64) {
        pending.push(TimelineOp {
            kind: TimelineOpKind::CancelTimer { id, owner },
        });
    }

    pub(crate) fn stage_coroutine(pending: &mut Vec<TimelineOp>, coroutine: ScriptCoroutine) {
        pending.push(TimelineOp {
            kind: TimelineOpKind::AddCoroutine(coroutine),
        });
    }

    pub(crate) fn stage_coroutine_remove(pending: &mut Vec<TimelineOp>, id: u64, owner: u64) {
        pending.push(TimelineOp {
            kind: TimelineOpKind::RemoveCoroutine { id, owner },
        });
    }

    pub(crate) fn apply(&mut self, pending: &mut Vec<TimelineOp>) {
        for operation in pending.drain(..) {
            match operation.kind {
                TimelineOpKind::AddTimer(timer) => {
                    if self.timers.len() < MAX_SCRIPT_TIMERS {
                        self.timers.push(timer);
                    }
                }
                TimelineOpKind::CancelTimer { id, owner } => {
                    self.timers
                        .retain(|timer| timer.id != id || timer.owner != owner);
                }
                TimelineOpKind::AddCoroutine(coroutine) => {
                    if self.coroutines.len() < MAX_SCRIPT_COROUTINES {
                        self.coroutines.push(coroutine);
                    }
                }
                TimelineOpKind::RemoveCoroutine { id, owner } => {
                    self.coroutines
                        .retain(|coroutine| coroutine.id != id || coroutine.owner != owner);
                }
            }
        }
    }

    pub(crate) fn snapshot_due(&mut self, frame: u64, elapsed: f64) {
        for timer in &mut self.timers {
            timer.eligible = timer.due <= elapsed;
        }
        for coroutine in &mut self.coroutines {
            coroutine.eligible = match coroutine.wait {
                CoroutineWait::Frame(due) => frame >= due,
                CoroutineWait::Time(due) => elapsed >= due,
            };
        }
    }

    pub(crate) fn take_due_timers(&mut self, now: f64, out: &mut Vec<DueTimer>) {
        out.clear();
        let mut index = 0;
        while index < self.timers.len() {
            if !self.timers[index].eligible || self.timers[index].due > now {
                self.timers[index].eligible = false;
                index += 1;
                continue;
            }
            let timer = &mut self.timers[index];
            timer.eligible = false;
            let due = timer.due;
            if let Some(repeat) = timer.repeat {
                timer.due = next_repeat_due(due, repeat, now);
                out.push(DueTimer {
                    id: timer.id,
                    owner: timer.owner,
                    repeating: true,
                    callback: timer.callback.clone(),
                });
                index += 1;
            } else {
                let timer = &self.timers[index];
                out.push(DueTimer {
                    id: timer.id,
                    owner: timer.owner,
                    repeating: false,
                    callback: timer.callback.clone(),
                });
                index += 1;
            }
        }
        out.sort_unstable_by_key(|entry| entry.id);
    }

    pub(crate) fn finish_due_timer(&mut self, timer: &DueTimer) {
        if !timer.repeating {
            self.timers
                .retain(|entry| entry.id != timer.id || entry.owner != timer.owner);
        }
    }

    pub(crate) fn take_due_coroutines(&mut self, out: &mut Vec<ScriptCoroutine>) {
        out.clear();
        let mut index = 0;
        while index < self.coroutines.len() {
            if self.coroutines[index].eligible {
                out.push(self.coroutines.swap_remove(index));
            } else {
                index += 1;
            }
        }
        out.sort_unstable_by_key(|entry| entry.id);
    }

    pub(crate) fn cancel_owner(&mut self, owner: u64) {
        self.timers.retain(|timer| timer.owner != owner);
        self.coroutines.retain(|coroutine| coroutine.owner != owner);
    }

    pub(crate) fn clear(&mut self) {
        self.timers.clear();
        self.coroutines.clear();
    }
}

fn next_repeat_due(due: f64, repeat: f64, now: f64) -> f64 {
    let missed = ((now - due) / repeat).floor().max(0.0);
    let next = due + (missed + 1.0) * repeat;
    if next.is_finite() && next > now {
        next
    } else {
        now + repeat
    }
}
