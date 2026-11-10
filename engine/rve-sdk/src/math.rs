//! Small, dependency-free math types shared by Rust script examples.

use std::ops::{Add, Div, Mul, Neg, Sub};

/// Three-dimensional vector used by script APIs. Angles are expressed in degrees by [`Quat`].
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Vec3 {
    pub x: f32,
    pub y: f32,
    pub z: f32,
}

impl Vec3 {
    pub const ZERO: Self = Self::new(0.0, 0.0, 0.0);
    pub const X: Self = Self::new(1.0, 0.0, 0.0);
    pub const Y: Self = Self::new(0.0, 1.0, 0.0);
    pub const Z: Self = Self::new(0.0, 0.0, 1.0);

    pub const fn new(x: f32, y: f32, z: f32) -> Self {
        Self { x, y, z }
    }

    pub fn dot(self, rhs: Self) -> f32 {
        self.x * rhs.x + self.y * rhs.y + self.z * rhs.z
    }

    pub fn cross(self, rhs: Self) -> Self {
        Self::new(
            self.y * rhs.z - self.z * rhs.y,
            self.z * rhs.x - self.x * rhs.z,
            self.x * rhs.y - self.y * rhs.x,
        )
    }

    pub fn length_squared(self) -> f32 {
        self.dot(self)
    }

    pub fn length(self) -> f32 {
        self.length_squared().sqrt()
    }

    /// Returns zero for a zero or non-finite vector.
    pub fn normalize(self) -> Self {
        let length = self.length();
        if length.is_finite() && length > 0.0 {
            self / length
        } else {
            Self::ZERO
        }
    }

    pub fn lerp(self, rhs: Self, t: f32) -> Self {
        self + (rhs - self) * t
    }

    pub fn is_finite(self) -> bool {
        self.x.is_finite() && self.y.is_finite() && self.z.is_finite()
    }
}

impl Add for Vec3 {
    type Output = Self;
    fn add(self, rhs: Self) -> Self::Output {
        Self::new(self.x + rhs.x, self.y + rhs.y, self.z + rhs.z)
    }
}

impl Sub for Vec3 {
    type Output = Self;
    fn sub(self, rhs: Self) -> Self::Output {
        Self::new(self.x - rhs.x, self.y - rhs.y, self.z - rhs.z)
    }
}

impl Mul<f32> for Vec3 {
    type Output = Self;
    fn mul(self, rhs: f32) -> Self::Output {
        Self::new(self.x * rhs, self.y * rhs, self.z * rhs)
    }
}

impl Div<f32> for Vec3 {
    type Output = Self;
    fn div(self, rhs: f32) -> Self::Output {
        Self::new(self.x / rhs, self.y / rhs, self.z / rhs)
    }
}

/// Unit quaternion using right-handed, Y-up rotations and YXZ Euler angles in degrees.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Quat {
    pub x: f32,
    pub y: f32,
    pub z: f32,
    pub w: f32,
}

impl Default for Quat {
    fn default() -> Self {
        Self::IDENTITY
    }
}

impl Quat {
    pub const IDENTITY: Self = Self::new(0.0, 0.0, 0.0, 1.0);

    pub const fn new(x: f32, y: f32, z: f32, w: f32) -> Self {
        Self { x, y, z, w }
    }

    pub fn from_euler(euler_deg: Vec3) -> Self {
        if !euler_deg.is_finite() {
            return Self::IDENTITY;
        }
        let half = std::f32::consts::PI / 360.0;
        let (sx, cx) = (euler_deg.x * half).sin_cos();
        let (sy, cy) = (euler_deg.y * half).sin_cos();
        let (sz, cz) = (euler_deg.z * half).sin_cos();
        (Self::new(0.0, sy, 0.0, cy) * Self::new(sx, 0.0, 0.0, cx) * Self::new(0.0, 0.0, sz, cz))
            .normalize()
    }

