use rve_sdk::{Context, Params, Script, Value};

struct Orbit {
    radius: f32,
    height: f32,
    speed: f32,
}

impl Script for Orbit {
    fn on_update(&mut self, context: &mut Context<'_>, _dt: f32) {
        let angle = (context.time().elapsed as f32 * self.speed.to_radians())
            .rem_euclid(std::f32::consts::TAU);
        context.transform.position = [
            self.radius * angle.sin(),
            self.height,
            self.radius * angle.cos(),
        ];
        let pitch = (1.0 - self.height).atan2(self.radius).to_degrees();
        context.transform.rotation_deg = [pitch, angle.to_degrees(), 0.0];
    }
}

fn float(params: &Params, name: &str) -> f32 {
    match params.get(name) {
        Some(Value::Float(value)) => *value as f32,
        _ => 0.0,
    }
}

fn create_orbit(params: &Params) -> Result<Box<dyn Script>, String> {
    Ok(Box::new(Orbit {
        radius: float(params, "radius"),
        height: float(params, "height"),
        speed: float(params, "speed"),
    }))
}

rve_sdk::export_scripts! {
    "Orbit" => create_orbit {
        radius: f32 = 6.0 (1.0..=20.0) "Orbit Radius",
        height: f32 = 2.2 (0.0..=10.0) "Camera Height",
        speed: f32 = 20.0 (-180.0..=180.0) "Orbit Speed (deg/s)",
    }
}
