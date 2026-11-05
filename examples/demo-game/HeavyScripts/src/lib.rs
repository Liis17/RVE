use rve_sdk::{Context, Params, Script, Value};

struct Bobber {
    base_y: f32,
    amplitude: f32,
    speed: f32,
}

impl Script for Bobber {
    fn on_start(&mut self, context: &mut Context<'_>) {
        self.base_y = context.transform.position[1];
        context.log_info("Bobber started");
    }

    fn on_update(&mut self, context: &mut Context<'_>, dt: f32) {
        let phase = context.time().elapsed as f32 * self.speed;
        let movement = context.input().vector("Move").unwrap_or_default();
        context.transform.position[1] = self.base_y + phase.sin() * self.amplitude;
        context.transform.position[0] += movement[0] * dt;
        context.transform.position[1] += movement[1] * dt;
    }

    fn on_message(&mut self, context: &mut Context<'_>, sender: &str, name: &str, value: &Value) {
        if name == "demo_ready" {
            context.log_info(format!("Bobber received {name} from {sender}: {value:?}"));
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

rve_sdk::export_scripts! {
    "Bobber" => create_bobber {
        amplitude: f32 = 0.15 (0.0..=10.0) "Amplitude",
        speed: f32 = 1.4 (0.0..=20.0) "Speed",
    }
}