    pub fn to_euler(self) -> Vec3 {
        let q = self.normalize();
        let r00 = 1.0 - 2.0 * (q.y * q.y + q.z * q.z);
        let r02 = 2.0 * (q.x * q.z + q.y * q.w);
        let r10 = 2.0 * (q.x * q.y + q.z * q.w);
        let r11 = 1.0 - 2.0 * (q.x * q.x + q.z * q.z);
        let r12 = 2.0 * (q.y * q.z - q.x * q.w);
        let r20 = 2.0 * (q.x * q.z - q.y * q.w);
        let r22 = 1.0 - 2.0 * (q.x * q.x + q.y * q.y);
        let x = (-r12).clamp(-1.0, 1.0).asin();
        let cx = x.cos();
        let (y, z) = if cx.abs() > 1.0e-6 {
            (r02.atan2(r22), r10.atan2(r11))
        } else {
            ((-r20).atan2(r00), 0.0)
        };
        let degrees = 180.0 / std::f32::consts::PI;
        Vec3::new(x * degrees, y * degrees, z * degrees)
    }

    pub fn dot(self, rhs: Self) -> f32 {
        self.x * rhs.x + self.y * rhs.y + self.z * rhs.z + self.w * rhs.w
    }

    pub fn normalize(self) -> Self {
        let length = self.dot(self).sqrt();
        if length.is_finite() && length > 0.0 {
            Self::new(
                self.x / length,
                self.y / length,
                self.z / length,
                self.w / length,
            )
        } else {
            Self::IDENTITY
        }
    }

    pub fn conjugate(self) -> Self {
        Self::new(-self.x, -self.y, -self.z, self.w)
    }

    pub fn rotate_vec3(self, value: Vec3) -> Vec3 {
        let q = self.normalize();
        let v = Vec3::new(q.x, q.y, q.z);
        value + v.cross(v.cross(value) + value * q.w) * 2.0
    }

    pub fn lerp(self, rhs: Self, t: f32) -> Self {
        let rhs = if self.dot(rhs) < 0.0 { -rhs } else { rhs };
        Self::new(
            self.x + (rhs.x - self.x) * t,
            self.y + (rhs.y - self.y) * t,
            self.z + (rhs.z - self.z) * t,
            self.w + (rhs.w - self.w) * t,
        )
        .normalize()
    }

    pub fn slerp(self, rhs: Self, t: f32) -> Self {
        let a = self.normalize();
        let mut b = rhs.normalize();
        let mut dot = a.dot(b);
        if dot < 0.0 {
            b = -b;
            dot = -dot;
        }
        if dot > 0.9995 {
            return a.lerp(b, t);
        }
        let theta = dot.clamp(-1.0, 1.0).acos();
        let sin_theta = theta.sin();
        if sin_theta.abs() < f32::EPSILON {
            return a;
        }
        let wa = ((1.0 - t) * theta).sin() / sin_theta;
        let wb = (t * theta).sin() / sin_theta;
        Self::new(
            a.x * wa + b.x * wb,
            a.y * wa + b.y * wb,
            a.z * wa + b.z * wb,
            a.w * wa + b.w * wb,
        )
        .normalize()
    }

    /// Builds a rotation whose local +Z axis points along `forward` and +Y follows `up`.
    /// Zero forward returns identity; parallel up is resolved with a deterministic world axis.
    pub fn look_rotation(forward: Vec3, up: Vec3) -> Self {
        let forward = forward.normalize();
        if forward == Vec3::ZERO {
            return Self::IDENTITY;
        }
        let mut right = up.normalize().cross(forward).normalize();
        if right == Vec3::ZERO {
            let fallback = if forward.y.abs() < 0.999 {
                Vec3::Y
            } else {
                Vec3::X
            };
            right = fallback.cross(forward).normalize();
        }
        let up = forward.cross(right).normalize();
        Self::from_basis(right, up, forward)
    }

