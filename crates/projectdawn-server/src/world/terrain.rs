//! The server's ground (the height model, `docs/design/server_height_model.md`,
//! 2026-11-03). The client's `TerrainMesh` triangles and `Floor` plane, written
//! by `tools/export_terrain.gd` into `data/terrain.toml` and compiled in, so the
//! binary carries its own ground and samples exactly the surface the client
//! walks on: the same corners, the same diagonal, the same floor. Three
//! questions are answered here: how high the ground is at a point (`ground`),
//! whether a player may step from here to there (`slope_ok`), and whether two
//! points can see each other across it (`can_see`). Every answer is total: a
//! non-finite input gets the floor, a refused step, no sight.

use super::connection::Vec3f;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::sync::OnceLock;

const TERRAIN_TOML: &str = include_str!("../../data/terrain.toml");

/// The steepest ground a player may step up, in degrees: the client's
/// `floor_max_angle` (55) plus a margin, so an honest body, which its own
/// collider stopped first, is never refused.
pub const SLOPE_LIMIT_DEG: f32 = 60.0;
/// Slack on the slope rule, as a fraction of the allowed rise, for float
/// rounding. Relative, never a fixed height per step: a fixed centimetre a
/// tick would let a forged client climb any face at 20 cm/s with steps too
/// short to be measured against it (the review of 2026-11-04). NOT the
/// client's 0.5 m floor snap either: snapping keeps a body on the ground going
/// down, and half a metre allowed on every tick would be a vertical wall at
/// 10 m/s.
pub const SLOPE_SLACK: f32 = 0.001;
/// How high above the ground both ends of a sight line are taken: chest height
/// for a human, head height for a gnome, above a rat entirely. One constant for
/// everything until creatures carry a height; the same at both ends, so sight
/// is symmetric.
pub const LOS_HEIGHT: f32 = 1.2;
/// A sight line this far under the ground still sees, so a cast along flat
/// ground never fails on rounding.
pub const LOS_TOLERANCE: f32 = 0.01;

/// What `terrain.toml` carries. `heights` is row-major by z then x, as the
/// client's `TerrainMesh.heights` is; `hash` is the SHA-256 of the canonical
/// bytes (`Terrain::hash_hex`), written by the exporter and checked at parse,
/// so a hand edit is refused.
#[derive(Deserialize)]
struct Parsed {
    grid_size: usize,
    cell_size: f32,
    origin: [f32; 3],
    floor_y: f32,
    heights: Vec<f32>,
    hash: String,
}

#[derive(Debug, Clone)]
pub struct Terrain {
    grid_size: usize,
    cell_size: f32,
    origin: Vec3f,
    floor_y: f32,
    heights: Vec<f32>,
}

/// Where one tick of a player's movement ends (`Terrain::player_step`), and
/// whether the slope rule refused it.
#[derive(Debug, Clone, Copy)]
pub struct Step {
    /// The player's position after the tick, standing on the ground.
    pub pos: Vec3f,
    /// The slope rule refused the step and `pos` is where the player stood.
    /// An honest client's own collider stops it at such a face first, so a
    /// refusal is counted on the `world health` line (`health.rs`).
    pub refused: bool,
}

impl Step {
    fn taken(pos: Vec3f) -> Self {
        Step {
            pos,
            refused: false,
        }
    }
}

impl Terrain {
    /// A terrain from its parts, checked: a grid of at least one cell with a
    /// positive cell size, `(grid_size + 1)²` finite heights.
    pub fn from_heights(
        grid_size: usize,
        cell_size: f32,
        origin: [f32; 3],
        floor_y: f32,
        heights: Vec<f32>,
    ) -> Result<Self, String> {
        let origin = Vec3f {
            x: origin[0],
            y: origin[1],
            z: origin[2],
        };
        if grid_size == 0 {
            return Err("grid_size must be at least 1".into());
        }
        if !(cell_size.is_finite() && cell_size > 0.0) {
            return Err(format!(
                "cell_size must be a positive number, not {cell_size}"
            ));
        }
        let want = (grid_size + 1) * (grid_size + 1);
        if heights.len() != want {
            return Err(format!(
                "a {grid_size}x{grid_size} grid needs {want} heights, the file has {}",
                heights.len()
            ));
        }
        if !(origin.x.is_finite() && origin.y.is_finite() && origin.z.is_finite()) {
            return Err("origin must be finite".into());
        }
        if !floor_y.is_finite() {
            return Err("floor_y must be finite".into());
        }
        if let Some(i) = heights.iter().position(|h| !h.is_finite()) {
            return Err(format!("height {i} is not finite"));
        }
        Ok(Self {
            grid_size,
            cell_size,
            origin,
            floor_y,
            heights,
        })
    }

    /// A flat terrain at the floor: what today's world is.
    pub fn flat(grid_size: usize, cell_size: f32, floor_y: f32) -> Self {
        Self::from_heights(
            grid_size,
            cell_size,
            [0.0; 3],
            floor_y,
            vec![floor_y; (grid_size + 1) * (grid_size + 1)],
        )
        .unwrap_or_else(|e| unreachable!("a flat terrain is always valid: {e}"))
    }

    /// Parses the exporter's TOML and checks its hash against the contents.
    pub fn parse(text: &str) -> Result<Self, String> {
        let parsed: Parsed = toml::from_str(text).map_err(|e| e.to_string())?;
        let terrain = Self::from_heights(
            parsed.grid_size,
            parsed.cell_size,
            parsed.origin,
            parsed.floor_y,
            parsed.heights,
        )?;
        let stored = parsed.hash.trim().to_ascii_lowercase();
        if stored != terrain.hash_hex() {
            return Err(format!(
                "the hash ({stored}) does not match the heights ({}): terrain.toml was edited by hand; regenerate it with tools/export_terrain.gd",
                terrain.hash_hex()
            ));
        }
        Ok(terrain)
    }

    pub fn grid_size(&self) -> usize {
        self.grid_size
    }

    pub fn cell_size(&self) -> f32 {
        self.cell_size
    }

    pub fn floor_y(&self) -> f32 {
        self.floor_y
    }

