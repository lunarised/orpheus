//! Procedural, old-school 3D pipes screensaver.
//!
//! The scene is intentionally deterministic for a given elapsed time. That
//! makes animation restarts clean, avoids keeping another timer thread alive,
//! and gives the geometry useful unit-test coverage.

use std::cmp::Ordering;

const PIPE_COUNT: usize = 4;
const SEGMENTS_PER_PIPE: usize = 18;
const SEGMENTS_PER_SECOND: f64 = 3.2;
const PIPE_STAGGER_SECONDS: f64 = 0.55;
const CYCLE_SECONDS: f64 = 10.0;
const WORLD_X_LIMIT: i8 = 4;
const WORLD_Y_LIMIT: i8 = 2;
const WORLD_Z_LIMIT: i8 = 3;
const PIPE_COLORS: [[u8; 3]; 6] = [
    [0x2D, 0xE6, 0x72],
    [0x26, 0xC6, 0xF7],
    [0xF0, 0x4F, 0xD2],
    [0xFF, 0xC1, 0x38],
    [0x8B, 0x7C, 0xFF],
    [0xFF, 0x65, 0x55],
];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct GridPoint {
    x: i8,
    y: i8,
    z: i8,
}

impl GridPoint {
    fn moved(self, direction: Direction) -> Self {
        let (dx, dy, dz) = direction.delta();
        Self {
            x: self.x + dx,
            y: self.y + dy,
            z: self.z + dz,
        }
    }

    fn in_bounds(self) -> bool {
        self.x.abs() <= WORLD_X_LIMIT
            && self.y.abs() <= WORLD_Y_LIMIT
            && self.z.abs() <= WORLD_Z_LIMIT
    }