    fn from_basis(x: Vec3, y: Vec3, z: Vec3) -> Self {
        let m00 = x.x;
        let m01 = y.x;
        let m02 = z.x;
        let m10 = x.y;
        let m11 = y.y;
        let m12 = z.y;
        let m20 = x.z;
        let m21 = y.z;
        let m22 = z.z;
        let trace = m00 + m11 + m22;
        let q = if trace > 0.0 {
            let s = (trace + 1.0).sqrt() * 2.0;
            Self::new((m21 - m12) / s, (m02 - m20) / s, (m10 - m01) / s, 0.25 * s)
        } else if m00 > m11 && m00 > m22 {
            let s = (1.0 + m00 - m11 - m22).sqrt() * 2.0;
            Self::new(0.25 * s, (m01 + m10) / s, (m02 + m20) / s, (m21 - m12) / s)
        } else if m11 > m22 {
            let s = (1.0 + m11 - m00 - m22).sqrt() * 2.0;
            Self::new((m01 + m10) / s, 0.25 * s, (m12 + m21) / s, (m02 - m20) / s)
        } else {
            let s = (1.0 + m22 - m00 - m11).sqrt() * 2.0;
            Self::new((m02 + m20) / s, (m12 + m21) / s, 0.25 * s, (m10 - m01) / s)
        };
        q.normalize()
    }
}

impl Mul for Quat {
    type Output = Self;
    fn mul(self, rhs: Self) -> Self::Output {
        Self::new(
            self.w * rhs.x + self.x * rhs.w + self.y * rhs.z - self.z * rhs.y,
            self.w * rhs.y - self.x * rhs.z + self.y * rhs.w + self.z * rhs.x,
            self.w * rhs.z + self.x * rhs.y - self.y * rhs.x + self.z * rhs.w,
            self.w * rhs.w - self.x * rhs.x - self.y * rhs.y - self.z * rhs.z,
        )
    }
}

impl Neg for Quat {
    type Output = Self;
    fn neg(self) -> Self::Output {
        Self::new(-self.x, -self.y, -self.z, -self.w)
    }
}

#[cfg(test)]
mod tests {
    use super::{Quat, Vec3};

    fn close(a: f32, b: f32) {
        assert!((a - b).abs() < 1.0e-4, "{a} != {b}");
    }

    #[test]
    fn vector_operations_and_zero_normalization_are_finite() {
        assert_eq!(Vec3::X.cross(Vec3::Y), Vec3::Z);
        close(Vec3::new(1.0, 2.0, 3.0).dot(Vec3::new(4.0, 5.0, 6.0)), 32.0);
        assert_eq!(Vec3::ZERO.normalize(), Vec3::ZERO);
        assert!(Vec3::new(f32::NAN, 0.0, 0.0).normalize().is_finite());
    }

    #[test]
    fn euler_yxz_roundtrips_and_look_rotation_handles_parallel_inputs() {
        let euler = Vec3::new(25.0, -40.0, 13.0);
        let roundtrip = Quat::from_euler(euler).to_euler();
        close(roundtrip.x, euler.x);
        close(roundtrip.y, euler.y);
        close(roundtrip.z, euler.z);
        let q = Quat::look_rotation(Vec3::Y, Vec3::Y);
        assert!(q.rotate_vec3(Vec3::Z).is_finite());
        close(q.rotate_vec3(Vec3::Z).dot(Vec3::Y), 1.0);
        assert_eq!(Quat::look_rotation(Vec3::ZERO, Vec3::Y), Quat::IDENTITY);
    }

    #[test]
    fn slerp_uses_shortest_arc_and_stays_normalized() {
        let start = Quat::IDENTITY;
        let end = Quat::from_euler(Vec3::new(0.0, 180.0, 0.0));
        let half = start.slerp(end, 0.5);
        close(half.dot(half), 1.0);
        close(half.rotate_vec3(Vec3::Z).x.abs(), 1.0);
    }
}