    /// One line for the boot log: which ground this binary runs.
    pub fn describe(&self) -> String {
        let short: String = self.hash_hex().chars().take(12).collect();
        format!(
            "{}x{} cells of {} m, floor {}, hash {short}",
            self.grid_size, self.grid_size, self.cell_size, self.floor_y
        )
    }

    /// The canonical bytes the hash covers: the grid as a u32, then every
    /// float as its IEEE-754 bits, little-endian. The exporter builds the same
    /// bytes from the scene, so the two hashes agree exactly whatever either
    /// side's float formatting does.
    fn canonical_bytes(&self) -> Vec<u8> {
        let mut bytes = Vec::with_capacity(24 + 4 * self.heights.len());
        bytes.extend_from_slice(&(self.grid_size as u32).to_le_bytes());
        bytes.extend_from_slice(&self.cell_size.to_le_bytes());
        for v in [self.origin.x, self.origin.y, self.origin.z, self.floor_y] {
            bytes.extend_from_slice(&v.to_le_bytes());
        }
        for h in &self.heights {
            bytes.extend_from_slice(&h.to_le_bytes());
        }
        bytes
    }

    /// SHA-256 of the canonical bytes, lowercase hex. Not a tamper seal (the
    /// lockstep check is the guard): an identity for the boot line, and a
    /// refusal for a file someone edited by hand.
    pub fn hash_hex(&self) -> String {
        let digest = Sha256::digest(self.canonical_bytes());
        digest.iter().map(|b| format!("{b:02x}")).collect()
    }

    fn vertex(&self, ix: usize, iz: usize) -> f32 {
        self.heights[iz * (self.grid_size + 1) + ix]
    }

    fn half(&self) -> f32 {
        self.grid_size as f32 * self.cell_size * 0.5
    }

    /// World XZ to grid-local coordinates, where the first vertex is (0, 0)
    /// and the last is (extent, extent).
    fn local(&self, x: f32, z: f32) -> (f32, f32) {
        let half = self.half();
        (x - self.origin.x + half, z - self.origin.z + half)
    }

    /// The mesh's height at a point, or `None` outside its extent (and for a
    /// non-finite point). Inside, the plane through the three corners of the
    /// triangle the point is in, the client's own triangles: each cell is split
    /// on the diagonal from (x1, z0) to (x0, z1).
    pub fn mesh_height(&self, x: f32, z: f32) -> Option<f32> {
        let (lx, lz) = self.local(x, z);
        self.mesh_height_local(lx, lz)
    }

    fn extent(&self) -> f32 {
        self.grid_size as f32 * self.cell_size
    }

    /// The cell a grid-local point is in and where it sits inside it, or
    /// `None` outside the grid (and for a non-finite point). The one place
    /// that decides which triangle a point belongs to: the height and the
    /// slope are both read off it.
    fn cell(&self, lx: f32, lz: f32) -> Option<Cell> {
        let extent = self.extent();
        if !(lx >= 0.0 && lx <= extent && lz >= 0.0 && lz <= extent) {
            return None;
        }
        let fx = lx / self.cell_size;
        let fz = lz / self.cell_size;
        // The far edge belongs to the last cell.
        let cx = (fx.floor() as usize).min(self.grid_size - 1);
        let cz = (fz.floor() as usize).min(self.grid_size - 1);
        Some(Cell {
            u: fx - cx as f32,
            v: fz - cz as f32,
            h00: self.vertex(cx, cz),
            h10: self.vertex(cx + 1, cz),
            h01: self.vertex(cx, cz + 1),
            h11: self.vertex(cx + 1, cz + 1),
        })
    }

    fn mesh_height_local(&self, lx: f32, lz: f32) -> Option<f32> {
        let c = self.cell(lx, lz)?;
        let h = if c.lower() {
            c.h00 + c.u * (c.h10 - c.h00) + c.v * (c.h01 - c.h00)
        } else {
            c.h11 + (1.0 - c.u) * (c.h01 - c.h11) + (1.0 - c.v) * (c.h10 - c.h11)
        };
        Some(self.origin.y + h)
    }

    /// The ground under a point: the mesh where it is above the floor, the
    /// floor everywhere else (past the mesh's edge, in a valley the floor
    /// fills, and for a non-finite point).
    pub fn ground(&self, x: f32, z: f32) -> f32 {
        let (lx, lz) = self.local(x, z);
        self.ground_local(lx, lz)
    }

    fn ground_local(&self, lx: f32, lz: f32) -> f32 {
        self.mesh_height_local(lx, lz)
            .map_or(self.floor_y, |h| h.max(self.floor_y))
    }

    /// The ground for a point computed along a line: a coordinate that
    /// rounding left a hair outside the grid's edge is taken as on it. At the
    /// edge the ground drops from the mesh to the floor, so a sample meant for
    /// the edge (where a sight line crosses it) must read the mesh's side.
    fn ground_snapped(&self, x: f32, z: f32) -> f32 {
        let (lx, lz) = self.local(x, z);
        let extent = self.extent();
        let eps = self.cell_size * 1e-4;
        let snap = |v: f32| {
            if v < 0.0 && v > -eps {
                0.0
            } else if v > extent && v < extent + eps {
                extent
            } else {
                v
            }
        };
        self.ground_local(snap(lx), snap(lz))
    }

    /// The same XZ, standing on the ground.
    pub fn ground_vec(&self, pos: Vec3f) -> Vec3f {
        Vec3f {
            x: pos.x,
            y: self.ground(pos.x, pos.z),
            z: pos.z,
        }
    }

