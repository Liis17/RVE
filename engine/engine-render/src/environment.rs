//! Environment input contracts and CPU lighting helpers.

use crate::RenderError;

/// Stable renderer-local environment resource id.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct EnvironmentId(pub(crate) usize);

/// Source data uploaded and convolved when an environment is created.
#[derive(Clone, Debug, PartialEq)]
pub enum EnvironmentSource {
    Equirect {
        width: u32,
        height: u32,
        rgb: Vec<f32>,
    },
    Procedural(ProceduralSky),
}

/// Parameters used to bake a procedural environment cubemap.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ProceduralSky {
    pub zenith_color: [f32; 3],
    pub horizon_color: [f32; 3],
    pub ground_color: [f32; 3],
    pub sun_size_deg: f32,
    pub sun_intensity: f32,
    /// Direction from the scene origin toward the sun.
    pub sun_direction: [f32; 3],
}

pub(crate) fn validate_source(source: &EnvironmentSource) -> Result<(), RenderError> {
    match source {
        EnvironmentSource::Equirect { width, height, rgb } => {
            let expected = (*width as usize)
                .checked_mul(*height as usize)
                .and_then(|count| count.checked_mul(3));
            if *width == 0 || *height == 0 || expected != Some(rgb.len()) {
                return Err(RenderError::InvalidEnvironment(
                    "equirect dimensions must be non-zero and match RGB data".to_owned(),
                ));
            }
            if !rgb.iter().all(|value| value.is_finite() && *value >= 0.0) {
                return Err(RenderError::InvalidEnvironment(
                    "equirect RGB values must be finite and non-negative".to_owned(),
                ));
            }
        }
        EnvironmentSource::Procedural(sky) => {
            if [sky.zenith_color, sky.horizon_color, sky.ground_color]
                .into_iter()
                .flatten()
                .any(|value| !value.is_finite() || value < 0.0)
            {
                return Err(RenderError::InvalidEnvironment(
                    "procedural colors must be finite and non-negative".to_owned(),
                ));
            }
            if !sky.sun_size_deg.is_finite()
                || !(0.0..=180.0).contains(&sky.sun_size_deg)
                || sky.sun_size_deg == 0.0
            {
                return Err(RenderError::InvalidEnvironment(
                    "procedural sun_size_deg must be in (0, 180]".to_owned(),
                ));
            }
            if !sky.sun_intensity.is_finite() || sky.sun_intensity < 0.0 {
                return Err(RenderError::InvalidEnvironment(
                    "procedural sun_intensity must be finite and non-negative".to_owned(),
                ));
            }
            let length_squared = sky.sun_direction.into_iter().map(|v| v * v).sum::<f32>();
            if !sky.sun_direction.into_iter().all(f32::is_finite)
                || !length_squared.is_finite()
                || length_squared <= 1e-12
            {
                return Err(RenderError::InvalidEnvironment(
                    "procedural sun_direction must be finite and non-zero".to_owned(),
                ));
            }
        }
    }
    Ok(())
}

/// Integrates the split-sum GGX BRDF LUT in row-major order (NdotV, roughness).
pub fn integrate_brdf_lut(size: u32, sample_count: u32) -> Vec<[f32; 2]> {
    let mut lut = Vec::with_capacity((size * size) as usize);
    for y in 0..size {
        let roughness = (y as f32 + 0.5) / size as f32;
        for x in 0..size {
            let n_dot_v = (x as f32 + 0.5) / size as f32;
            lut.push(integrate_brdf(n_dot_v, roughness, sample_count));
        }
    }
    lut
}

