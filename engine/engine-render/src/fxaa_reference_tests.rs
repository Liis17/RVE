// The renderer has no CPU GLSL interpreter; build.rs compiles the actual shader with Naga.
// This small numeric oracle covers edge-coordinate behavior without a Vulkan device.
fn fxaa_edge_offset(
    sample_luma: impl Fn(f32, f32) -> f32,
    uv: [f32; 2],
    pixel: [f32; 2],
) -> (bool, [f32; 2], f32) {
    let at = |x: f32, y: f32| sample_luma(uv[0] + x * pixel[0], uv[1] + y * pixel[1]);
    let m = at(0.0, 0.0);
    let n = at(0.0, -1.0);
    let w = at(-1.0, 0.0);
    let e = at(1.0, 0.0);
    let s = at(0.0, 1.0);
    let nw = at(-1.0, -1.0);
    let ne = at(1.0, -1.0);
    let sw = at(-1.0, 1.0);
    let se = at(1.0, 1.0);

    let horizontal_1 = -2.0 * m + n + s;
    let vertical_1 = -2.0 * m + w + e;
    let horizontal_2 = -2.0 * e + ne + se;
    let vertical_2 = -2.0 * n + nw + ne;
    let horizontal_3 = -2.0 * w + nw + sw;
    let vertical_3 = -2.0 * s + sw + se;
    let edge_horizontal = horizontal_3.abs() + 2.0 * horizontal_1.abs() + horizontal_2.abs();
    let edge_vertical = vertical_3.abs() + 2.0 * vertical_1.abs() + vertical_2.abs();
    let horizontal = edge_horizontal >= edge_vertical;

    let (luma_a, luma_b) = if horizontal { (n, s) } else { (w, e) };
    let gradient_a = (luma_a - m).abs();
    let gradient_b = (luma_b - m).abs();
    let use_a = gradient_a >= gradient_b;
    let edge_luma = 0.5 * (m + if use_a { luma_a } else { luma_b });
    let gradient = gradient_a.max(gradient_b);
    let normal = if horizontal { [0.0, 1.0] } else { [1.0, 0.0] };
    let along = if horizontal { [1.0, 0.0] } else { [0.0, 1.0] };
    let side = if use_a { -0.5 } else { 0.5 };
    let edge_start = [normal[0] * side, normal[1] * side];
    let mut end_a = [edge_start[0] - along[0], edge_start[1] - along[1]];
    let mut end_b = [edge_start[0] + along[0], edge_start[1] + along[1]];
    let mut luma_end_a = at(end_a[0], end_a[1]);
    let mut luma_end_b = at(end_b[0], end_b[1]);
    let mut done_a = (luma_end_a - edge_luma).abs() >= gradient * 0.25;
    let mut done_b = (luma_end_b - edge_luma).abs() >= gradient * 0.25;
    const STEPS: [f32; 11] = [1.0, 1.0, 1.0, 1.0, 1.5, 2.0, 2.0, 2.0, 2.0, 4.0, 8.0];
    for step in STEPS {
        if !done_a {
            end_a[0] -= along[0] * step;
            end_a[1] -= along[1] * step;
            luma_end_a = at(end_a[0], end_a[1]);
            done_a = (luma_end_a - edge_luma).abs() >= gradient * 0.25;
        }
        if !done_b {
            end_b[0] += along[0] * step;
            end_b[1] += along[1] * step;
            luma_end_b = at(end_b[0], end_b[1]);
            done_b = (luma_end_b - edge_luma).abs() >= gradient * 0.25;
        }
    }

    let (distance_a, distance_b) = if horizontal {
        (end_a[0].abs(), end_b[0].abs())
    } else {
        (end_a[1].abs(), end_b[1].abs())
    };
    let nearest_luma = if distance_a < distance_b {
        luma_end_a
    } else {
        luma_end_b
    };
    let good_span = (nearest_luma < edge_luma) != (m < edge_luma);
    let edge_offset = if good_span {
        0.5 - distance_a.min(distance_b) / (distance_a + distance_b)
    } else {
        0.0
    };
    let direction = if use_a { -1.0 } else { 1.0 };
    (
        horizontal,
        [normal[0] * direction, normal[1] * direction],
        edge_offset,
    )
}

fn finite_step_luma(
    position: [f32; 2],
    horizontal: bool,
    center_positive: bool,
    bright_inside: bool,
) -> f32 {
    let texel = |x: i32, y: i32| {
        let (across, along) = if horizontal { (y, x) } else { (x, y) };
        let inside_side = if center_positive {
            across >= 1
        } else {
            across <= 0
        };
        let inside = (-1..=3).contains(&along) && inside_side;
        let high = if bright_inside { inside } else { !inside };
        if high { 1.0 } else { 0.0 }
    };
    let x0 = position[0].floor() as i32;
    let y0 = position[1].floor() as i32;
    let tx = position[0] - x0 as f32;
    let ty = position[1] - y0 as f32;
    let top = texel(x0, y0) * (1.0 - tx) + texel(x0 + 1, y0) * tx;
    let bottom = texel(x0, y0 + 1) * (1.0 - tx) + texel(x0 + 1, y0 + 1) * tx;
    top * (1.0 - ty) + bottom * ty
}

#[test]
fn finite_step_edges_have_oriented_offsets_for_both_polarities() {
    for horizontal in [false, true] {
        for bright_inside in [false, true] {
            let center_positive = bright_inside;
            let uv = if horizontal {
                [0.0, if center_positive { 1.0 } else { 0.0 }]
            } else {
                [if center_positive { 1.0 } else { 0.0 }, 0.0]
            };
            let sample =
                |x, y| finite_step_luma([x, y], horizontal, center_positive, bright_inside);
            let (detected_horizontal, direction, offset) = fxaa_edge_offset(sample, uv, [1.0, 1.0]);
            let expected_sign = if center_positive { -1.0 } else { 1.0 };
            let normal_axis = if horizontal { 1 } else { 0 };

            assert_eq!(detected_horizontal, horizontal);
            assert!(
                offset.is_finite() && offset > 0.0,
                "{horizontal}, {bright_inside}"
            );
            assert!(
                direction[normal_axis] * expected_sign > 0.0,
                "{horizontal}, {bright_inside}: {direction:?}"
            );
            assert_eq!(direction[1 - normal_axis], 0.0);
        }
    }
}

#[test]
fn subpixel_filter_uses_weighted_twelve_sample_average_and_squared_smoothstep() {
    let center: f32 = 0.5;
    let cardinal_sum: f32 = 4.0;
    let diagonal_sum: f32 = 0.0;
    let range: f32 = 1.0;
    let average = (2.0 * cardinal_sum + diagonal_sum) / 12.0;
    let value = ((average - center).abs() / range).clamp(0.0, 1.0);
    let smooth = value * value * (3.0 - 2.0 * value);
    let offset = smooth * smooth * 0.75;

    assert!((offset - 0.004_115_226).abs() < 1e-6);
}
