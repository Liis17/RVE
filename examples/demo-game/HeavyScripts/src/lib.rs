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

    fn on_update(&mut self, context: &mut Context<'_>, _dt: f32) {
        let phase = context.time().elapsed as f32 * self.speed;
        context.transform.position[1] = self.base_y + phase.sin() * self.amplitude;
    }
}

fn number(params: &Params, name: &str, default: f32) -> Result<f32, String> {
    let value = match params.get(name) {
        None => default,
        Some(Value::Int(value)) => *value as f32,
        Some(Value::Float(value)) => *value as f32,
        Some(_) => return Err(format!("{name} must be a number")),
    };
    if value.is_finite() {
        Ok(value)
    } else {
        Err(format!("{name} must be finite"))
    }
}

fn create_bobber(params: &Params) -> Result<Box<dyn Script>, String> {
    Ok(Box::new(Bobber {
        base_y: 0.0,
        amplitude: number(params, "amplitude", 0.15)?,
        speed: number(params, "speed", 1.4)?,
    }))
}

rve_sdk::export_scripts! { "Bobber" => create_bobber }