fn integrate_brdf(n_dot_v: f32, roughness: f32, sample_count: u32) -> [f32; 2] {
    let n_dot_v = n_dot_v.clamp(1e-4, 1.0);
    let roughness = roughness.clamp(0.0, 1.0);
    let view = [(1.0 - n_dot_v * n_dot_v).sqrt(), 0.0, n_dot_v];
    let alpha = roughness.max(0.001).powi(2);
    let alpha_squared = alpha * alpha;
    let mut a = 0.0;
    let mut b = 0.0;
    for index in 0..sample_count.max(1) {
        let xi_x = index as f32 / sample_count.max(1) as f32;
        let xi_y = radical_inverse_vdc(index);
        let phi = std::f32::consts::TAU * xi_x;
        let cos_theta = ((1.0 - xi_y) / (1.0 + (alpha_squared - 1.0) * xi_y)).sqrt();
        let sin_theta = (1.0 - cos_theta * cos_theta).max(0.0).sqrt();
        let half = [phi.cos() * sin_theta, phi.sin() * sin_theta, cos_theta];
        let v_dot_h = (view[0] * half[0] + view[1] * half[1] + view[2] * half[2]).max(0.0);
        let light = [
            2.0 * v_dot_h * half[0] - view[0],
            2.0 * v_dot_h * half[1] - view[1],
            2.0 * v_dot_h * half[2] - view[2],
        ];
        let n_dot_l = light[2].max(0.0);
        let n_dot_h = half[2].max(0.0);
        if n_dot_l <= 0.0 || n_dot_h <= 0.0 {
            continue;
        }
        let geometry = smith_ggx(n_dot_v, roughness) * smith_ggx(n_dot_l, roughness);
        let visibility = geometry * v_dot_h / (n_dot_h * n_dot_v).max(1e-6);
        let fresnel = (1.0 - v_dot_h).powi(5);
        a += (1.0 - fresnel) * visibility;
        b += fresnel * visibility;
    }
    let scale = 1.0 / sample_count.max(1) as f32;
    [a * scale, b * scale]
}

fn smith_ggx(n_dot: f32, roughness: f32) -> f32 {
    let k = roughness * roughness * 0.5;
    n_dot / (n_dot * (1.0 - k) + k).max(1e-6)
}

fn radical_inverse_vdc(bits: u32) -> f32 {
    let bits = bits.reverse_bits();
    bits as f32 * 2.328_306_4e-10
}

pub(crate) fn float_to_half(value: f32) -> u16 {
    let bits = value.to_bits();
    let sign = ((bits >> 16) & 0x8000) as u16;
    let exponent = ((bits >> 23) & 0xff) as i32 - 127 + 15;
    let mantissa = bits & 0x7f_ffff;
    if exponent <= 0 {
        if exponent < -10 {
            return sign;
        }
        let significand = (mantissa | 0x80_0000) >> (1 - exponent);
        return sign | ((significand + 0x1000) >> 13) as u16;
    }
    if exponent >= 31 {
        return sign | 0x7c00 | u16::from(mantissa != 0);
    }
    sign | ((exponent as u16) << 10) | ((mantissa + 0x1000) >> 13) as u16
}

/// Inverts a column-major 4×4 matrix, returning `None` for singular or non-finite input.
pub fn inverse4x4(matrix: [[f32; 4]; 4]) -> Option<[[f32; 4]; 4]> {
    let mut rows = [[0.0_f64; 8]; 4];
    for row in 0..4 {
        for column in 0..4 {
            rows[row][column] = f64::from(matrix[column][row]);
        }
        rows[row][row + 4] = 1.0;
    }
    for pivot in 0..4 {
        let best =
            (pivot..4).max_by(|&a, &b| rows[a][pivot].abs().total_cmp(&rows[b][pivot].abs()))?;
        if !rows[best][pivot].is_finite() || rows[best][pivot].abs() <= 1e-12 {
            return None;
        }
        rows.swap(pivot, best);
        let divisor = rows[pivot][pivot];
        for value in &mut rows[pivot] {
            *value /= divisor;
        }
        for row in 0..4 {
            if row == pivot {
                continue;
            }
            let factor = rows[row][pivot];
            let pivot_row = rows[pivot];
            for (column, value) in rows[row].iter_mut().enumerate() {
                *value -= factor * pivot_row[column];
            }
        }
    }
    let mut result = [[0.0_f32; 4]; 4];
    for row in 0..4 {
        for column in 0..4 {
            let value = rows[row][column + 4] as f32;
            if !value.is_finite() {
                return None;
            }
            result[column][row] = value;
        }
    }
    Some(result)
}