    /// Whether a player may step from `from` to `to` in one tick: the rise
    /// between the two grounds is at most the run times `tan(SLOPE_LIMIT_DEG)`,
    /// with `SLOPE_SLACK` of that for rounding, and a step that rises does not
    /// land on ground steeper than that. An angle, whatever the step's length,
    /// so a step too short to measure climbs nothing. Downhill is always
    /// allowed; a non-finite step never is.
    ///
    /// The second half is the face itself: a step's own grade on a face of
    /// gradient g is g times the cosine of its angle to straight uphill, so a
    /// diagonal climbs a 63 degree face at 55 and zigzags up any face at all.
    /// The client's body cannot (it slides off ground steeper than its floor
    /// limit whichever way it faces), so neither may a modified client.
    pub fn slope_ok(&self, from: Vec3f, to: Vec3f) -> bool {
        if !(from.x.is_finite() && from.z.is_finite() && to.x.is_finite() && to.z.is_finite()) {
            return false;
        }
        let rise = self.ground(to.x, to.z) - self.ground(from.x, from.z);
        if rise <= 0.0 {
            return true;
        }
        let limit = slope_limit_tan() * (1.0 + SLOPE_SLACK);
        let run = ((to.x - from.x).powi(2) + (to.z - from.z).powi(2)).sqrt();
        if rise > run * limit {
            return false;
        }
        let (gx, gz) = self.gradient(to.x, to.z);
        (gx * gx + gz * gz).sqrt() <= limit
    }

    /// The slope of the ground under a point, as (dh/dx, dh/dz): the plane of
    /// the triangle there; flat where the ground is the floor (past the mesh,
    /// or where the mesh dips under the floor) and for a non-finite point.
    pub fn gradient(&self, x: f32, z: f32) -> (f32, f32) {
        let (lx, lz) = self.local(x, z);
        let Some(c) = self.cell(lx, lz) else {
            return (0.0, 0.0);
        };
        let Some(h) = self.mesh_height_local(lx, lz) else {
            return (0.0, 0.0);
        };
        if h < self.floor_y {
            return (0.0, 0.0);
        }
        let (dx, dz) = if c.lower() {
            (c.h10 - c.h00, c.h01 - c.h00)
        } else {
            (c.h11 - c.h01, c.h11 - c.h10)
        };
        (dx / self.cell_size, dz / self.cell_size)
    }

    /// Move `(x, z)` down the ground's gradient until the ground there is at
    /// or below `y`, or until the ground is a floor (a gradient no steeper
    /// than `floor_tan`): a falling body that meets a face too steep to stand
    /// on slides down it, as the client's body slides off a wall, and lands
    /// on the first floor the slide reaches (`fall.rs`). Returns where the
    /// slide got to; a body still inside a face after the passes rests
    /// against it there.
    pub fn slide_clear(&self, x: f32, z: f32, y: f32, floor_tan: f32) -> (f32, f32) {
        let (mut x, mut z) = (x, z);
        // A face spans a cell or two; each pass clears the plane under the
        // point, and a crossing into the next cell gets another pass.
        for _ in 0..8 {
            let excess = self.ground(x, z) - y;
            if excess <= 0.0 {
                break;
            }
            let (gx, gz) = self.gradient(x, z);
            let g = (gx * gx + gz * gz).sqrt();
            if g <= floor_tan {
                break;
            }
            // Down the gradient the plane drops `g` a metre: a hair past the
            // distance that clears it.
            let d = excess / g * 1.001 + 1e-4;
            x -= d * gx / g;
            z -= d * gz / g;
        }
        (x, z)
    }

    /// One tick of a player's movement (`Step`): the horizontal step `(dx, dz)` from
    /// `from`, taken as the client's body takes it. Uphill the step shrinks
    /// exactly as `move_and_slide` shrinks it: the motion loses `sin²θ` of
    /// its part up the slope, so straight uphill it covers `cos²θ` of the
    /// flat distance (measured 2026-11-03 with the client's
    /// `tools/probe_ramp_speed.gd`: 3.75 m/s of 5 at 30°). Downhill and
    /// across it keeps its length, as the body does with its floor snap. A
    /// step whose rise breaks the slope rule is refused and the player stays.
    /// The result stands on the ground, a zero step included, so a teleport
    /// or a load never leaves a player in the air.
    pub fn player_step(&self, from: Vec3f, dx: f32, dz: f32) -> Step {
        let here = self.ground_vec(from);
        if !(dx.is_finite() && dz.is_finite()) || (dx == 0.0 && dz == 0.0) {
            // Nothing to take (the Move handler already dropped a non-finite
            // direction): not a slope refusal.
            return Step::taken(here);
        }
        let (gx, gz) = self.gradient(from.x, from.z);
        let up = dx * gx + dz * gz;
        let (dx, dz) = if up > 0.0 {
            // tan²θ, then sin²θ; the unit direction of steepest ascent and
            // the step's part along it.
            let s2 = gx * gx + gz * gz;
            let sin2 = s2 / (1.0 + s2);
            let g = s2.sqrt();
            let (ux, uz) = (gx / g, gz / g);
            let along = dx * ux + dz * uz;
            (dx - sin2 * along * ux, dz - sin2 * along * uz)
        } else {
            (dx, dz)
        };
        let to = Vec3f {
            x: from.x + dx,
            y: here.y,
            z: from.z + dz,
        };
        if !self.slope_ok(here, to) {
            return Step {
                pos: here,
                refused: true,
            };
        }
        Step::taken(self.ground_vec(to))
    }

    /// Whether `a` and `b`, each raised by `LOS_HEIGHT`, see each other over
    /// the terrain. Inside one triangle the ground is a plane and the sight
    /// line is a line, so their difference is linear there: the clearance is
    /// checked at both ends and wherever the line crosses a cell edge or a cell
    /// diagonal, and if it holds at all of those it holds between them (the
    /// floor keeps that true: the clearance over `max(mesh, floor)` is the
    /// smaller of two linear functions, lowest at an end). Exact for the
    /// client's collider, no step size, no thin ridge stepped over.
    pub fn can_see(&self, a: Vec3f, b: Vec3f) -> bool {
        let finite = |p: Vec3f| p.x.is_finite() && p.y.is_finite() && p.z.is_finite();
        if !(finite(a) && finite(b)) {
            return false;
        }
        let a = Vec3f {
            y: a.y + LOS_HEIGHT,
            ..a
        };
        let b = Vec3f {
            y: b.y + LOS_HEIGHT,
            ..b
        };
        self.min_clearance(a, b) >= -LOS_TOLERANCE
    }

