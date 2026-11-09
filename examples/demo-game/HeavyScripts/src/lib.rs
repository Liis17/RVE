use rve_sdk::{Context, Params, Quat, Script, Value, Vec3};

struct Bobber {
    base_y: f32,
    amplitude: f32,
    speed: f32,
}

impl Script for Bobber {
    fn on_start(&mut self, context: &mut Context<'_>) {
        self.base_y = context.transform.position[1];
        context.log_info("Bobber started");
        let starts = match context.save().get("bobber_starts") {
            Ok(Some(Value::Int(value))) => value,
            Ok(_) => 0,
            Err(error) => {
                context.log_warn(format!("Bobber save read failed: {error}"));
                0
            }
        };
        if let Err(error) = context
            .save()
            .set("bobber_starts", &Value::Int(starts.saturating_add(1)))
        {
            context.log_warn(format!("Bobber save write failed: {error}"));
        } else if let Err(error) = context.save().flush() {
            context.log_warn(format!("Bobber save flush failed: {error}"));
        }
        let forward = Quat::look_rotation(Vec3::Z, Vec3::Y).rotate_vec3(Vec3::Z);
        context.log_info(format!("Bobber math forward {forward:?}"));
        if let Err(error) = context.schedule(1.0, None, "pulse") {
            context.log_warn(format!("Bobber timer unavailable: {error}"));
        }
    }

    fn on_update(&mut self, context: &mut Context<'_>, dt: f32) {
        let movement = context.input().vector("Move").unwrap_or_default();
        context.transform.position[0] += movement[0] * dt;
        context.transform.position[1] += movement[1] * dt;
    }

    fn on_fixed_update(&mut self, context: &mut Context<'_>, _: f32) {
        let phase = context.script_time().elapsed as f32 * self.speed;
        context.transform.position[1] = self.base_y + phase.sin() * self.amplitude;
    }

    fn on_timer(&mut self, context: &mut Context<'_>, tag: &str) {
        context.log_info(format!("Bobber timer: {tag}"));
    }

    fn on_message(&mut self, context: &mut Context<'_>, sender: &str, name: &str, value: &Value) {
        if name == "demo_ready" {
            context.log_info(format!("Bobber received {name} from {sender}: {value:?}"));
        }
    }
}

struct PhysicsProbe {
    reported: bool,
}

impl Script for PhysicsProbe {
    fn on_start(&mut self, context: &mut Context<'_>) {
        if let Ok(controller) = context.object().get_controller(context) {
            context.log_info(format!(
                "PhysicsProbe controller radius {:.2}, gravity {}",
                controller.radius, controller.use_gravity
            ));
        }
        if let Some(crate_a) = context.find("crate-a")
            && let (Ok(collider), Ok(body)) = (
                crate_a.get_collider(context),
                crate_a.get_rigid_body(context),
            )
        {
            context.log_info(format!(
                "PhysicsProbe crate {:?} {:?}",
                collider.shape, body.kind
            ));
        }
    }

    fn on_update(&mut self, context: &mut Context<'_>, _: f32) {
        if self.reported {
            return;
        }
        self.reported = true;
        if let Ok(objects) = context.physics().overlap_sphere(
            context.transform.position,
            2.0,
            rve_sdk::QueryOptions::default(),
        ) {
            context.log_info(format!("PhysicsProbe overlap count {}", objects.len()));
        }
    }
}

/// The host has already checked parameters against the schema below and filled defaults.
fn float(params: &Params, name: &str) -> f32 {
    match params.get(name) {
        Some(Value::Float(value)) => *value as f32,
        _ => 0.0,
    }
}

fn create_bobber(params: &Params) -> Result<Box<dyn Script>, String> {
    Ok(Box::new(Bobber {
        base_y: 0.0,
        amplitude: float(params, "amplitude"),
        speed: float(params, "speed"),
    }))
}

fn create_physics_probe(_: &Params) -> Result<Box<dyn Script>, String> {
    Ok(Box::new(PhysicsProbe { reported: false }))
}

rve_sdk::export_scripts! {
    "Bobber" => create_bobber {
        amplitude: f32 = 0.15 (0.0..=10.0) "Amplitude",
        speed: f32 = 1.4 (0.0..=20.0) "Speed",
    },
    "PhysicsProbe" => create_physics_probe,
}
