use super::particle_step;

#[test]
fn first_submitted_frame_does_not_step_particles() {
    assert_eq!(particle_step(12.0, None), 0.0);
}

#[test]
fn step_is_simulation_time_since_last_submitted_frame() {
    assert!((particle_step(1.5, Some(1.25)) - 0.25).abs() < 1e-6);
}

#[test]
fn non_advancing_or_backwards_time_gives_no_step() {
    assert_eq!(particle_step(2.0, Some(2.0)), 0.0);
    assert_eq!(particle_step(1.0, Some(2.0)), 0.0);
}