    /// The lowest height of the segment `a` to `b` over the ground, found at
    /// the ends and at the crossings of every cell edge and cell diagonal
    /// (the only places a piecewise-linear difference can be lowest). Both
    /// ends finite.
    fn min_clearance(&self, a: Vec3f, b: Vec3f) -> f32 {
        let (ax, az) = self.local(a.x, a.z);
        let (bx, bz) = self.local(b.x, b.z);
        let mut ts: Vec<f32> = vec![0.0, 1.0];
        // The cell edges along x, along z, and the diagonals (lx + lz = k * cell).
        push_crossings(&mut ts, ax, bx, self.cell_size, self.grid_size);
        push_crossings(&mut ts, az, bz, self.cell_size, self.grid_size);
        push_crossings(
            &mut ts,
            ax + az,
            bx + bz,
            self.cell_size,
            2 * self.grid_size,
        );
        ts.into_iter()
            .map(|t| {
                let p = Vec3f {
                    x: a.x + (b.x - a.x) * t,
                    y: a.y + (b.y - a.y) * t,
                    z: a.z + (b.z - a.z) * t,
                };
                p.y - self.ground_snapped(p.x, p.z)
            })
            .fold(f32::INFINITY, f32::min)
    }
}

fn slope_limit_tan() -> f32 {
    SLOPE_LIMIT_DEG.to_radians().tan()
}

/// A point's cell: its fraction across the cell (`u` along x, `v` along z)
/// and the four corner heights, `h<x><z>`.
struct Cell {
    u: f32,
    v: f32,
    h00: f32,
    h10: f32,
    h01: f32,
    h11: f32,
}

impl Cell {
    /// On the lower triangle, the one with the (x0, z0) corner: the cell is
    /// split on the diagonal from (x1, z0) to (x0, z1), the client's own.
    fn lower(&self) -> bool {
        self.u + self.v <= 1.0
    }
}

/// The parameters in (0, 1) at which a coordinate moving from `s0` to `s1`
/// crosses the lines `k * cell` for `k` in `0..=kmax` (the grid's own lines;
/// past them the ground is the flat floor and needs no sample).
fn push_crossings(ts: &mut Vec<f32>, s0: f32, s1: f32, cell: f32, kmax: usize) {
    if s0 == s1 {
        return;
    }
    let (lo, hi) = if s0 < s1 { (s0, s1) } else { (s1, s0) };
    let k0 = ((lo / cell).ceil().max(0.0)) as usize;
    let k1 = (hi / cell).floor();
    if k1 < 0.0 {
        return;
    }
    let k1 = (k1 as usize).min(kmax);
    for k in k0..=k1 {
        let t = (k as f32 * cell - s0) / (s1 - s0);
        if t > 0.0 && t < 1.0 {
            ts.push(t);
        }
    }
}

