//! Minimal vector and AABB math.

use std::ops::{Add, Mul, Sub};

/// 3D vector (f64).
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Vec3 {
    pub x: f64,
    pub y: f64,
    pub z: f64,
}

impl Vec3 {
    pub const ZERO: Vec3 = Vec3::new(0.0, 0.0, 0.0);

    /// Creates a vector.
    pub const fn new(x: f64, y: f64, z: f64) -> Self {
        Self { x, y, z }
    }

    /// Euclidean length.
    pub fn length(self) -> f64 {
        (self.x * self.x + self.y * self.y + self.z * self.z).sqrt()
    }

    /// Horizontal length.
    pub fn horizontal_length(self) -> f64 {
        (self.x * self.x + self.z * self.z).sqrt()
    }

    /// Distance to another point.
    pub fn distance(self, o: Vec3) -> f64 {
        (self - o).length()
    }

    /// As `[f32; 3]` for the wire.
    pub fn to_f32(self) -> [f32; 3] {
        [self.x as f32, self.y as f32, self.z as f32]
    }

    /// From a wire vector.
    pub fn from_f32(v: [f32; 3]) -> Self {
        Self::new(v[0] as f64, v[1] as f64, v[2] as f64)
    }
}

impl Add for Vec3 {
    type Output = Vec3;
    fn add(self, o: Vec3) -> Vec3 {
        Vec3::new(self.x + o.x, self.y + o.y, self.z + o.z)
    }
}

impl Sub for Vec3 {
    type Output = Vec3;
    fn sub(self, o: Vec3) -> Vec3 {
        Vec3::new(self.x - o.x, self.y - o.y, self.z - o.z)
    }
}

impl Mul<f64> for Vec3 {
    type Output = Vec3;
    fn mul(self, s: f64) -> Vec3 {
        Vec3::new(self.x * s, self.y * s, self.z * s)
    }
}

/// Axis-aligned bounding box.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Aabb {
    pub min: Vec3,
    pub max: Vec3,
}

impl Aabb {
    /// Creates a box from corners.
    pub const fn new(min: Vec3, max: Vec3) -> Self {
        Self { min, max }
    }

    /// Box of `width` × `height` whose bottom centre is `feet`.
    pub fn from_feet(feet: Vec3, width: f64, height: f64) -> Self {
        let h = width / 2.0;
        Self::new(
            Vec3::new(feet.x - h, feet.y, feet.z - h),
            Vec3::new(feet.x + h, feet.y + height, feet.z + h),
        )
    }

    /// Translated copy.
    pub fn offset(self, d: Vec3) -> Self {
        Self::new(self.min + d, self.max + d)
    }

    /// Box grown towards the movement direction.
    pub fn expand_towards(self, d: Vec3) -> Self {
        let mut b = self;
        if d.x < 0.0 {
            b.min.x += d.x
        } else {
            b.max.x += d.x
        }
        if d.y < 0.0 {
            b.min.y += d.y
        } else {
            b.max.y += d.y
        }
        if d.z < 0.0 {
            b.min.z += d.z
        } else {
            b.max.z += d.z
        }
        b
    }

    /// Box shrunk by `v` on every side.
    pub fn deflate(self, v: f64) -> Self {
        Self::new(self.min + Vec3::new(v, v, v), self.max - Vec3::new(v, v, v))
    }

    /// Strict overlap test.
    pub fn intersects(&self, o: &Aabb) -> bool {
        self.min.x < o.max.x
            && self.max.x > o.min.x
            && self.min.y < o.max.y
            && self.max.y > o.min.y
            && self.min.z < o.max.z
            && self.max.z > o.min.z
    }

    /// Clips `dx` so that `self` moving along X does not enter `other`.
    pub fn clip_x(&self, other: &Aabb, mut dx: f64) -> f64 {
        if other.max.y <= self.min.y || other.min.y >= self.max.y {
            return dx;
        }
        if other.max.z <= self.min.z || other.min.z >= self.max.z {
            return dx;
        }
        if dx > 0.0 && other.min.x >= self.max.x {
            dx = dx.min(other.min.x - self.max.x);
        } else if dx < 0.0 && other.max.x <= self.min.x {
            dx = dx.max(other.max.x - self.min.x);
        }
        dx
    }

    /// Clips `dy` against `other`.
    pub fn clip_y(&self, other: &Aabb, mut dy: f64) -> f64 {
        if other.max.x <= self.min.x || other.min.x >= self.max.x {
            return dy;
        }
        if other.max.z <= self.min.z || other.min.z >= self.max.z {
            return dy;
        }
        if dy > 0.0 && other.min.y >= self.max.y {
            dy = dy.min(other.min.y - self.max.y);
        } else if dy < 0.0 && other.max.y <= self.min.y {
            dy = dy.max(other.max.y - self.min.y);
        }
        dy
    }

    /// Clips `dz` against `other`.
    pub fn clip_z(&self, other: &Aabb, mut dz: f64) -> f64 {
        if other.max.x <= self.min.x || other.min.x >= self.max.x {
            return dz;
        }
        if other.max.y <= self.min.y || other.min.y >= self.max.y {
            return dz;
        }
        if dz > 0.0 && other.min.z >= self.max.z {
            dz = dz.min(other.min.z - self.max.z);
        } else if dz < 0.0 && other.max.z <= self.min.z {
            dz = dz.max(other.max.z - self.min.z);
        }
        dz
    }
}