    fn as_vec3(self) -> Vec3 {
        Vec3 {
            x: f64::from(self.x),
            y: f64::from(self.y),
            z: f64::from(self.z),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Direction {
    PositiveX,
    NegativeX,
    PositiveY,
    NegativeY,
    PositiveZ,
    NegativeZ,
}

impl Direction {
    const ALL: [Self; 6] = [
        Self::PositiveX,
        Self::NegativeX,
        Self::PositiveY,
        Self::NegativeY,
        Self::PositiveZ,
        Self::NegativeZ,
    ];

    fn delta(self) -> (i8, i8, i8) {
        match self {
            Self::PositiveX => (1, 0, 0),
            Self::NegativeX => (-1, 0, 0),
            Self::PositiveY => (0, 1, 0),
            Self::NegativeY => (0, -1, 0),
            Self::PositiveZ => (0, 0, 1),
            Self::NegativeZ => (0, 0, -1),
        }
    }

    fn opposite(self) -> Self {
        match self {
            Self::PositiveX => Self::NegativeX,
            Self::NegativeX => Self::PositiveX,
            Self::PositiveY => Self::NegativeY,
            Self::NegativeY => Self::PositiveY,
            Self::PositiveZ => Self::NegativeZ,
            Self::NegativeZ => Self::PositiveZ,
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct Vec3 {
    x: f64,
    y: f64,
    z: f64,
}

impl Vec3 {
    fn lerp(self, other: Self, amount: f64) -> Self {
        Self {
            x: self.x + (other.x - self.x) * amount,
            y: self.y + (other.y - self.y) * amount,
            z: self.z + (other.z - self.z) * amount,
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct ProjectedPoint {
    x: f64,
    y: f64,
    depth: f64,
    radius: f64,
}

#[derive(Debug, Clone, Copy)]
struct ProjectedSegment {
    start: ProjectedPoint,
    end: ProjectedPoint,
    color: [u8; 3],
}

impl ProjectedSegment {
    fn depth(self) -> f64 {
        (self.start.depth + self.end.depth) * 0.5
    }
}

#[derive(Debug, Clone)]
struct PipePath {
    points: Vec<GridPoint>,
    color: [u8; 3],
}

#[derive(Debug, Clone, Copy)]
struct Lcg(u64);

impl Lcg {
    fn next(&mut self) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        self.0
    }

    fn index(&mut self, length: usize) -> usize {
        (self.next() as usize) % length
    }

    fn coordinate(&mut self, limit: i8) -> i8 {
        self.index((limit as usize * 2) + 1) as i8 - limit
    }
}

/// Render one RGBA frame of the pipes animation.
pub fn render(width: u32, height: u32, elapsed_seconds: f64) -> Vec<u8> {
    if width == 0 || height == 0 {
        return Vec::new();
    }

    let elapsed_seconds = if elapsed_seconds.is_finite() {
        elapsed_seconds.max(0.0)
    } else {
        0.0
    };
    let cycle = (elapsed_seconds / CYCLE_SECONDS).floor() as u64;
    let cycle_time = elapsed_seconds % CYCLE_SECONDS;
    let paths = generate_paths(cycle);
    let mut projected = visible_segments(&paths, cycle_time, width, height);
    projected.sort_by(|left, right| {
        right
            .depth()
            .partial_cmp(&left.depth())
            .unwrap_or(Ordering::Equal)
    });

    let mut frame = background(width, height);
    for segment in projected {
        draw_segment(&mut frame, width, height, segment);
    }
    frame
}

fn generate_paths(cycle: u64) -> Vec<PipePath> {
    let mut random = Lcg(0xD1B5_4A32_D192_ED03 ^ cycle.wrapping_mul(0x9E37_79B9_7F4A_7C15));
    let mut paths = Vec::with_capacity(PIPE_COUNT);

    for pipe_index in 0..PIPE_COUNT {
        let start = GridPoint {
            x: random.coordinate(WORLD_X_LIMIT - 1),
            y: random.coordinate(WORLD_Y_LIMIT),
            z: random.coordinate(WORLD_Z_LIMIT - 1),
        };
        let mut points = Vec::with_capacity(SEGMENTS_PER_PIPE + 1);
        points.push(start);
        let mut current = start;
        let mut previous_direction = None;

        for _ in 0..SEGMENTS_PER_PIPE {
            let offset = random.index(Direction::ALL.len());
            let mut selected = None;

            // Prefer unvisited cells, which creates the pleasing sprawling
            // paths of the original while still allowing a fallback if a pipe
            // corners itself at a boundary.
            for allow_visited in [false, true] {
                for candidate_index in 0..Direction::ALL.len() {
                    let direction =
                        Direction::ALL[(offset + candidate_index) % Direction::ALL.len()];
                    if previous_direction
                        .is_some_and(|previous: Direction| direction == previous.opposite())
                    {
                        continue;
                    }
                    let next = current.moved(direction);
                    if next.in_bounds() && (allow_visited || !points.contains(&next)) {
                        selected = Some((direction, next));
                        break;
                    }
                }
                if selected.is_some() {
                    break;
                }
            }

            let Some((direction, next)) = selected else {
                break;
            };
            previous_direction = Some(direction);
            current = next;
            points.push(next);
        }

        paths.push(PipePath {
            points,
            color: PIPE_COLORS[(pipe_index + cycle as usize) % PIPE_COLORS.len()],
        });
    }
    paths
}

fn visible_segments(
    paths: &[PipePath],
    cycle_time: f64,
    width: u32,
    height: u32,
) -> Vec<ProjectedSegment> {
    let mut segments = Vec::with_capacity(PIPE_COUNT * SEGMENTS_PER_PIPE);
    for (pipe_index, path) in paths.iter().enumerate() {
        let growth = ((cycle_time - pipe_index as f64 * PIPE_STAGGER_SECONDS)
            * SEGMENTS_PER_SECOND)
            .max(0.0);
        let complete_segments = growth.floor() as usize;
        let partial = growth.fract();

        for (index, points) in path.points.windows(2).enumerate() {
            if index > complete_segments || (index == complete_segments && partial <= 0.0) {
                break;
            }
            let start = points[0].as_vec3();
            let end = if index < complete_segments {
                points[1].as_vec3()
            } else {
                start.lerp(points[1].as_vec3(), partial)
            };
            let start = project(start, width, height);
            let end = project(end, width, height);
            segments.push(ProjectedSegment {
                start,
                end,
                color: path.color,
            });
        }
    }
    segments
}

fn project(point: Vec3, width: u32, height: u32) -> ProjectedPoint {
    // A fixed yaw and downward pitch expose all three lattice axes. Perspective
    // scaling then makes receding segments and joints visibly smaller.
    let yaw = 0.66_f64;
    let pitch = -0.36_f64;
    let x_rotated = yaw.cos() * point.x - yaw.sin() * point.z;
    let z_rotated = yaw.sin() * point.x + yaw.cos() * point.z;
    let y_rotated = pitch.cos() * point.y - pitch.sin() * z_rotated;
    let depth = pitch.sin() * point.y + pitch.cos() * z_rotated;
    let camera_distance = 10.5;
    let perspective = camera_distance / (camera_distance + depth);
    let scale = (f64::from(width) / 12.0)
        .min(f64::from(height) / 5.1)
        .max(1.0);

    ProjectedPoint {
        x: f64::from(width) * 0.5 + x_rotated * scale * perspective,
        y: f64::from(height) * 0.5 - y_rotated * scale * perspective,
        depth,
        radius: (scale * 0.235 * perspective).clamp(1.0, 12.0),
    }
}

fn background(width: u32, height: u32) -> Vec<u8> {
    let mut frame = vec![0_u8; width as usize * height as usize * 4];
    let center_x = f64::from(width) * 0.5;
    let center_y = f64::from(height) * 0.5;
    let maximum_distance = center_x.hypot(center_y).max(1.0);
    for y in 0..height {
        for x in 0..width {
            let distance =
                (f64::from(x) - center_x).hypot(f64::from(y) - center_y) / maximum_distance;
            let glow = ((1.0 - distance).max(0.0) * 7.0) as u8;
            put_pixel(
                &mut frame,
                width,
                height,
                x as i32,
                y as i32,
                [4 + glow / 2, 6 + glow, 11 + glow, 255],
            );
        }
    }
    frame
}

fn draw_segment(frame: &mut [u8], width: u32, height: u32, segment: ProjectedSegment) {
    let radius = (segment.start.radius + segment.end.radius) * 0.5;
    draw_shaded_capsule(
        frame,
        width,
        height,
        (segment.start.x, segment.start.y),
        (segment.end.x, segment.end.y),
        radius,
        segment.color,
    );
    draw_shaded_joint(
        frame,
        width,
        height,
        segment.end.x,
        segment.end.y,
        segment.end.radius * 1.08,
        segment.color,
    );
}

fn draw_shaded_capsule(
    frame: &mut [u8],
    width: u32,
    height: u32,
    start: (f64, f64),
    end: (f64, f64),
    radius: f64,
    color: [u8; 3],
) {
    let shadow_radius = radius + 1.5;
    let min_x = (start.0.min(end.0) - shadow_radius).floor() as i32;
    let max_x = (start.0.max(end.0) + shadow_radius).ceil() as i32;
    let min_y = (start.1.min(end.1) - shadow_radius).floor() as i32;
    let max_y = (start.1.max(end.1) + shadow_radius).ceil() as i32;
    let dx = end.0 - start.0;
    let dy = end.1 - start.1;
    let length_squared = dx * dx + dy * dy;
    let length = length_squared.sqrt().max(1.0);
    let perpendicular = (-dy / length, dx / length);

    for y in min_y..=max_y {
        for x in min_x..=max_x {
            let pixel = (f64::from(x) + 0.5, f64::from(y) + 0.5);
            let along = if length_squared > f64::EPSILON {
                (((pixel.0 - start.0) * dx + (pixel.1 - start.1) * dy) / length_squared)
                    .clamp(0.0, 1.0)
            } else {
                0.0
            };
            let nearest = (start.0 + dx * along, start.1 + dy * along);
            let offset = (pixel.0 - nearest.0, pixel.1 - nearest.1);
            let distance = offset.0.hypot(offset.1);
            if distance > shadow_radius {
                continue;
            }
            if distance > radius {
                put_pixel(frame, width, height, x, y, [2, 3, 6, 255]);
                continue;
            }

            let across = (offset.0 * perpendicular.0 + offset.1 * perpendicular.1) / radius;
            let rounded = (1.0 - (distance / radius).powi(2)).max(0.0).sqrt();
            let highlight = (-across * 0.18).clamp(-0.18, 0.18);
            let brightness = (0.38 + rounded * 0.58 + highlight).clamp(0.22, 1.12);
            put_pixel(frame, width, height, x, y, shaded_color(color, brightness));
        }
    }
}

fn draw_shaded_joint(
    frame: &mut [u8],
    width: u32,
    height: u32,
    center_x: f64,
    center_y: f64,
    radius: f64,
    color: [u8; 3],
) {
    let shadow_radius = radius + 1.5;
    let min_x = (center_x - shadow_radius).floor() as i32;
    let max_x = (center_x + shadow_radius).ceil() as i32;
    let min_y = (center_y - shadow_radius).floor() as i32;
    let max_y = (center_y + shadow_radius).ceil() as i32;

    for y in min_y..=max_y {
        for x in min_x..=max_x {
            let nx = (f64::from(x) + 0.5 - center_x) / radius;
            let ny = (f64::from(y) + 0.5 - center_y) / radius;
            let distance_squared = nx * nx + ny * ny;
            if distance_squared > (shadow_radius / radius).powi(2) {
                continue;
            }
            if distance_squared > 1.0 {
                put_pixel(frame, width, height, x, y, [2, 3, 6, 255]);
                continue;
            }

            let nz = (1.0 - distance_squared).sqrt();
            let light = (-nx * 0.42 - ny * 0.52 + nz * 0.74).max(-0.35);
            let brightness = (0.42 + light * 0.63).clamp(0.2, 1.12);
            put_pixel(frame, width, height, x, y, shaded_color(color, brightness));
        }
    }
}

fn shaded_color(color: [u8; 3], brightness: f64) -> [u8; 4] {
    [
        (f64::from(color[0]) * brightness).clamp(0.0, 255.0) as u8,
        (f64::from(color[1]) * brightness).clamp(0.0, 255.0) as u8,
        (f64::from(color[2]) * brightness).clamp(0.0, 255.0) as u8,
        255,
    ]
}

fn put_pixel(frame: &mut [u8], width: u32, height: u32, x: i32, y: i32, color: [u8; 4]) {
    if x < 0 || y < 0 || x >= width as i32 || y >= height as i32 {
        return;
    }
    let index = (y as usize * width as usize + x as usize) * 4;
    if index + 3 >= frame.len() {
        return;
    }
    frame[index..index + 4].copy_from_slice(&color);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn colourful_pixels(frame: &[u8]) -> usize {
        frame
            .chunks_exact(4)
            .filter(|pixel| {
                let maximum = pixel[0].max(pixel[1]).max(pixel[2]);
                let minimum = pixel[0].min(pixel[1]).min(pixel[2]);
                maximum > 60 && maximum.saturating_sub(minimum) > 25
            })
            .count()
    }

    #[test]
    fn generated_paths_are_bounded_connected_and_deterministic() {
        let first = generate_paths(7);
        let repeated = generate_paths(7);
        assert_eq!(first.len(), PIPE_COUNT);
        assert_eq!(first.len(), repeated.len());

        for (path, repeated_path) in first.iter().zip(repeated.iter()) {
            assert_eq!(path.points, repeated_path.points);
            assert_eq!(path.color, repeated_path.color);
            assert!(path.points.len() > 8);
            assert!(path.points.iter().all(|point| point.in_bounds()));
            for points in path.points.windows(2) {
                let distance = (points[0].x - points[1].x).abs()
                    + (points[0].y - points[1].y).abs()
                    + (points[0].z - points[1].z).abs();
                assert_eq!(distance, 1);
            }
        }
    }

    #[test]
    fn each_cycle_changes_geometry_and_palette_assignment() {
        let first = generate_paths(1);
        let second = generate_paths(2);
        assert!(
            first
                .iter()
                .zip(second.iter())
                .any(|(left, right)| left.points != right.points || left.color != right.color)
        );
    }

    #[test]
    fn perspective_makes_nearer_pipe_sections_larger() {
        let near = project(
            Vec3 {
                x: 0.0,
                y: 0.0,
                z: -3.0,
            },
            428,
            142,
        );
        let far = project(
            Vec3 {
                x: 0.0,
                y: 0.0,
                z: 3.0,
            },
            428,
            142,
        );
        assert!(near.radius > far.radius);
    }

    #[test]
    fn animation_grows_coloured_geometry_into_an_opaque_frame() {
        let early = render(428, 142, 0.5);
        let later = render(428, 142, 5.5);

        assert_eq!(early.len(), 428 * 142 * 4);
        assert!(early.chunks_exact(4).all(|pixel| pixel[3] == 255));
        assert!(later.chunks_exact(4).all(|pixel| pixel[3] == 255));
        assert!(colourful_pixels(&later) > colourful_pixels(&early));
        assert!(colourful_pixels(&later) > 1_000);
    }

    #[test]
    fn invalid_time_and_empty_dimensions_are_safe() {
        assert!(render(0, 142, 1.0).is_empty());
        assert!(render(428, 0, 1.0).is_empty());
        assert_eq!(render(8, 8, f64::NAN).len(), 8 * 8 * 4);
    }
}