/// The ground this binary was built with. Parsed once; a file that does not
/// parse or whose hash does not match stops the server at first use, which
/// `main` makes the boot line.
pub fn compiled() -> &'static Terrain {
    static TERRAIN: OnceLock<Terrain> = OnceLock::new();
    TERRAIN.get_or_init(|| {
        Terrain::parse(TERRAIN_TOML)
            .unwrap_or_else(|e| panic!("terrain.toml must be valid; fix the embedded file: {e}"))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use rand::{Rng, SeedableRng};

    fn at(x: f32, z: f32) -> Vec3f {
        Vec3f { x, y: 0.0, z }
    }

    /// A 4x4 grid of 2 m cells with pseudo-random heights in [0, 5).
    fn bumpy(seed: u64) -> Terrain {
        let mut rng = rand::rngs::StdRng::seed_from_u64(seed);
        let heights = (0..25).map(|_| rng.gen_range(0.0..5.0)).collect();
        Terrain::from_heights(4, 2.0, [0.0; 3], -10.0, heights).unwrap()
    }

    /// The client's own triangle list for a terrain (`TerrainMesh._rebuild`),
    /// as (x, y, z) corners in world space.
    fn client_triangles(t: &Terrain) -> Vec<[Vec3f; 3]> {
        let gs = t.grid_size;
        let half = t.half();
        let corner = |ix: usize, iz: usize| Vec3f {
            x: t.origin.x - half + ix as f32 * t.cell_size,
            y: t.origin.y + t.vertex(ix, iz),
            z: t.origin.z - half + iz as f32 * t.cell_size,
        };
        let mut tris = Vec::new();
        for z in 0..gs {
            for x in 0..gs {
                tris.push([corner(x, z), corner(x, z + 1), corner(x + 1, z)]);
                tris.push([corner(x + 1, z), corner(x, z + 1), corner(x + 1, z + 1)]);
            }
        }
        tris
    }

    /// A vertical ray down at (x, z) against the triangle list: the height of
    /// the plane of whichever triangle contains the point in XZ, by its
    /// barycentric weights (each corner's weight is the area of the triangle
    /// the point makes with the other two, over the whole).
    fn ray_down(tris: &[[Vec3f; 3]], x: f32, z: f32) -> Option<f32> {
        let area = |ax: f32, az: f32, bx: f32, bz: f32, cx: f32, cz: f32| {
            (bx - ax) * (cz - az) - (cx - ax) * (bz - az)
        };
        for [p0, p1, p2] in tris {
            let d = area(p0.x, p0.z, p1.x, p1.z, p2.x, p2.z);
            if d.abs() < 1e-9 {
                continue;
            }
            let w0 = area(x, z, p1.x, p1.z, p2.x, p2.z) / d;
            let w1 = area(p0.x, p0.z, x, z, p2.x, p2.z) / d;
            let w2 = 1.0 - w0 - w1;
            let eps = -1e-5;
            if w0 >= eps && w1 >= eps && w2 >= eps {
                return Some(w0 * p0.y + w1 * p1.y + w2 * p2.y);
            }
        }
        None
    }

    #[test]
    fn every_vertex_returns_its_height() {
        let t = bumpy(1);
        for iz in 0..=4 {
            for ix in 0..=4 {
                let x = -4.0 + ix as f32 * 2.0;
                let z = -4.0 + iz as f32 * 2.0;
                let want = t.vertex(ix, iz);
                assert!((t.ground(x, z) - want).abs() < 1e-5, "vertex ({ix}, {iz})");
            }
        }
    }

    #[test]
    fn inside_each_triangle_is_the_plane_through_its_corners() {
        // One cell, corners h00 = 0, h10 = 4, h01 = 2, h11 = 10, on a 1x1 grid
        // of 2 m cells centred on the origin: local (0, 0) is world (-1, -1).
        let t = Terrain::from_heights(1, 2.0, [0.0; 3], -100.0, vec![0.0, 4.0, 2.0, 10.0]).unwrap();
        // Lower triangle, u = 0.25, v = 0.25: 0 + 0.25 * 4 + 0.25 * 2 = 1.5.
        assert!((t.ground(-0.5, -0.5) - 1.5).abs() < 1e-5);
        // Upper triangle, u = 0.75, v = 0.75: 10 + 0.25 * (2 - 10) + 0.25 * (4 - 10) = 6.5.
        assert!((t.ground(0.5, 0.5) - 6.5).abs() < 1e-5);
        // On the diagonal, u = 0.5, v = 0.5: both planes give 3.0.
        assert!((t.ground(0.0, 0.0) - 3.0).abs() < 1e-5);
        // Bilinear would give 4.0 there; the client's collider does not.
        assert!((t.ground(0.0, 0.0) - 4.0).abs() > 0.5);
    }

    #[test]
    fn a_ray_down_the_clients_triangles_agrees_everywhere() {
        for seed in 0..4u64 {
            let t = bumpy(seed);
            let tris = client_triangles(&t);
            let mut rng = rand::rngs::StdRng::seed_from_u64(100 + seed);
            for _ in 0..2000 {
                let x = rng.gen_range(-4.0..4.0f32);
                let z = rng.gen_range(-4.0..4.0f32);
                let want = ray_down(&tris, x, z).expect("inside the grid");
                let got = t.mesh_height(x, z).expect("inside the grid");
                assert!(
                    (got - want).abs() < 1e-4,
                    "seed {seed} at ({x}, {z}): {got} vs {want}"
                );
            }
        }
    }

    #[test]
    fn the_edges_and_corners_are_inside() {
        let t = bumpy(2);
        assert!(t.mesh_height(-4.0, -4.0).is_some());
        assert!(t.mesh_height(4.0, 4.0).is_some());
        assert!(t.mesh_height(4.0, -4.0).is_some());
        assert!(t.mesh_height(0.0, 4.0).is_some());
        assert!((t.mesh_height(4.0, 4.0).unwrap() - t.vertex(4, 4)).abs() < 1e-5);
    }

    #[test]
    fn outside_the_extent_is_the_floor() {
        let t = bumpy(3);
        assert_eq!(t.mesh_height(4.001, 0.0), None);
        assert_eq!(t.ground(4.001, 0.0), -10.0);
        assert_eq!(t.ground(-5.0, -5.0), -10.0);
        assert_eq!(t.ground(0.0, 1000.0), -10.0);
    }

    #[test]
    fn a_valley_below_the_floor_is_the_floor() {
        let t = Terrain::from_heights(1, 2.0, [0.0; 3], 0.0, vec![-3.0, -3.0, -3.0, -3.0]).unwrap();
        assert_eq!(t.ground(0.0, 0.0), 0.0);
        assert_eq!(t.mesh_height(0.0, 0.0), Some(-3.0));
    }

    #[test]
    fn a_non_finite_point_is_the_floor() {
        let t = bumpy(4);
        assert_eq!(t.ground(f32::NAN, 0.0), -10.0);
        assert_eq!(t.ground(0.0, f32::INFINITY), -10.0);
        assert_eq!(t.ground(f32::NEG_INFINITY, f32::NAN), -10.0);
    }

    #[test]
    fn the_origin_moves_the_grid() {
        let t = Terrain::from_heights(1, 2.0, [100.0, 5.0, -50.0], -100.0, vec![1.0; 4]).unwrap();
        assert!((t.ground(100.0, -50.0) - 6.0).abs() < 1e-5);
        assert_eq!(t.mesh_height(0.0, 0.0), None);
    }

    #[test]
    fn the_compiled_terrain_parses_and_its_hash_matches() {
        let t = compiled();
        assert_eq!(t.grid_size(), 400);
        assert_eq!(t.cell_size(), 10.0);
        // The 4 km plate (the client's data/terrain_features.toml): the town
        // flat at the floor, the Wraith Gate plateau's top, the Greywood
        // ridge's wall, the pass through it (its floor at the level of the
        // ground at its mouths), and the mountains at the plate's edge, well
        // over 160 m everywhere.
        assert_eq!(t.ground(0.0, 0.0), t.floor_y());
        assert_eq!(t.mesh_height(12.5, 5.0), Some(0.0));
        assert_eq!(t.mesh_height(1700.0, 0.0), Some(60.0));
        assert!(t.ground(-700.0, 900.0) >= 62.0, "the ridge's crest");
        assert_eq!(t.ground(0.0, 900.0), 6.0, "the pass's floor");
        for (x, z) in [
            (-2000.0, 0.0),
            (2000.0, 0.0),
            (0.0, -2000.0),
            (0.0, 2000.0),
            (2000.0, 2000.0),
        ] {
            let h = t
                .mesh_height(x, z)
                .expect("the plate's edge is on the mesh");
            assert!(h >= 160.0, "({x}, {z}): {h}");
        }
    }

    #[test]
    fn a_hand_edited_file_is_refused() {
        let text = TERRAIN_TOML.replacen("0.0,", "1.5,", 1);
        assert_ne!(text, TERRAIN_TOML, "the fixture must change a height");
        let err = Terrain::parse(&text).expect_err("a changed height with the old hash");
        assert!(err.contains("hash"), "{err}");
        let wrong_count = TERRAIN_TOML.replacen("0.0,", "", 1);
        assert!(Terrain::parse(&wrong_count).is_err());
    }

    #[test]
    fn the_hash_is_the_sha256_of_the_canonical_bytes() {
        // A 1x1 grid of 2 m cells at the origin, flat at 0: the u32 1, the
        // float 2.0, then eight zero floats.
        let t = Terrain::flat(1, 2.0, 0.0);
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&1u32.to_le_bytes());
        bytes.extend_from_slice(&2.0f32.to_le_bytes());
        bytes.extend_from_slice(&[0u8; 32]);
        let want: String = Sha256::digest(&bytes)
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        assert_eq!(t.hash_hex(), want);
        assert_eq!(t.hash_hex().len(), 64);
        // The exporter writes the same bytes: proven by the compiled file's
        // hash matching (`the_compiled_terrain_parses_and_its_hash_matches`).
    }

    // Slopes: a 1x1 grid of 20 m cells, the far edge raised to make a ramp.
    fn ramp(rise: f32) -> Terrain {
        Terrain::from_heights(1, 20.0, [0.0; 3], -100.0, vec![0.0, rise, 0.0, rise]).unwrap()
    }

    #[test]
    fn a_step_under_the_slope_limit_is_taken_and_one_over_it_refused() {
        let run = 0.25;
        let taken = ramp(20.0 * 59f32.to_radians().tan());
        assert!(taken.slope_ok(at(0.0, 0.0), at(run, 0.0)));
        let refused = ramp(20.0 * 61f32.to_radians().tan());
        assert!(!refused.slope_ok(at(0.0, 0.0), at(run, 0.0)));
        // The same faces, downhill: always allowed.
        assert!(refused.slope_ok(at(run, 0.0), at(0.0, 0.0)));
        // Across the slope: no rise, allowed.
        assert!(refused.slope_ok(at(0.0, 0.0), at(0.0, run)));
    }

    #[test]
    fn a_steep_face_is_refused_at_every_speed() {
        let cliff = ramp(20.0 * 70f32.to_radians().tan());
        for run in [0.000_025, 0.001, 0.01, 0.25, 0.41, 2.0, 10.0] {
            assert!(!cliff.slope_ok(at(0.0, 0.0), at(run, 0.0)), "run {run}");
        }
        // The client's own limit, 55°, at full speed: taken, with room.
        let steep_but_fair = ramp(20.0 * 55f32.to_radians().tan());
        assert!(steep_but_fair.slope_ok(at(0.0, 0.0), at(0.41, 0.0)));
    }

    /// The review's find (2026-11-04): a fixed allowance per step let a forged
    /// client climb any face with steps too short to measure. Ten thousand
    /// micro-steps (a direction of length 1e-4, 25 µm each) up a 70° face go
    /// nowhere.
    #[test]
    fn micro_steps_climb_no_face() {
        let cliff = ramp(20.0 * 70f32.to_radians().tan());
        let start = cliff.ground_vec(at(0.0, 0.0));
        let mut p = start;
        for _ in 0..10_000 {
            let step = cliff.player_step(p, 0.000_025, 0.0);
            assert!(step.refused, "every micro-step is a refusal: {step:?}");
            p = step.pos;
        }
        assert_eq!((p.x, p.y, p.z), (start.x, start.y, start.z), "{p:?}");
    }

    /// A step at an angle to straight uphill climbs a face at less than the
    /// face's own slope: on this 63 degree face a 45 degree diagonal climbs
    /// at 55, and the slope rule judged the step alone. A walker zigzagging
    /// from the flat ground at its foot never gets onto it.
    #[test]
    fn a_diagonal_climbs_no_face() {
        // Two 10 m cells along x: flat from x = -10 to 0, then a face rising
        // 20 m over the cell from x = 0 to 10 (63.4 degrees).
        let face = Terrain::from_heights(
            2,
            10.0,
            [0.0; 3],
            -100.0,
            vec![0.0, 0.0, 20.0, 0.0, 0.0, 20.0, 0.0, 0.0, 20.0],
        )
        .unwrap();
        let on_face = face.ground_vec(at(5.0, 0.0));
        let d = 0.25 / 2f32.sqrt();
        let diagonal = Vec3f {
            x: on_face.x + d,
            y: on_face.y,
            z: on_face.z + d,
        };
        assert!(
            !face.slope_ok(on_face, diagonal),
            "a 45 degree diagonal up the face"
        );
        // Along the face's contour: no rise, still a step (the body then
        // leaves the ground it cannot stand on, fall.rs).
        assert!(face.slope_ok(on_face, at(5.0, 0.25)));
        // Down it: always.
        assert!(face.slope_ok(on_face, at(4.75, 0.0)));
        let mut p = face.ground_vec(at(-2.0, -9.0));
        for i in 0..200 {
            // Zigzag: north-east, then south-east.
            let dz = if (i / 20) % 2 == 0 { d } else { -d };
            p = face.player_step(p, d, dz).pos;
            assert!(p.y < 0.01, "step {i}: the walker is {p:?}, up the face");
        }
    }

    #[test]
    fn the_gradient_is_the_triangles_slope_and_flat_off_the_mesh() {
        // Rising 10 m over the 20 m cell along +x.
        let t = ramp(10.0);
        let (gx, gz) = t.gradient(1.0, 1.0);
        assert!((gx - 0.5).abs() < 1e-6 && gz.abs() < 1e-6, "{gx} {gz}");
        assert_eq!(t.gradient(100.0, 0.0), (0.0, 0.0));
        assert_eq!(t.gradient(f32::NAN, 0.0), (0.0, 0.0));
        // Where the mesh is under the floor, the ground is the flat floor.
        let valley =
            Terrain::from_heights(1, 2.0, [0.0; 3], 0.0, vec![-3.0, -1.0, -3.0, -1.0]).unwrap();
        assert_eq!(valley.gradient(0.0, 0.0), (0.0, 0.0));
    }

    #[test]
    fn a_step_up_a_slope_covers_cos_squared_of_the_flat_distance() {
        // The 30° ramp rising along +x: the client's measured 3.75 m/s of 5.
        let t = ramp(20.0 * 30f32.to_radians().tan());
        let from = t.ground_vec(at(2.0, 1.0));
        let step = t.player_step(from, 0.25, 0.0);
        assert!(!step.refused, "a 30° slope is climbable: {step:?}");
        let to = step.pos;
        assert!((to.x - from.x - 0.25 * 0.75).abs() < 1e-4, "{to:?}");
        assert!((to.z - from.z).abs() < 1e-6);
        assert!((to.y - t.ground(to.x, to.z)).abs() < 1e-5);
        assert!(to.y > from.y);
        // Across the slope and downhill: the full step.
        let across = t.player_step(from, 0.0, 0.25).pos;
        assert!((across.z - from.z - 0.25).abs() < 1e-5);
        let down = t.player_step(t.ground_vec(at(5.0, 1.0)), -0.25, 0.0).pos;
        assert!((down.x - 4.75).abs() < 1e-5, "{down:?}");
        // Diagonally: only the part up the slope shrinks.
        let diag = t.player_step(from, 0.2, 0.2).pos;
        assert!((diag.x - from.x - 0.2 * 0.75).abs() < 1e-4, "{diag:?}");
        assert!((diag.z - from.z - 0.2).abs() < 1e-5);
    }

    #[test]
    fn a_falling_body_slides_clear_of_a_face_and_not_of_flat_ground() {
        // A 70° face rising along +x from x = -10 (0 m) to 10 (55 m): at x = 0
        // the ground is 27.5 m. A body at 20 m there is inside the face and
        // slides down-gradient (west) to where the face is at 20 m,
        // x = 20 / tan(70°) - 10 = -2.72.
        let t55 = 55f32.to_radians().tan();
        let cliff = ramp(20.0 * 70f32.to_radians().tan());
        let (x, z) = cliff.slide_clear(0.0, 0.0, 20.0, t55);
        assert!((x + 2.72).abs() < 0.02 && z.abs() < 1e-6, "({x}, {z})");
        assert!(cliff.ground(x, z) <= 20.0);
        // Already clear: unmoved.
        assert_eq!(cliff.slide_clear(-8.0, 0.0, 20.0, t55), (-8.0, 0.0));
        // Flat ground above the point is a floor: the slide stops there (the
        // body lands on it) rather than sliding anywhere.
        assert_eq!(compiled().slide_clear(3.0, 4.0, -1.0, t55), (3.0, 4.0));
        // A face with a floor at its foot: a body under the floor's level
        // slides down the face and stops on the floor, where it lands.
        let foot = Terrain::from_heights(
            2,
            10.0,
            [0.0; 3],
            0.0,
            vec![0.0, 0.0, 30.0, 0.0, 0.0, 30.0, 0.0, 0.0, 30.0],
        )
        .unwrap();
        let (x, z) = foot.slide_clear(5.0, 0.0, -1.0, t55);
        assert!(x < 0.0 && x > -1.0 && z.abs() < 1e-6, "({x}, {z})");
        assert_eq!(foot.gradient(x, z), (0.0, 0.0));
    }

    #[test]
    fn a_step_into_a_steep_face_is_refused_and_a_zero_step_still_pins() {
        let cliff = ramp(20.0 * 70f32.to_radians().tan());
        let from = cliff.ground_vec(at(1.0, 0.0));
        let stayed = cliff.player_step(from, 0.25, 0.0);
        assert!(stayed.refused, "{stayed:?}");
        assert_eq!((stayed.pos.x, stayed.pos.z), (from.x, from.z));
        let t = compiled();
        // A non-finite step is dropped, not refused: the handler never lets
        // one this far, and the counter is for the slope rule alone.
        let p = t.player_step(at(3.0, 4.0), f32::NAN, 0.0);
        assert!(!p.refused, "{p:?}");
        assert_eq!((p.pos.x, p.pos.y, p.pos.z), (3.0, 0.0, 4.0));
        let floating = Vec3f {
            x: 3.0,
            y: 9.0,
            z: 4.0,
        };
        let pinned = t.player_step(floating, 0.0, 0.0);
        assert!(!pinned.refused);
        assert_eq!(pinned.pos.y, 0.0);
    }

    #[test]
    fn step_toward_steps_by_the_horizontal_delta() {
        let here = at(0.0, 0.0);
        let above = Vec3f {
            x: 0.0,
            y: 50.0,
            z: 1.0,
        };
        // Directly below its target it arrives; the old 3D rule never did.
        let p = here.step_toward(above, 2.0);
        assert_eq!((p.x, p.y, p.z), (0.0, 0.0, 1.0));
        // The step's length is horizontal, whatever the height difference.
        let far = Vec3f {
            x: 10.0,
            y: 50.0,
            z: 0.0,
        };
        let p = here.step_toward(far, 0.3);
        assert!((p.x - 0.3).abs() < 1e-6 && p.z == 0.0 && p.y == 0.0);
    }

    #[test]
    fn the_flat_world_allows_every_step_and_a_non_finite_one_none() {
        let t = Terrain::flat(20, 20.0, 0.0);
        assert!(t.slope_ok(at(0.0, 0.0), at(0.41, 0.0)));
        assert!(t.slope_ok(at(-199.0, 50.0), at(-199.4, 50.3)));
        assert!(!t.slope_ok(at(0.0, 0.0), at(f32::NAN, 0.0)));
        assert!(!t.slope_ok(at(f32::INFINITY, 0.0), at(0.0, 0.0)));
    }

    // Sight: a 3x1 strip would do, but a 4x4 grid of 10 m cells with a ridge
    // down the middle row of vertices (x index 2) keeps the geometry plain.
    fn ridge(height: f32) -> Terrain {
        let mut heights = vec![0.0; 25];
        for iz in 0..=4 {
            heights[iz * 5 + 2] = height;
        }
        Terrain::from_heights(4, 10.0, [0.0; 3], 0.0, heights).unwrap()
    }

    #[test]
    fn the_flat_world_sees_everywhere() {
        let t = Terrain::flat(20, 20.0, 0.0);
        assert!(t.can_see(at(-150.0, -150.0), at(150.0, 150.0)));
        assert!(t.can_see(at(0.0, 0.0), at(30.0, 0.0)));
        assert!(t.can_see(at(-250.0, 0.0), at(250.0, 0.0)));
    }

    #[test]
    fn a_ridge_between_two_points_blocks_and_lowered_does_not() {
        // Feet at x = -15 and x = 15, eyes at 1.2 m; the ridge at x = 0.
        let a = at(-15.0, 3.0);
        let b = at(15.0, -2.0);
        assert!(!ridge(4.0).can_see(a, b));
        assert!(ridge(1.0).can_see(a, b));
        // A ridge of exactly eye height over flat feet is grazed: inside the
        // tolerance, sight.
        assert!(ridge(LOS_HEIGHT).can_see(at(-15.0, 0.0), at(15.0, 0.0)));
        assert!(!ridge(LOS_HEIGHT + 0.05).can_see(at(-15.0, 0.0), at(15.0, 0.0)));
    }

    #[test]
    fn sight_is_symmetric() {
        let t = ridge(2.0);
        let mut rng = rand::rngs::StdRng::seed_from_u64(7);
        for _ in 0..500 {
            let a = at(rng.gen_range(-20.0..20.0), rng.gen_range(-20.0..20.0));
            let b = at(rng.gen_range(-20.0..20.0), rng.gen_range(-20.0..20.0));
            let a = t.ground_vec(a);
            let b = t.ground_vec(b);
            assert_eq!(t.can_see(a, b), t.can_see(b, a), "{a:?} {b:?}");
        }
    }

    #[test]
    fn a_target_on_the_crest_is_seen_from_both_feet() {
        let t = ridge(6.0);
        let crest = t.ground_vec(at(0.0, 0.0));
        assert!((crest.y - 6.0).abs() < 1e-5);
        assert!(t.can_see(at(-15.0, 0.0), crest));
        assert!(t.can_see(at(15.0, 0.0), crest));
        assert!(!t.can_see(at(-15.0, 0.0), at(15.0, 0.0)));
    }

    #[test]
    fn a_non_finite_end_has_no_sight() {
        let t = compiled();
        assert!(!t.can_see(at(f32::NAN, 0.0), at(1.0, 0.0)));
        assert!(!t.can_see(
            at(0.0, 0.0),
            Vec3f {
                x: 1.0,
                y: f32::INFINITY,
                z: 0.0
            }
        ));
    }

    /// The proof that the edge-crossing shortcut is exact: along random
    /// segments over random ground, the lowest clearance the crossings find is
    /// never above what a 1 cm march finds (the march can only miss a kink,
    /// never find a lower point), and never more than the march's step times
    /// the steepest slope below it (what a kink between two samples can hide).
    #[test]
    fn a_brute_force_march_agrees_with_the_edge_crossings() {
        // Heights in [0, 5) on 2 m cells over a floor at -10: a ground slope
        // under 2.5 along an axis and 3.6 along a diagonal, a sight line under
        // 2; at the grid's edge the ground drops to the floor, so the lowest
        // clearance can sit right at the edge with the march's nearest sample
        // a whole step inside it. A 5 mm step at a combined 5.6 hides 2.8 cm;
        // 4 cm leaves margin.
        let step = 0.005;
        let hidden_by_a_step = 0.04;
        let mut exact_below_the_march = 0;
        for seed in 0..6u64 {
            let t = bumpy(seed);
            let mut rng = rand::rngs::StdRng::seed_from_u64(200 + seed);
            for _ in 0..600 {
                let a = t.ground_vec(at(rng.gen_range(-5.0..5.0), rng.gen_range(-5.0..5.0)));
                let b = t.ground_vec(at(rng.gen_range(-5.0..5.0), rng.gen_range(-5.0..5.0)));
                let ea = Vec3f {
                    y: a.y + LOS_HEIGHT,
                    ..a
                };
                let eb = Vec3f {
                    y: b.y + LOS_HEIGHT,
                    ..b
                };
                let len = a.distance_to(b).max(step);
                let steps = (len / step).ceil() as usize;
                let mut marched = f32::INFINITY;
                for i in 0..=steps {
                    let s = i as f32 / steps as f32;
                    let x = ea.x + (eb.x - ea.x) * s;
                    let z = ea.z + (eb.z - ea.z) * s;
                    let y = ea.y + (eb.y - ea.y) * s;
                    marched = marched.min(y - t.ground(x, z));
                }
                let exact = t.min_clearance(ea, eb);
                assert!(
                    exact <= marched + 1e-4,
                    "seed {seed}: {a:?} to {b:?}: the crossings found {exact}, the march found lower, {marched}"
                );
                assert!(
                    marched - exact <= hidden_by_a_step,
                    "seed {seed}: {a:?} to {b:?}: the crossings found {exact}, the march only {marched}"
                );
                if exact < marched - 1e-4 {
                    exact_below_the_march += 1;
                }
                // And the answer itself agrees wherever the line is not grazed.
                if (marched + LOS_TOLERANCE).abs() > hidden_by_a_step {
                    assert_eq!(t.can_see(a, b), marched >= -LOS_TOLERANCE, "{a:?} to {b:?}");
                }
            }
        }
        // Kinks between samples are expected on bumpy ground; none at all would
        // mean the march was too fine to be a check.
        assert!(exact_below_the_march > 0);
    }

    #[test]
    fn an_invalid_terrain_is_refused_with_a_reason() {
        assert!(Terrain::from_heights(0, 2.0, [0.0; 3], 0.0, vec![0.0]).is_err());
        assert!(Terrain::from_heights(1, 0.0, [0.0; 3], 0.0, vec![0.0; 4]).is_err());
        assert!(Terrain::from_heights(1, 2.0, [0.0; 3], 0.0, vec![0.0; 3]).is_err());
        assert!(
            Terrain::from_heights(1, 2.0, [0.0; 3], 0.0, vec![0.0, f32::NAN, 0.0, 0.0]).is_err()
        );
        assert!(Terrain::from_heights(1, 2.0, [0.0; 3], f32::NAN, vec![0.0; 4]).is_err());
    }
}