#[cfg(test)]
mod tests {
    use super::{
        EnvironmentSource, ProceduralSky, float_to_half, integrate_brdf_lut, inverse4x4,
        validate_source,
    };
    use crate::RenderError;

    #[test]
    fn brdf_lut_values_are_finite_bounded_and_stable_at_zero_roughness() {
        let lut = integrate_brdf_lut(64, 128);
        assert_eq!(lut.len(), 4096);
        assert!(
            lut.iter()
                .flatten()
                .all(|value| value.is_finite() && (0.0..=1.1).contains(value))
        );
        let smooth = super::integrate_brdf(0.7, 0.0, 1024);
        let near_smooth = super::integrate_brdf(0.7, 0.001, 1024);
        assert!(
            smooth
                .into_iter()
                .all(|value| value.is_finite() && (0.0..=1.1).contains(&value))
        );
        assert!((smooth[0] - near_smooth[0]).abs() < 0.02);
        assert!((smooth[1] - near_smooth[1]).abs() < 0.02);
    }

    #[test]
    fn validates_environment_sources_without_gpu_state() {
        assert!(
            validate_source(&EnvironmentSource::Equirect {
                width: 1,
                height: 1,
                rgb: vec![0.0; 3]
            })
            .is_ok()
        );
        assert!(matches!(
            validate_source(&EnvironmentSource::Equirect {
                width: u32::MAX,
                height: u32::MAX,
                rgb: vec![]
            }),
            Err(RenderError::InvalidEnvironment(_))
        ));
        assert!(matches!(
            validate_source(&EnvironmentSource::Equirect {
                width: 1,
                height: 1,
                rgb: vec![f32::NAN; 3]
            }),
            Err(RenderError::InvalidEnvironment(_))
        ));
        let sky = ProceduralSky {
            zenith_color: [0.1, 0.2, 0.4],
            horizon_color: [0.8, 0.7, 0.5],
            ground_color: [0.1; 3],
            sun_size_deg: 2.0,
            sun_intensity: 4.0,
            sun_direction: [0.0, 1.0, 0.0],
        };
        assert!(validate_source(&EnvironmentSource::Procedural(sky)).is_ok());
        assert!(matches!(
            validate_source(&EnvironmentSource::Procedural(ProceduralSky {
                sun_direction: [0.0; 3],
                ..sky
            })),
            Err(RenderError::InvalidEnvironment(_))
        ));
    }

    #[test]
    fn inverse4x4_round_trips_column_major_matrices() {
        let matrix = [
            [2.0, 0.0, 0.0, 0.0],
            [0.0, 3.0, 0.0, 0.0],
            [0.0, 0.0, 4.0, 0.0],
            [5.0, -7.0, 9.0, 1.0],
        ];
        let inverse = inverse4x4(matrix).unwrap();
        let product = multiply(matrix, inverse);
        for (column, values) in product.iter().enumerate() {
            for (row, value) in values.iter().enumerate() {
                assert!((*value - if column == row { 1.0 } else { 0.0 }).abs() < 1e-6);
            }
        }
        assert!(inverse4x4([[0.0; 4]; 4]).is_none());
    }

    #[test]
    fn float_to_half_rounds_expected_hdr_values() {
        assert_eq!(float_to_half(0.0), 0x0000);
        assert_eq!(float_to_half(1.0), 0x3c00);
        assert_eq!(float_to_half(2.0), 0x4000);
        assert_eq!(float_to_half(65504.0), 0x7bff);
    }

    fn multiply(a: [[f32; 4]; 4], b: [[f32; 4]; 4]) -> [[f32; 4]; 4] {
        let mut out = [[0.0; 4]; 4];
        for column in 0..4 {
            for row in 0..4 {
                out[column][row] = (0..4).map(|index| a[index][row] * b[column][index]).sum();
            }
        }
        out
    }
}
