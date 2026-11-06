use std::fmt;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GeometryError {
    NonFiniteInput,
    InvalidDimensions,
    InvalidMass,
    InvalidRay,
    InvalidAabb,
    TooFewPoints,
    TooManyPoints,
    DegenerateHull,
    HullVertexLimit,
    IterationLimit,
    EmptyTriangleMesh,
    InvalidTriangleIndex,
    DegenerateTriangle,
    TriangleMeshMassUnsupported,
}

impl fmt::Display for GeometryError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let message = match self {
            Self::NonFiniteInput => "geometry input must be finite",
            Self::InvalidDimensions => "shape dimensions are invalid",
            Self::InvalidMass => "mass must be positive and finite",
            Self::InvalidRay => {
                "ray requires a finite origin, nonzero direction, and finite distance"
            }
            Self::InvalidAabb => "AABB bounds must be finite and ordered",
            Self::TooFewPoints => "convex hull requires at least four unique points",
            Self::TooManyPoints => "geometry input exceeds its configured limit",
            Self::DegenerateHull => "convex hull points are coplanar or degenerate",
            Self::HullVertexLimit => "convex hull exceeds the vertex limit",
            Self::IterationLimit => "convex hull exceeded its iteration limit",
            Self::EmptyTriangleMesh => "triangle mesh requires at least one triangle",
            Self::InvalidTriangleIndex => "triangle mesh index is out of range",
            Self::DegenerateTriangle => "triangle mesh contains a degenerate triangle",
            Self::TriangleMeshMassUnsupported => {
                "triangle meshes do not have dynamic mass properties"
            }
        };
        formatter.write_str(message)
    }
}

impl std::error::Error for GeometryError {}
