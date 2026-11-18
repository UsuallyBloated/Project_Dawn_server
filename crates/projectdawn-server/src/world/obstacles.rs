//! What stands on the ground (`docs/design/obstacle_collision.md`, 2026-11-17):
//! the zone's trees, rocks, walls and set pieces as the server knows them. The
//! client's scenery is baked by its `tools/export_obstacles.gd` into
//! `data/obstacles.bin` and compiled in, so the binary carries the same
//! obstacles the client draws, and the client loads the same bytes
//! (`scripts/zone/zone_obstacles.gd`, the same rule in GDScript).
//!
//! The data is a list of prisms. A prism is one solid of the scenery seen
//! from above: a convex outline in world x and z, counter-clockwise, with the
//! height of its lowest and highest point, and two flags:
//!
//! * `FLAG_WALK`: it stands in a body's way. For walking a prism is a column
//!   of any height: the server's bodies have an x, a z and the ground under
//!   them, so nothing is stood on or jumped over.
//! * `FLAG_SIGHT`: it blocks a line of sight where the line passes through the
//!   outline (drawn in by its sight scale, since a cone or a ball is narrower
//!   on average than its widest) between its base and its top.
//!
//! The walking rule (`resolve`): a step is taken in pieces no longer than
//! `SUBSTEP`; after each, a body whose centre is inside a walk outline grown
//! by the body's radius is moved out the shortest way, which keeps the part of
//! the step along the surface and drops the part into it: a slide. A piece
//! that cannot be settled ends the step where the body last stood free. The
//! server applies it to every step it integrates, so a modified client walks
//! into the server's walls; the client applies the same rule to its own body
//! only so the picture agrees.
//!
//! Every answer is total: a non-finite input moves nothing and sees nothing.

use super::connection::Vec3f;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::sync::OnceLock;

const OBSTACLES_BIN: &[u8] = include_bytes!("../../data/obstacles.bin");

/// The file: a 12-byte header (magic, version, prism count, little-endian
/// u32), then 32-bit floats (per prism: flags, base, top, sight scale, corner
/// count, then x and z of each corner), then the SHA-256 of everything before
/// it, so a hand edit or a short file is refused.
pub const MAGIC: u32 = 0x424F_4450; // "PDOB"
pub const VERSION: u32 = 1;
const HEADER_BYTES: usize = 12;
const HASH_BYTES: usize = 32;
pub const FLAG_WALK: u8 = 1;
pub const FLAG_SIGHT: u8 = 2;

/// A body's radius for the walking rule: the client's capsule
/// (`scenes/player.tscn`). Mobs and pets use the same one, never a larger, so
/// there is no gap a player fits through and a mob does not.
pub const BODY_RADIUS: f32 = 0.4;
/// The lookup grid's cell, metres.
pub const CELL: f32 = 8.0;
/// A step is taken in pieces no longer than this, so no speed carries a body
/// across a thin outline between two looks.
pub const SUBSTEP: f32 = 0.1;
/// No piece of a step moves a body further than this many times its own
/// length (plus `FREE_SLACK` twice over). A slide never lengthens a step, and
/// rounding a corner lengthens it by a few percent at most; a piece that
/// would move the body further was being squeezed between solids, and ends
/// the step instead. With pieces of `SUBSTEP` this makes crossing a solid
/// impossible whatever the scenery: to stand free on the far side of even a
/// sheet of paper, a body's centre has to travel twice its radius.
pub const PIECE_STRETCH: f32 = 1.5;
/// How many times over the nearby prisms push a piece of a step (a corner
/// between two solids takes more than one).
pub const PASSES: usize = 4;
/// The same, for a body being put somewhere (`clear_of`).
pub const CLEAR_PASSES: usize = 16;
/// `clear_of`'s search when the push-out does not settle: rings this far
/// apart, this many of them, this many points on each.
pub const CLEAR_RING_STEP: f32 = 0.25;
pub const CLEAR_RINGS: usize = 160;
/// The directions tried on each ring, written out (and not computed with a
/// cosine) so every build of the server and the client's copy of the rule try
/// exactly the same points. Sixteen ways round the compass, to two places:
/// they are directions to look in, and need not be exactly a metre long.
pub const CLEAR_SPOKES: [(f32, f32); 16] = [
    (1.0, 0.0),
    (0.92, 0.38),
    (0.7, 0.7),
    (0.38, 0.92),
    (0.0, 1.0),
    (-0.38, 0.92),
    (-0.7, 0.7),
    (-0.92, 0.38),
    (-1.0, 0.0),
    (-0.92, -0.38),
    (-0.7, -0.7),
    (-0.38, -0.92),
    (0.0, -1.0),
    (0.38, -0.92),
    (0.7, -0.7),
    (0.92, -0.38),
];
/// A place a body is put must be one it can walk away from (`can_leave`): a
/// walk of `ESCAPE_REACH` in some direction, sliding as any walk does, has to
/// end at least `ESCAPE_NEED` away. Two rocks and a trunk can close a gap a
/// body fits in and never gets out of; nothing is ever put in one. Closed
/// ground wider than `ESCAPE_NEED` passes this (a body can walk about in it),
/// so the zone must hold none: `obstacles_content.rs` fails on one.
pub const ESCAPE_REACH: f32 = 4.0;
pub const ESCAPE_NEED: f32 = 3.0;
/// A body pushed out to exactly its radius counts as free: this much overlap
/// is allowed when asking whether a point is free.
pub const FREE_SLACK: f32 = 0.001;
/// A piece of a step that ends further than this from where it aimed was
/// deflected.
pub const DEFLECT_EPS: f32 = 0.0001;
/// Past this many grid cells under a sight line, every prism is tested.
const SIGHT_CELL_LIMIT: i64 = 1024;

/// A point on the ground plane.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct P2 {
    pub x: f32,
    pub z: f32,
}

impl P2 {
    pub fn new(x: f32, z: f32) -> Self {
        Self { x, z }
    }

    pub fn of(v: Vec3f) -> Self {
        Self { x: v.x, z: v.z }
    }

    fn is_finite(self) -> bool {
        self.x.is_finite() && self.z.is_finite()
    }

    fn sub(self, o: P2) -> P2 {
        P2::new(self.x - o.x, self.z - o.z)
    }

    fn dot(self, o: P2) -> f32 {
        self.x * o.x + self.z * o.z
    }

    fn dist2(self, o: P2) -> f32 {
        let d = self.sub(o);
        d.dot(d)
    }

    /// The distance to another point.
    pub fn dist_to(self, o: P2) -> f32 {
        self.dist2(o).sqrt()
    }
}

/// One solid, as the file holds it.
#[derive(Debug, Clone, PartialEq)]
pub struct Prism {
    pub flags: u8,
    pub base: f32,
    pub top: f32,
    pub sight_scale: f32,
    /// The outline, counter-clockwise (x to the right, z up).
    pub corners: Vec<P2>,
}

impl Prism {
    /// A box standing on axis-aligned ground: both flags, full sight.
    pub fn rect(x0: f32, z0: f32, x1: f32, z1: f32, base: f32, top: f32) -> Self {
        Prism {
            flags: FLAG_WALK | FLAG_SIGHT,
            base,
            top,
            sight_scale: 1.0,
            corners: vec![
                P2::new(x0, z0),
                P2::new(x1, z0),
                P2::new(x1, z1),
                P2::new(x0, z1),
            ],
        }
    }

    /// A column of `sides` sides round a centre: both flags, full sight.
    pub fn round(cx: f32, cz: f32, radius: f32, sides: usize, base: f32, top: f32) -> Self {
        let corners = (0..sides)
            .map(|k| {
                let a = std::f32::consts::TAU * k as f32 / sides as f32;
                P2::new(cx + a.cos() * radius, cz + a.sin() * radius)
            })
            .collect();
        Prism {
            flags: FLAG_WALK | FLAG_SIGHT,
            base,
            top,
            sight_scale: 1.0,
            corners,
        }
    }
}

/// Where a step ends (`Obstacles::resolve`).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Resolved {
    pub x: f32,
    pub z: f32,
    /// Something turned the step aside or stopped it.
    pub deflected: bool,
}

#[derive(Debug, Clone, Default)]
pub struct Obstacles {
    flags: Vec<u8>,
    base: Vec<f32>,
    top: Vec<f32>,
    scale: Vec<f32>,
    /// Where prism i's corners start in `pts`, and how many it has.
    first: Vec<usize>,
    count: Vec<usize>,
    pts: Vec<P2>,
    /// The corners drawn in towards the outline's middle by the prism's sight
    /// scale: what a line of sight is tested against.
    sight_pts: Vec<P2>,
    walk_cells: HashMap<(i32, i32), Vec<u32>>,
    sight_cells: HashMap<(i32, i32), Vec<u32>>,
    sight_all: Vec<u32>,
    walk_count: usize,
    hash_hex: String,
}

fn cell(v: f32) -> i32 {
    (v / CELL).floor() as i32
}

impl Obstacles {
    /// A world with nothing standing on it: what every test that brings no
    /// obstacles of its own runs on.
    pub fn empty() -> Self {
        Self::default()
    }

    /// Obstacles from a list of prisms, each checked (`add`).
    pub fn from_prisms(prisms: &[Prism]) -> Result<Self, String> {
        let mut out = Self::default();
        for (i, p) in prisms.iter().enumerate() {
            out.add(p).map_err(|e| format!("prism {i}: {e}"))?;
        }
        Ok(out)
    }

    /// The file's bytes for a list of prisms (the exporter's format), for
    /// tests that need a file.
    pub fn encode(prisms: &[Prism]) -> Vec<u8> {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&MAGIC.to_le_bytes());
        bytes.extend_from_slice(&VERSION.to_le_bytes());
        bytes.extend_from_slice(&(prisms.len() as u32).to_le_bytes());
        for p in prisms {
            let head = [
                f32::from(p.flags),
                p.base,
                p.top,
                p.sight_scale,
                p.corners.len() as f32,
            ];
            for v in head {
                bytes.extend_from_slice(&v.to_le_bytes());
            }
            for c in &p.corners {
                bytes.extend_from_slice(&c.x.to_le_bytes());
                bytes.extend_from_slice(&c.z.to_le_bytes());
            }
        }
        let digest = Sha256::digest(&bytes);
        bytes.extend_from_slice(&digest);
        bytes
    }

    /// The obstacles a file holds. Refused with a reason: a short or ragged
    /// file, the wrong magic or version, a hash that does not match (a hand
    /// edit), a prism that is not a finite convex counter-clockwise outline.
    pub fn parse(bytes: &[u8]) -> Result<Self, String> {
        if bytes.len() < HEADER_BYTES + HASH_BYTES
            || !(bytes.len() - HEADER_BYTES - HASH_BYTES).is_multiple_of(4)
        {
            return Err(format!("{} bytes is not a whole file", bytes.len()));
        }
        let word = |at: usize| -> u32 {
            let mut b = [0u8; 4];
            b.copy_from_slice(&bytes[at..at + 4]);
            u32::from_le_bytes(b)
        };
        if word(0) != MAGIC || word(4) != VERSION {
            return Err("wrong magic or version".into());
        }
        let body_end = bytes.len() - HASH_BYTES;
        let digest = Sha256::digest(&bytes[..body_end]);
        if digest.as_slice() != &bytes[body_end..] {
            return Err(
                "the file does not match its own hash (edited by hand, or cut short)".into(),
            );
        }
        let floats: Vec<f32> = bytes[HEADER_BYTES..body_end]
            .chunks_exact(4)
            .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
            .collect();
        let mut out = Self::default();
        let mut i = 0;
        while i < floats.len() {
            if i + 5 > floats.len() {
                return Err("the file ends inside a prism".into());
            }
            let n = floats[i + 4];
            if !((3.0..=4096.0).contains(&n) && n.fract() == 0.0) {
                return Err(format!("prism {} has {n} corners", out.len()));
            }
            let n = n as usize;
            if i + 5 + 2 * n > floats.len() {
                return Err("the file ends inside a prism".into());
            }
            let flags = floats[i];
            if !(flags == 1.0 || flags == 2.0 || flags == 3.0) {
                return Err(format!("prism {} has flags {flags}", out.len()));
            }
            let prism = Prism {
                flags: flags as u8,
                base: floats[i + 1],
                top: floats[i + 2],
                sight_scale: floats[i + 3],
                corners: floats[i + 5..i + 5 + 2 * n]
                    .chunks_exact(2)
                    .map(|c| P2::new(c[0], c[1]))
                    .collect(),
            };
            let index = out.len();
            out.add(&prism).map_err(|e| format!("prism {index}: {e}"))?;
            i += 5 + 2 * n;
        }
        if out.len() != word(8) as usize {
            return Err(format!(
                "the file declares {} prisms and holds {}",
                word(8),
                out.len()
            ));
        }
        out.hash_hex = digest.iter().take(6).map(|b| format!("{b:02x}")).collect();
        Ok(out)
    }

    /// Add one prism, checked: finite numbers, a top above its base, a sight
    /// scale in (0, 1], at least three corners that turn left all the way
    /// round (convex, counter-clockwise; a millimetre's rounding is forgiven).
    pub fn add(&mut self, p: &Prism) -> Result<(), String> {
        if p.flags == 0 || p.flags & !(FLAG_WALK | FLAG_SIGHT) != 0 {
            return Err(format!("flags {}", p.flags));
        }
        if !(p.base.is_finite() && p.top.is_finite() && p.top >= p.base) {
            return Err(format!("base {} and top {}", p.base, p.top));
        }
        if !(p.sight_scale > 0.0 && p.sight_scale <= 1.0) {
            return Err(format!("sight scale {}", p.sight_scale));
        }
        let n = p.corners.len();
        if n < 3 {
            return Err(format!("{n} corners"));
        }
        if p.corners.iter().any(|c| !c.is_finite()) {
            return Err("a corner is not finite".into());
        }
        let mut twice_area = 0.0f64;
        for k in 0..n {
            let a = p.corners[k];
            let b = p.corners[(k + 1) % n];
            let c = p.corners[(k + 2) % n];
            twice_area += f64::from(a.x) * f64::from(b.z) - f64::from(b.x) * f64::from(a.z);
            let turn = (f64::from(b.x) - f64::from(a.x)) * (f64::from(c.z) - f64::from(a.z))
                - (f64::from(b.z) - f64::from(a.z)) * (f64::from(c.x) - f64::from(a.x));
            let reach = f64::from(a.dist2(b).max(b.dist2(c)).sqrt());
            if turn < -0.002 * reach {
                return Err("the outline is not convex and counter-clockwise".into());
            }
        }
        if twice_area <= 0.0 {
            return Err("the outline has no area or runs clockwise".into());
        }

        let index = self.flags.len() as u32;
        self.flags.push(p.flags);
        self.base.push(p.base);
        self.top.push(p.top);
        self.scale.push(p.sight_scale);
        self.first.push(self.pts.len());
        self.count.push(n);
        let (mut lo, mut hi) = (
            P2::new(f32::INFINITY, f32::INFINITY),
            P2::new(f32::NEG_INFINITY, f32::NEG_INFINITY),
        );
        let mut mid = P2::new(0.0, 0.0);
        for c in &p.corners {
            lo = P2::new(lo.x.min(c.x), lo.z.min(c.z));
            hi = P2::new(hi.x.max(c.x), hi.z.max(c.z));
            mid = P2::new(mid.x + c.x, mid.z + c.z);
        }
        mid = P2::new(mid.x / n as f32, mid.z / n as f32);
        for c in &p.corners {
            self.pts.push(*c);
            self.sight_pts.push(P2::new(
                mid.x + (c.x - mid.x) * p.sight_scale,
                mid.z + (c.z - mid.z) * p.sight_scale,
            ));
        }
        for cz in cell(lo.z)..=cell(hi.z) {
            for cx in cell(lo.x)..=cell(hi.x) {
                if p.flags & FLAG_WALK != 0 {
                    self.walk_cells.entry((cx, cz)).or_default().push(index);
                }
                if p.flags & FLAG_SIGHT != 0 {
                    self.sight_cells.entry((cx, cz)).or_default().push(index);
                }
            }
        }
        if p.flags & FLAG_WALK != 0 {
            self.walk_count += 1;
        }
        if p.flags & FLAG_SIGHT != 0 {
            self.sight_all.push(index);
        }
        Ok(())
    }

    pub fn len(&self) -> usize {
        self.flags.len()
    }

    pub fn is_empty(&self) -> bool {
        self.flags.is_empty()
    }

    /// How many prisms stand in a body's way.
    pub fn walk_count(&self) -> usize {
        self.walk_count
    }

    /// The first twelve hex digits of the file's SHA-256 (empty for obstacles
    /// that came from no file).
    pub fn hash_hex(&self) -> &str {
        &self.hash_hex
    }

    /// One line for the boot log.
    pub fn describe(&self) -> String {
        format!(
            "{} prisms ({} walk, {} sight), hash {}",
            self.len(),
            self.walk_count,
            self.sight_all.len(),
            if self.hash_hex.is_empty() {
                "none"
            } else {
                &self.hash_hex
            }
        )
    }

    /// Prism i as it was added.
    pub fn prism(&self, i: usize) -> Option<Prism> {
        let first = *self.first.get(i)?;
        let n = *self.count.get(i)?;
        Some(Prism {
            flags: self.flags[i],
            base: self.base[i],
            top: self.top[i],
            sight_scale: self.scale[i],
            corners: self.pts[first..first + n].to_vec(),
        })
    }

    fn corners(&self, i: u32) -> &[P2] {
        let i = i as usize;
        &self.pts[self.first[i]..self.first[i] + self.count[i]]
    }

    fn sight_corners(&self, i: u32) -> &[P2] {
        let i = i as usize;
        &self.sight_pts[self.first[i]..self.first[i] + self.count[i]]
    }

    fn gather(
        cells: &HashMap<(i32, i32), Vec<u32>>,
        x0: f32,
        z0: f32,
        x1: f32,
        z1: f32,
    ) -> Vec<u32> {
        let mut found: Vec<u32> = Vec::new();
        for cz in cell(z0)..=cell(z1) {
            for cx in cell(x0)..=cell(x1) {
                if let Some(held) = cells.get(&(cx, cz)) {
                    found.extend_from_slice(held);
                }
            }
        }
        found.sort_unstable();
        found.dedup();
        found
    }

    /// The walk prisms whose outline comes within `reach` of `p`, lowest
    /// index first.
    fn walk_near(&self, p: P2, reach: f32) -> Vec<u32> {
        if self.walk_count == 0 {
            return Vec::new();
        }
        Self::gather(
            &self.walk_cells,
            p.x - reach,
            p.z - reach,
            p.x + reach,
            p.z + reach,
        )
    }

    /// Where `p` ends up after prism `i` has had its say: `p` itself when the
    /// body (a circle of `radius` round p) is clear of the outline, otherwise
    /// p moved out the shortest way until it only touches.
    fn push_from(&self, i: u32, p: P2, radius: f32) -> P2 {
        let corners = self.corners(i);
        let mut inside = true;
        // For a centre inside the outline: the edge it is least far behind.
        let mut best_d = f32::NEG_INFINITY;
        let mut best_n = P2::new(0.0, 0.0);
        // For a centre outside it: the nearest point of the outline.
        let mut near_d2 = f32::INFINITY;
        let mut near = P2::new(0.0, 0.0);
        let mut a = corners[corners.len() - 1];
        for &b in corners {
            let e = b.sub(a);
            let length = e.dot(e).sqrt();
            if length > 0.0 {
                // The outline runs counter-clockwise, so this points out of it.
                let normal = P2::new(e.z / length, -e.x / length);
                let d = p.sub(a).dot(normal);
                if d > 0.0 {
                    inside = false;
                }
                if d > best_d {
                    best_d = d;
                    best_n = normal;
                }
                let t = (p.sub(a).dot(e) / (length * length)).clamp(0.0, 1.0);
                let q = P2::new(a.x + e.x * t, a.z + e.z * t);
                let d2 = p.dist2(q);
                if d2 < near_d2 {
                    near_d2 = d2;
                    near = q;
                }
            }
            a = b;
        }
        if inside {
            let push = radius - best_d;
            return P2::new(p.x + best_n.x * push, p.z + best_n.z * push);
        }
        if near_d2 >= radius * radius {
            return p;
        }
        let dist = near_d2.sqrt();
        if dist < 0.000_001 {
            return P2::new(p.x + best_n.x * radius, p.z + best_n.z * radius);
        }
        let k = radius / dist;
        P2::new(near.x + (p.x - near.x) * k, near.z + (p.z - near.z) * k)
    }

    fn blocks(&self, i: u32, p: P2, radius: f32) -> bool {
        let corners = self.corners(i);
        let mut inside = true;
        let mut near_d2 = f32::INFINITY;
        let mut a = corners[corners.len() - 1];
        for &b in corners {
            let e = b.sub(a);
            let length2 = e.dot(e);
            if length2 > 0.0 {
                if p.sub(a).dot(P2::new(e.z, -e.x)) > 0.0 {
                    inside = false;
                }
                let t = (p.sub(a).dot(e) / length2).clamp(0.0, 1.0);
                near_d2 = near_d2.min(p.dist2(P2::new(a.x + e.x * t, a.z + e.z * t)));
            }
            a = b;
        }
        inside || near_d2 < radius * radius
    }

    /// Whether a body of `radius` at `p` touches no walk prism (`FREE_SLACK`
    /// of overlap allowed: a body pushed out to exactly its radius is free).
    /// A point that is not finite is never free.
    pub fn is_free(&self, p: P2, radius: f32) -> bool {
        if !p.is_finite() {
            return false;
        }
        self.walk_near(p, radius)
            .into_iter()
            .all(|i| !self.blocks(i, p, radius - FREE_SLACK))
    }

    /// `p` after every nearby prism has pushed it, up to `passes` times over.
    fn settle(&self, mut p: P2, radius: f32, passes: usize) -> P2 {
        for _ in 0..passes {
            let mut moved = false;
            for i in self.walk_near(p, radius) {
                let q = self.push_from(i, p, radius);
                if q != p {
                    moved = true;
                    p = q;
                }
            }
            if !moved {
                break;
            }
        }
        p
    }

    /// One step of a body of `radius` from `from` towards `to`: where it
    /// ends, and whether anything turned it aside or stopped it. With nothing
    /// near the step, the step is taken whole and `to` comes back untouched.
    pub fn resolve(&self, from: P2, to: P2, radius: f32) -> Resolved {
        let stay = |deflected| Resolved {
            x: from.x,
            z: from.z,
            deflected,
        };
        if !(from.is_finite() && to.is_finite() && radius.is_finite()) {
            return stay(false);
        }
        if self.walk_count == 0
            || Self::gather(
                &self.walk_cells,
                from.x.min(to.x) - radius,
                from.z.min(to.z) - radius,
                from.x.max(to.x) + radius,
                from.z.max(to.z) + radius,
            )
            .is_empty()
        {
            return Resolved {
                x: to.x,
                z: to.z,
                deflected: false,
            };
        }
        if !self.is_free(from, radius) {
            // Standing inside something (a scenery change since the body was
            // put here): the step is spent getting out.
            let out = self.clear_of(from, radius);
            return Resolved {
                x: out.x,
                z: out.z,
                deflected: true,
            };
        }
        let whole = to.sub(from);
        let length = whole.dot(whole).sqrt();
        if length == 0.0 {
            return stay(false);
        }
        let pieces = (length / SUBSTEP).floor() as usize + 1;
        let piece = P2::new(whole.x / pieces as f32, whole.z / pieces as f32);
        let furthest = length / pieces as f32 * PIECE_STRETCH + 2.0 * FREE_SLACK;
        let mut p = from;
        let mut deflected = false;
        for _ in 0..pieces {
            let want = P2::new(p.x + piece.x, p.z + piece.z);
            let got = self.settle(want, radius, PASSES);
            if !self.is_free(got, radius) || got.dist2(p) > furthest * furthest {
                return Resolved {
                    x: p.x,
                    z: p.z,
                    deflected: true,
                };
            }
            if got.dist2(want) > DEFLECT_EPS * DEFLECT_EPS {
                deflected = true;
            }
            p = got;
        }
        Resolved {
            x: p.x,
            z: p.z,
            deflected,
        }
    }

    /// Whether a body standing free at `p` can walk away from it: a walk of
    /// `ESCAPE_REACH` in one of the ring directions ends `ESCAPE_NEED` or more
    /// from `p`. False in a gap closed all round by solids.
    pub fn can_leave(&self, p: P2, radius: f32) -> bool {
        if self.walk_near(p, radius + ESCAPE_REACH).is_empty() {
            return true;
        }
        CLEAR_SPOKES.iter().any(|(sx, sz)| {
            let to = P2::new(p.x + sx * ESCAPE_REACH, p.z + sz * ESCAPE_REACH);
            let r = self.resolve(p, to, radius);
            P2::new(r.x, r.z).dist2(p) >= ESCAPE_NEED * ESCAPE_NEED
        })
    }

    /// Whether a body may be put at `p`: free of every solid, and able to
    /// walk away.
    pub fn stands(&self, p: P2, radius: f32) -> bool {
        self.is_free(p, radius) && self.can_leave(p, radius)
    }

    /// The nearest place to `p` a body of `radius` can be put (`stands`): `p`
    /// itself when it will do, else where the push-out settles, else the
    /// first such point on widening rings round it. Everything that puts a
    /// body somewhere without walking it there goes through this (a login, a
    /// teleport, a port, a summon, a spawn, a landing), so nothing ever stands
    /// inside a solid or in a gap it cannot leave.
    pub fn clear_of(&self, p: P2, radius: f32) -> P2 {
        if !p.is_finite() || self.stands(p, radius) {
            return p;
        }
        let q = self.settle(p, radius, CLEAR_PASSES);
        if self.stands(q, radius) {
            return q;
        }
        for ring in 1..=CLEAR_RINGS {
            let reach = ring as f32 * CLEAR_RING_STEP;
            for (sx, sz) in CLEAR_SPOKES {
                let c = P2::new(p.x + sx * reach, p.z + sz * reach);
                if self.stands(c, radius) {
                    return c;
                }
            }
        }
        p
    }

    /// Where a body put at `pos` stands: `pos` itself when a body can be put
    /// there, else the nearest point that is clear of every solid, that a
    /// body can walk away from, and that is ground a body stands on (a floor,
    /// not a face: moved out of a rock at a cliff's foot, a body must not be
    /// stood on the cliff). On the ground there. Every placement goes through
    /// this (a login, a respawn, a port, a resurrection summon, a spawn, a
    /// corpse at load). A point that is already good is never moved, whatever
    /// its ground: that is the terrain's business, as it was.
    pub fn place(&self, terrain: &super::terrain::Terrain, pos: Vec3f) -> Vec3f {
        let at = P2::of(pos);
        let good = |p: P2| {
            self.stands(p, BODY_RADIUS) && super::fall::is_floor(terrain.gradient(p.x, p.z))
        };
        let out = if !at.is_finite() || self.stands(at, BODY_RADIUS) {
            at
        } else {
            let settled = self.settle(at, BODY_RADIUS, CLEAR_PASSES);
            let mut found = good(settled).then_some(settled);
            'rings: for ring in 1..=CLEAR_RINGS {
                if found.is_some() {
                    break;
                }
                let reach = ring as f32 * CLEAR_RING_STEP;
                for (sx, sz) in CLEAR_SPOKES {
                    let c = P2::new(at.x + sx * reach, at.z + sz * reach);
                    if good(c) {
                        found = Some(c);
                        break 'rings;
                    }
                }
            }
            // No floor in reach (it never happens on the zone's ground): the
            // nearest clear point, as `clear_of` gives it.
            found.unwrap_or_else(|| self.clear_of(at, BODY_RADIUS))
        };
        terrain.ground_vec(Vec3f {
            x: out.x,
            y: pos.y,
            z: out.z,
        })
    }

    /// Whether a body of `radius` can run straight from `a` to `b` touching
    /// no walk prism: the segment keeps the body's radius from every outline.
    /// What a mob walking along a wall asks before it turns for its goal
    /// again (`Entity::step_on`). Not finite: never clear.
    pub fn run_clear(&self, a: P2, b: P2, radius: f32) -> bool {
        if !(a.is_finite() && b.is_finite()) {
            return false;
        }
        if self.walk_count == 0 {
            return true;
        }
        let reach = radius - FREE_SLACK;
        Self::gather(
            &self.walk_cells,
            a.x.min(b.x) - radius,
            a.z.min(b.z) - radius,
            a.x.max(b.x) + radius,
            a.z.max(b.z) + radius,
        )
        .into_iter()
        .all(|i| self.segment_gap(i, a, b) >= reach)
    }

    /// How near the segment `a` to `b` comes to prism `i`'s outline: 0 when
    /// it crosses it or an end is inside.
    fn segment_gap(&self, i: u32, a: P2, b: P2) -> f32 {
        let corners = self.corners(i);
        let inside = |q: P2| {
            let mut p = corners[corners.len() - 1];
            for &c in corners {
                let e = c.sub(p);
                if q.sub(p).dot(P2::new(e.z, -e.x)) > 0.0 {
                    return false;
                }
                p = c;
            }
            true
        };
        if inside(a) || inside(b) {
            return 0.0;
        }
        // Which side of the line p to q the point r is on.
        let side = |p: P2, q: P2, r: P2| (q.x - p.x) * (r.z - p.z) - (q.z - p.z) * (r.x - p.x);
        let to_segment = |r: P2, p: P2, q: P2| {
            let e = q.sub(p);
            let len2 = e.dot(e);
            let t = if len2 > 0.0 {
                (r.sub(p).dot(e) / len2).clamp(0.0, 1.0)
            } else {
                0.0
            };
            r.dist_to(P2::new(p.x + e.x * t, p.z + e.z * t))
        };
        let mut best = f32::INFINITY;
        let mut p = corners[corners.len() - 1];
        for &q in corners {
            let crosses =
                side(a, b, p) * side(a, b, q) < 0.0 && side(p, q, a) * side(p, q, b) < 0.0;
            if crosses {
                return 0.0;
            }
            best = best
                .min(to_segment(a, p, q))
                .min(to_segment(b, p, q))
                .min(to_segment(p, a, b))
                .min(to_segment(q, a, b));
            p = q;
        }
        best
    }

    /// Whether a sight prism stands between `a` and `b` (world points already
    /// at eye height): the line passes through an outline between its base
    /// and its top. A point that is not finite sees nothing.
    pub fn sight_blocked(&self, a: Vec3f, b: Vec3f) -> bool {
        let finite = |p: Vec3f| p.x.is_finite() && p.y.is_finite() && p.z.is_finite();
        if !(finite(a) && finite(b)) {
            return true;
        }
        if self.sight_all.is_empty() {
            return false;
        }
        let (x0, x1) = (a.x.min(b.x), a.x.max(b.x));
        let (z0, z1) = (a.z.min(b.z), a.z.max(b.z));
        let cells = (i64::from(cell(x1)) - i64::from(cell(x0)) + 1)
            * (i64::from(cell(z1)) - i64::from(cell(z0)) + 1);
        if cells > SIGHT_CELL_LIMIT {
            return self.sight_all.iter().any(|&i| self.line_meets(i, a, b));
        }
        Self::gather(&self.sight_cells, x0, z0, x1, z1)
            .into_iter()
            .any(|i| self.line_meets(i, a, b))
    }

    fn line_meets(&self, i: u32, a: Vec3f, b: Vec3f) -> bool {
        let corners = self.sight_corners(i);
        let (mut t0, mut t1) = (0.0f32, 1.0f32);
        let from = P2::of(a);
        let along = P2::new(b.x - a.x, b.z - a.z);
        let mut p = corners[corners.len() - 1];
        for &q in corners {
            let e = q.sub(p);
            let normal = P2::new(e.z, -e.x);
            let behind = from.sub(p).dot(normal);
            let rate = along.dot(normal);
            if rate.abs() < 0.000_000_000_001 {
                if behind > 0.0 {
                    return false;
                }
            } else {
                let t = -behind / rate;
                if rate < 0.0 {
                    t0 = t0.max(t);
                } else {
                    t1 = t1.min(t);
                }
                if t0 > t1 {
                    return false;
                }
            }
            p = q;
        }
        let i = i as usize;
        let (base, top) = (self.base[i], self.top[i]);
        // A line that begins or ends inside this prism is not blocked by it:
        // a body under a tree on a slope has its eye inside the crown, and
        // sees out as it is seen. (No body is ever inside a prism that stands
        // in its way; this is crowns, roofs and lintels.) Every other prism
        // on the line still blocks.
        if (t0 == 0.0 && a.y > base && a.y < top) || (t1 == 1.0 && b.y > base && b.y < top) {
            return false;
        }
        let y0 = a.y + (b.y - a.y) * t0;
        let y1 = a.y + (b.y - a.y) * t1;
        y0.max(y1) > base && y0.min(y1) < top
    }
}

/// The obstacles this binary was built with. Parsed once; a file that does
/// not parse or whose hash does not match stops the server at first use,
/// which the world's start makes the boot line.
pub fn compiled() -> &'static std::sync::Arc<Obstacles> {
    static OBSTACLES: OnceLock<std::sync::Arc<Obstacles>> = OnceLock::new();
    OBSTACLES.get_or_init(|| {
        std::sync::Arc::new(
            Obstacles::parse(OBSTACLES_BIN).unwrap_or_else(|e| {
                panic!("obstacles.bin must be valid; fix the embedded file: {e}")
            }),
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use rand::{Rng, SeedableRng};

    fn p(x: f32, z: f32) -> P2 {
        P2::new(x, z)
    }

    /// A 10 m square house at the origin's east and a tree to its north.
    fn yard() -> Obstacles {
        Obstacles::from_prisms(&[
            Prism::rect(5.0, -5.0, 15.0, 5.0, 0.0, 4.0),
            Prism::round(0.0, 12.0, 0.3, 6, 0.0, 8.0),
        ])
        .unwrap()
    }

    fn dist_to_rect(q: P2, x0: f32, z0: f32, x1: f32, z1: f32) -> f32 {
        let dx = (x0 - q.x).max(0.0).max(q.x - x1);
        let dz = (z0 - q.z).max(0.0).max(q.z - z1);
        (dx * dx + dz * dz).sqrt()
    }

    #[test]
    fn nothing_near_a_step_leaves_it_untouched() {
        let o = yard();
        let r = o.resolve(p(-50.0, -50.0), p(-49.7, -50.1), BODY_RADIUS);
        assert_eq!((r.x, r.z, r.deflected), (-49.7, -50.1, false));
        let empty = Obstacles::empty();
        let r = empty.resolve(p(6.0, 0.0), p(6.3, 0.0), BODY_RADIUS);
        assert_eq!((r.x, r.z, r.deflected), (6.3, 0.0, false));
    }

    #[test]
    fn a_step_into_a_wall_stops_at_it() {
        let o = yard();
        // Straight at the west face from 1 m out, in 0.25 m steps.
        let mut at = p(4.0, 0.0);
        for _ in 0..20 {
            let r = o.resolve(at, p(at.x + 0.25, at.z), BODY_RADIUS);
            at = p(r.x, r.z);
        }
        assert!((at.x - (5.0 - BODY_RADIUS)).abs() < 1e-3, "rests at {at:?}");
        assert!(at.z.abs() < 1e-4);
        assert!(o.is_free(at, BODY_RADIUS));
    }

    #[test]
    fn a_step_at_an_angle_slides_along_the_wall() {
        let o = yard();
        // Pressed against the west face, heading north-east: the part into
        // the wall is lost, the part along it is kept whole.
        let from = p(5.0 - BODY_RADIUS, 0.0);
        let r = o.resolve(from, p(from.x + 0.2, from.z + 0.2), BODY_RADIUS);
        assert!(r.deflected);
        assert!((r.x - from.x).abs() < 1e-4, "held at the face, got {}", r.x);
        assert!(
            (r.z - 0.2).abs() < 1e-4,
            "kept its way along it, got {}",
            r.z
        );
    }

    #[test]
    fn a_body_slides_round_a_trunk() {
        let o = yard();
        // Walk north a little west of the trunk's middle (past the end of
        // its flat south face): it is turned aside and comes out past the
        // trunk, never inside its grown outline.
        let mut at = p(-0.2, 10.0);
        let mut deflected = false;
        for _ in 0..40 {
            let r = o.resolve(at, p(at.x, at.z + 0.25), BODY_RADIUS);
            deflected |= r.deflected;
            at = p(r.x, r.z);
            assert!(o.is_free(at, BODY_RADIUS), "inside the trunk at {at:?}");
        }
        assert!(deflected);
        assert!(at.z > 13.0, "got past the trunk, stopped at {at:?}");
        assert!(at.x < -0.3, "went round its west side, at {at:?}");
    }

    #[test]
    fn a_body_run_square_into_a_flat_face_stops() {
        // The same trunk met dead on its flat south face: nothing along the
        // face to keep, so the body rests against it until it steers. The
        // boundary is the solid's own shape, flat faces included.
        let o = yard();
        let mut at = p(-0.05, 10.0);
        for _ in 0..40 {
            let r = o.resolve(at, p(at.x, at.z + 0.25), BODY_RADIUS);
            at = p(r.x, r.z);
        }
        let face = 12.0 - 0.3 * (std::f32::consts::PI / 3.0).sin();
        assert!(
            (at.z - (face - BODY_RADIUS)).abs() < 2e-3,
            "rests at {at:?}"
        );
        assert!((at.x + 0.05).abs() < 1e-4);
    }

    #[test]
    fn the_largest_step_crosses_nothing() {
        // A fence rail 7 cm thick, and a step far longer than a tick allows:
        // the body ends on the side it started.
        let o = Obstacles::from_prisms(&[Prism::rect(-20.0, 0.0, 20.0, 0.07, 0.4, 0.5)]).unwrap();
        for step in [0.25f32, 0.4125, 1.0, 3.0, 12.5] {
            let r = o.resolve(p(0.3, -1.0), p(0.3, -1.0 + step), BODY_RADIUS);
            assert!(r.z < 0.0, "a {step} m step ended at z {}", r.z);
            assert!(o.is_free(p(r.x, r.z), BODY_RADIUS));
        }
    }

    #[test]
    fn a_body_squeezed_between_solids_is_never_squirted_across_one() {
        // A thin wall with a pillar hard against it on the near side: however
        // a body is driven into the notch between them, it never ends on the
        // wall's far side.
        let o = Obstacles::from_prisms(&[
            Prism::rect(-10.0, 0.0, 10.0, 0.05, 0.0, 3.0),
            Prism::round(0.0, -0.45, 0.4, 5, 0.0, 3.0),
            Prism::round(1.1, -0.3, 0.25, 4, 0.0, 3.0),
        ])
        .unwrap();
        let mut rng = rand::rngs::StdRng::seed_from_u64(7);
        for _ in 0..400 {
            let mut at = p(rng.gen_range(-3.0..4.0), rng.gen_range(-3.0..-1.4));
            let aim = p(rng.gen_range(-0.6..1.6), rng.gen_range(0.2..2.0));
            for _ in 0..60 {
                let d = aim.sub(at);
                let len = d.dot(d).sqrt().max(1e-6);
                let step: f32 = rng.gen_range(0.05..0.4125);
                let r = o.resolve(
                    at,
                    p(at.x + d.x / len * step, at.z + d.z / len * step),
                    BODY_RADIUS,
                );
                at = p(r.x, r.z);
                assert!(at.z < 0.0, "crossed the wall, at {at:?}");
                assert!(o.is_free(at, BODY_RADIUS));
            }
        }
    }

    #[test]
    fn a_corner_between_two_walls_holds() {
        // Two walls meeting in an inside corner: a body run into the corner
        // settles there, free, and never passes either wall.
        let o = Obstacles::from_prisms(&[
            Prism::rect(0.0, 0.0, 10.0, 1.0, 0.0, 3.0),
            Prism::rect(-1.0, -10.0, 0.0, 1.0, 0.0, 3.0),
        ])
        .unwrap();
        let mut at = p(3.0, -3.0);
        for _ in 0..60 {
            let r = o.resolve(at, p(at.x - 0.2, at.z + 0.2), BODY_RADIUS);
            at = p(r.x, r.z);
            assert!(o.is_free(at, BODY_RADIUS), "not free at {at:?}");
        }
        assert!((at.x - BODY_RADIUS).abs() < 2e-3 && (at.z + BODY_RADIUS).abs() < 2e-3);
    }

    #[test]
    fn a_body_inside_a_solid_is_put_out_at_the_nearest_edge() {
        let o = yard();
        // 1 m inside the house's west face: out through that face, not the far one.
        let out = o.clear_of(p(6.0, 0.0), BODY_RADIUS);
        assert!((out.x - (5.0 - BODY_RADIUS)).abs() < 1e-3 && out.z.abs() < 1e-4);
        // A step from inside is spent getting out, whatever it aimed at.
        let r = o.resolve(p(6.0, 0.0), p(9.0, 0.0), BODY_RADIUS);
        assert!(r.deflected);
        assert!((r.x - (5.0 - BODY_RADIUS)).abs() < 1e-3);
        // A free point is its own answer.
        assert_eq!(o.clear_of(p(-3.0, -3.0), BODY_RADIUS), p(-3.0, -3.0));
    }

    #[test]
    fn a_body_wedged_between_solids_is_found_a_free_place() {
        // A cross of two long thin walls: the push-out from its middle
        // ping-pongs, and the ring search finds open ground.
        let o = Obstacles::from_prisms(&[
            Prism::rect(-6.0, -0.1, 6.0, 0.1, 0.0, 2.0),
            Prism::rect(-0.1, -6.0, 0.1, 6.0, 0.0, 2.0),
        ])
        .unwrap();
        let out = o.clear_of(p(0.0, 0.0), BODY_RADIUS);
        assert!(o.is_free(out, BODY_RADIUS), "still inside at {out:?}");
        assert!(out.dist2(p(0.0, 0.0)).sqrt() < 2.0);
    }

    #[test]
    fn a_body_is_never_put_in_a_gap_it_cannot_leave() {
        // Four pillars round a gap just wide enough to stand in: free, and
        // closed all round. A body put there, or pushed out of a pillar
        // towards it, is stood outside the ring instead.
        let ring = [
            Prism::rect(-1.5, -1.5, -0.45, 1.5, 0.0, 3.0),
            Prism::rect(0.45, -1.5, 1.5, 1.5, 0.0, 3.0),
            Prism::rect(-1.5, -1.5, 1.5, -0.45, 0.0, 3.0),
            Prism::rect(-1.5, 0.45, 1.5, 1.5, 0.0, 3.0),
        ];
        let o = Obstacles::from_prisms(&ring).unwrap();
        let middle = p(0.0, 0.0);
        assert!(o.is_free(middle, BODY_RADIUS), "the gap holds a body");
        assert!(!o.can_leave(middle, BODY_RADIUS), "and lets none out");
        let put = o.clear_of(middle, BODY_RADIUS);
        assert!(o.stands(put, BODY_RADIUS), "put at {put:?}");
        assert!(put.x.abs() > 1.5 || put.z.abs() > 1.5, "outside the ring");
        // Open ground and a spot beside a single wall are left alone.
        assert!(o.can_leave(p(20.0, 20.0), BODY_RADIUS));
        let beside = p(1.5 + BODY_RADIUS, 0.0);
        assert_eq!(o.clear_of(beside, BODY_RADIUS), beside);
    }

    #[test]
    fn nothing_non_finite_moves_or_sees() {
        let o = yard();
        let r = o.resolve(p(1.0, 1.0), p(f32::NAN, 1.0), BODY_RADIUS);
        assert_eq!((r.x, r.z, r.deflected), (1.0, 1.0, false));
        let r = o.resolve(p(f32::INFINITY, 1.0), p(1.0, 1.0), BODY_RADIUS);
        assert!(r.x.is_infinite() && !r.deflected);
        assert!(!o.is_free(p(f32::NAN, 0.0), BODY_RADIUS));
        let at = |x: f32, y: f32, z: f32| Vec3f { x, y, z };
        assert!(o.sight_blocked(at(f32::NAN, 1.0, 0.0), at(1.0, 1.0, 0.0)));
    }

    #[test]
    fn random_walks_never_end_inside_anything() {
        // A wood of trunks and walls, bodies driven through it at every
        // heading with steps up to the fastest a player takes: every position
        // is free, and no step moves a body further than it asked to go.
        let mut rng = rand::rngs::StdRng::seed_from_u64(20261117);
        let mut prisms = Vec::new();
        for _ in 0..120 {
            let (x, z) = (rng.gen_range(-40.0..40.0), rng.gen_range(-40.0..40.0f32));
            if rng.gen_bool(0.7) {
                prisms.push(Prism::round(x, z, rng.gen_range(0.2..1.5), 6, 0.0, 6.0));
            } else {
                let (w, d) = (rng.gen_range(0.1..6.0), rng.gen_range(0.1..6.0f32));
                prisms.push(Prism::rect(x, z, x + w, z + d, 0.0, 4.0));
            }
        }
        let o = Obstacles::from_prisms(&prisms).unwrap();
        for _ in 0..200 {
            let start = p(rng.gen_range(-45.0..45.0), rng.gen_range(-45.0..45.0));
            let mut at = o.clear_of(start, BODY_RADIUS);
            assert!(o.is_free(at, BODY_RADIUS));
            let heading: f32 = rng.gen_range(0.0..std::f32::consts::TAU);
            for _ in 0..80 {
                let step: f32 = rng.gen_range(0.05..0.4125);
                let to = p(at.x + heading.cos() * step, at.z + heading.sin() * step);
                let r = o.resolve(at, to, BODY_RADIUS);
                let got = p(r.x, r.z);
                assert!(o.is_free(got, BODY_RADIUS), "{at:?} to {to:?} ended inside");
                // Never further than the step's pieces allow between them.
                let pieces = (step / SUBSTEP).floor() + 1.0;
                assert!(
                    got.dist2(at).sqrt() <= step * PIECE_STRETCH + pieces * 2.0 * FREE_SLACK + 1e-4,
                    "{at:?} to {to:?} moved {} for a {step} step",
                    got.dist2(at).sqrt()
                );
                at = got;
            }
        }
    }

    #[test]
    fn the_same_step_gives_the_same_answer() {
        let o = yard();
        let a = o.resolve(p(4.3, -4.9), p(4.7, -4.6), BODY_RADIUS);
        let b = o.resolve(p(4.3, -4.9), p(4.7, -4.6), BODY_RADIUS);
        assert_eq!(a, b);
    }

    #[test]
    fn a_line_through_a_solid_is_blocked_and_one_over_it_is_not() {
        let o = yard();
        let at = |x: f32, y: f32, z: f32| Vec3f { x, y, z };
        // Through the house at eye height.
        assert!(o.sight_blocked(at(0.0, 1.2, 0.0), at(20.0, 1.2, 0.0)));
        // The same line from either end.
        assert!(o.sight_blocked(at(20.0, 1.2, 0.0), at(0.0, 1.2, 0.0)));
        // Past its corner.
        assert!(!o.sight_blocked(at(0.0, 1.2, 6.0), at(20.0, 1.2, 6.0)));
        // Over its roof (the house is 4 m tall).
        assert!(!o.sight_blocked(at(0.0, 5.0, 0.0), at(20.0, 5.0, 0.0)));
        // Down from above onto ground beyond it: the line is over the roof
        // where it crosses the outline.
        assert!(!o.sight_blocked(at(0.0, 30.0, 0.0), at(25.0, 1.2, 0.0)));
        // Down from above into the yard behind it, steeply enough to hit the wall.
        assert!(o.sight_blocked(at(0.0, 6.0, 0.0), at(16.0, 1.2, 0.0)));
        // Stopping short of it.
        assert!(!o.sight_blocked(at(0.0, 1.2, 0.0), at(4.0, 1.2, 0.0)));
        // A point looking at itself beside it.
        assert!(!o.sight_blocked(at(4.0, 1.2, 0.0), at(4.0, 1.2, 0.0)));
    }

    /// A body on a slope beside a tree can have its eye inside the tree's
    /// crown (a sight-only prism). It sees out, and is seen; the crown still
    /// blocks a line that passes clean through it, and a wall beyond still
    /// blocks. Before, such a body could see nothing and nothing could see it.
    #[test]
    fn a_line_from_inside_a_crown_sees_out_and_other_solids_still_block() {
        let crown = Prism {
            flags: FLAG_SIGHT,
            sight_scale: 1.0,
            ..Prism::round(0.0, 0.0, 2.0, 8, 1.4, 9.0)
        };
        let o =
            Obstacles::from_prisms(&[crown, Prism::rect(10.0, -5.0, 11.0, 5.0, 0.0, 6.0)]).unwrap();
        let at = |x: f32, y: f32, z: f32| Vec3f { x, y, z };
        let inside = at(0.5, 2.0, 0.0);
        // Out of the crown to open ground, both ways round.
        assert!(!o.sight_blocked(inside, at(-8.0, 1.2, 0.0)));
        assert!(!o.sight_blocked(at(-8.0, 1.2, 0.0), inside));
        // The wall beyond is not its own crown: still blocked.
        assert!(o.sight_blocked(inside, at(15.0, 1.2, 0.0)));
        // Clean through the crown from outside to outside: blocked.
        assert!(o.sight_blocked(at(-8.0, 3.0, 0.0), at(8.0, 3.0, 0.0)));
        // Under the crown's base the point is not inside it at all.
        assert!(!o.sight_blocked(at(0.5, 1.2, 0.0), at(-8.0, 1.2, 0.0)));
    }

    /// The straight run a mob asks about before it leaves a wall: clear when
    /// the whole segment keeps a body's radius from every solid.
    #[test]
    fn a_run_is_clear_only_with_a_bodys_width_to_spare() {
        let o = yard();
        // Along the house's west face, a body's radius off it: clear.
        assert!(o.run_clear(
            p(5.0 - BODY_RADIUS, -4.0),
            p(5.0 - BODY_RADIUS, 4.0),
            BODY_RADIUS
        ));
        // Nearer than that, or into it, or across its corner: not.
        assert!(!o.run_clear(p(4.8, -4.0), p(4.8, 4.0), BODY_RADIUS));
        assert!(!o.run_clear(p(2.0, 0.0), p(8.0, 0.0), BODY_RADIUS));
        assert!(!o.run_clear(p(4.0, 6.0), p(6.0, 4.9), BODY_RADIUS));
        // Past the corner with room: clear.
        assert!(o.run_clear(p(4.0, 6.0), p(8.0, 5.5), BODY_RADIUS));
        // Clean through the house, both ends outside: not.
        assert!(!o.run_clear(p(0.0, 0.0), p(20.0, 0.0), BODY_RADIUS));
        assert!(!o.run_clear(p(f32::NAN, 0.0), p(1.0, 0.0), BODY_RADIUS));
        assert!(Obstacles::empty().run_clear(p(0.0, 0.0), p(20.0, 0.0), BODY_RADIUS));
    }

    /// A body moved out of a solid is stood on a floor: with a cliff face on
    /// the near side of a rock and level ground on the far side, it goes to
    /// the level ground, though the face is nearer.
    #[test]
    fn a_body_moved_out_of_a_solid_is_stood_on_a_floor() {
        // West of x = 0 the ground climbs at 70 degrees; east of it, flat.
        let mut heights = vec![0.0f32; 21 * 21];
        for iz in 0..21 {
            for ix in 0..10 {
                heights[iz * 21 + ix] = (10 - ix) as f32 * 10.0 * 70f32.to_radians().tan();
            }
        }
        let ground =
            super::super::terrain::Terrain::from_heights(20, 10.0, [0.0; 3], 0.0, heights).unwrap();
        // A rock whose west edge is at the cliff's foot; the point is nearer
        // its west edge.
        let o = Obstacles::from_prisms(&[Prism::rect(0.2, -2.0, 4.2, 2.0, 0.0, 3.0)]).unwrap();
        let put = o.place(
            &ground,
            Vec3f {
                x: 1.0,
                y: 0.0,
                z: 0.0,
            },
        );
        assert!(o.is_free(p(put.x, put.z), BODY_RADIUS));
        assert!(
            super::super::fall::is_floor(ground.gradient(put.x, put.z)),
            "stood on a face at {put:?}"
        );
        assert!(put.x > 0.0, "on the level side: {put:?}");
    }

    #[test]
    fn a_crown_blocks_above_its_base_and_not_under_it() {
        // A crown from 2 m to 9 m, 3 m across, drawn in by half for sight.
        let crown = Prism {
            flags: FLAG_SIGHT,
            sight_scale: 0.5,
            ..Prism::round(0.0, 0.0, 3.0, 8, 2.0, 9.0)
        };
        let o = Obstacles::from_prisms(&[crown]).unwrap();
        let at = |x: f32, y: f32, z: f32| Vec3f { x, y, z };
        // Under it at eye height: seen.
        assert!(!o.sight_blocked(at(-10.0, 1.2, 0.0), at(10.0, 1.2, 0.0)));
        // Through its middle at 5 m: blocked.
        assert!(o.sight_blocked(at(-10.0, 5.0, 0.0), at(10.0, 5.0, 0.0)));
        // 2 m off its axis at 5 m: outside the drawn-in outline, seen.
        assert!(!o.sight_blocked(at(-10.0, 5.0, 2.0), at(10.0, 5.0, 2.0)));
        // It stands in nobody's way.
        assert!(o.is_free(p(0.0, 0.0), BODY_RADIUS));
        let r = o.resolve(p(-1.0, 0.0), p(-0.7, 0.0), BODY_RADIUS);
        assert!(!r.deflected);
    }

    #[test]
    fn a_file_round_trips_and_a_damaged_one_is_refused() {
        let prisms = vec![
            Prism::rect(5.0, -5.0, 15.0, 5.0, 0.0, 4.0),
            Prism {
                flags: FLAG_SIGHT,
                sight_scale: 0.5,
                ..Prism::round(0.0, 12.0, 2.5, 6, 1.6, 9.7)
            },
        ];
        let bytes = Obstacles::encode(&prisms);
        let o = Obstacles::parse(&bytes).unwrap();
        assert_eq!(o.len(), 2);
        assert_eq!(o.walk_count(), 1);
        assert_eq!(o.hash_hex().len(), 12);
        assert_eq!(
            o.prism(0).unwrap().corners,
            prisms[0].corners,
            "the corners come back as written"
        );

        // One float changed: the hash no longer matches.
        let mut edited = bytes.clone();
        edited[HEADER_BYTES + 4] ^= 0x01;
        let err = Obstacles::parse(&edited).unwrap_err();
        assert!(err.contains("hash"), "{err}");
        // Cut short.
        assert!(Obstacles::parse(&bytes[..bytes.len() - 8]).is_err());
        assert!(Obstacles::parse(&bytes[..20]).is_err());
        // The wrong magic.
        let mut other = bytes.clone();
        other[0] ^= 0xFF;
        assert!(Obstacles::parse(&other).is_err());
    }

    #[test]
    fn an_outline_that_is_not_convex_and_counter_clockwise_is_refused() {
        // Clockwise.
        let mut clockwise = Prism::rect(0.0, 0.0, 1.0, 1.0, 0.0, 1.0);
        clockwise.corners.reverse();
        assert!(Obstacles::from_prisms(&[clockwise]).is_err());
        // A dart: one corner pushed in.
        let dart = Prism {
            corners: vec![p(0.0, 0.0), p(4.0, 0.0), p(1.0, 1.0), p(0.0, 4.0)],
            ..Prism::rect(0.0, 0.0, 1.0, 1.0, 0.0, 1.0)
        };
        assert!(Obstacles::from_prisms(&[dart]).is_err());
        // Two corners, no flags, a top under its base, a NaN.
        let mut two = Prism::rect(0.0, 0.0, 1.0, 1.0, 0.0, 1.0);
        two.corners.truncate(2);
        assert!(Obstacles::from_prisms(&[two]).is_err());
        let mut none = Prism::rect(0.0, 0.0, 1.0, 1.0, 0.0, 1.0);
        none.flags = 0;
        assert!(Obstacles::from_prisms(&[none]).is_err());
        assert!(Obstacles::from_prisms(&[Prism::rect(0.0, 0.0, 1.0, 1.0, 2.0, 1.0)]).is_err());
        assert!(Obstacles::from_prisms(&[Prism::rect(0.0, 0.0, f32::NAN, 1.0, 0.0, 1.0)]).is_err());
    }

    /// The fixtures' own scenery: boxes, columns of a few shapes (their
    /// corners written out, so no cosine's last bit can differ between
    /// builds), some drawn in for sight, some sight-only crowns.
    fn fixture_prisms(rng: &mut rand::rngs::StdRng) -> Vec<Prism> {
        const HEX: [(f32, f32); 6] = [
            (1.0, 0.0),
            (0.5, 0.866),
            (-0.5, 0.866),
            (-1.0, 0.0),
            (-0.5, -0.866),
            (0.5, -0.866),
        ];
        const PENT: [(f32, f32); 5] = [
            (1.0, 0.0),
            (0.309, 0.951),
            (-0.809, 0.588),
            (-0.809, -0.588),
            (0.309, -0.951),
        ];
        let mut prisms = Vec::new();
        for i in 0..48 {
            let (x, z): (f32, f32) = (rng.gen_range(-30.0..30.0), rng.gen_range(-30.0..30.0));
            let snap = |v: f32| (v * 1000.0).round() / 1000.0;
            let (x, z) = (snap(x), snap(z));
            let top = snap(rng.gen_range(0.5..9.0));
            let mut prism = match i % 3 {
                0 => Prism::rect(
                    x,
                    z,
                    snap(x + rng.gen_range(0.07..7.0)),
                    snap(z + rng.gen_range(0.07..7.0)),
                    0.0,
                    top,
                ),
                1 => {
                    let r: f32 = rng.gen_range(0.2..1.6);
                    Prism {
                        corners: HEX
                            .iter()
                            .map(|(cx, cz)| P2::new(snap(x + cx * r), snap(z + cz * r)))
                            .collect(),
                        ..Prism::rect(0.0, 0.0, 1.0, 1.0, 0.0, top)
                    }
                }
                _ => {
                    let r: f32 = rng.gen_range(0.1..2.6);
                    Prism {
                        corners: PENT
                            .iter()
                            .map(|(cx, cz)| P2::new(snap(x + cx * r), snap(z + cz * r)))
                            .collect(),
                        ..Prism::rect(0.0, 0.0, 1.0, 1.0, 0.0, top)
                    }
                }
            };
            match i % 5 {
                3 => {
                    // A crown: sight only, off the ground, drawn in.
                    prism.flags = FLAG_SIGHT;
                    prism.base = 1.5;
                    prism.top = prism.base + top;
                    prism.sight_scale = 0.5;
                }
                4 => prism.sight_scale = 0.8,
                _ => {}
            }
            prisms.push(prism);
        }
        prisms
    }

    /// The rule's answers on a fixed set of cases, as text the client's
    /// `tools/check_obstacle_rule.gd` replays through its GDScript copy of
    /// the rule. Only cases whose answer does not hang on the last bit are
    /// kept: each is asked again with its inputs nudged, and dropped unless
    /// the answers agree.
    fn rule_fixtures() -> String {
        use std::fmt::Write;
        let mut rng = rand::rngs::StdRng::seed_from_u64(0x0B57_AC1E);
        let prisms = fixture_prisms(&mut rng);
        let o = Obstacles::from_prisms(&prisms).unwrap();
        let mut out = String::new();
        out.push_str(
            "# Written by `cargo test` (world/obstacles.rs, rule_fixtures); do not edit.\n\
             # Regenerate with PD_WRITE_FIXTURES=1 after a deliberate change to the rule.\n\
             # prism flags base top sight_scale n x z ...\n\
             # step from_x from_z to_x to_z radius -> x z deflected\n\
             # clear x z radius -> x z\n\
             # sight ax ay az bx by bz -> blocked\n",
        );
        for q in &prisms {
            write!(
                out,
                "prism {} {:?} {:?} {:?} {}",
                q.flags,
                q.base,
                q.top,
                q.sight_scale,
                q.corners.len()
            )
            .unwrap();
            for c in &q.corners {
                write!(out, " {:?} {:?}", c.x, c.z).unwrap();
            }
            out.push('\n');
        }
        const NUDGE: f32 = 0.000_05;
        const SAME: f32 = 0.001;
        let nudges = [(NUDGE, 0.0), (-NUDGE, 0.0), (0.0, NUDGE), (0.0, -NUDGE)];
        let close = |a: P2, b: P2| a.dist2(b).sqrt() <= SAME;

        let mut steps = 0;
        while steps < 600 {
            let start = p(rng.gen_range(-34.0..34.0), rng.gen_range(-34.0..34.0));
            // Mostly from free ground; one in ten from wherever it fell.
            let from = if rng.gen_bool(0.9) {
                o.clear_of(start, BODY_RADIUS)
            } else {
                start
            };
            let (dx, dz): (f32, f32) = (rng.gen_range(-1.0..1.0), rng.gen_range(-1.0..1.0));
            let len = (dx * dx + dz * dz).sqrt();
            if len < 0.05 {
                continue;
            }
            let step: f32 = rng.gen_range(0.02..0.45);
            let pieces = step / SUBSTEP;
            if (pieces - pieces.round()).abs() < 0.01 {
                continue;
            }
            let to = p(from.x + dx / len * step, from.z + dz / len * step);
            let r = o.resolve(from, to, BODY_RADIUS);
            let steady = nudges.iter().all(|(nx, nz)| {
                let n = o.resolve(
                    p(from.x + nx, from.z + nz),
                    p(to.x + nx, to.z + nz),
                    BODY_RADIUS,
                );
                n.deflected == r.deflected && close(p(n.x, n.z), p(r.x, r.z))
            });
            // Half the kept cases are ones something turned aside.
            if !steady || (!r.deflected && steps % 2 == 0) {
                continue;
            }
            writeln!(
                out,
                "step {:?} {:?} {:?} {:?} {:?} -> {:?} {:?} {}",
                from.x,
                from.z,
                to.x,
                to.z,
                BODY_RADIUS,
                r.x,
                r.z,
                u8::from(r.deflected)
            )
            .unwrap();
            steps += 1;
        }

        let mut clears = 0;
        while clears < 150 {
            let at = p(rng.gen_range(-32.0..32.0), rng.gen_range(-32.0..32.0));
            let c = o.clear_of(at, BODY_RADIUS);
            let steady = nudges
                .iter()
                .all(|(nx, nz)| close(o.clear_of(p(at.x + nx, at.z + nz), BODY_RADIUS), c));
            // Mostly points that had to be moved.
            if !steady || (c == at && clears % 4 != 0) {
                continue;
            }
            writeln!(
                out,
                "clear {:?} {:?} {:?} -> {:?} {:?}",
                at.x, at.z, BODY_RADIUS, c.x, c.z
            )
            .unwrap();
            clears += 1;
        }

        let mut sights = 0;
        while sights < 400 {
            let a = Vec3f {
                x: rng.gen_range(-34.0..34.0),
                y: rng.gen_range(0.2..10.0),
                z: rng.gen_range(-34.0..34.0),
            };
            let b = Vec3f {
                x: a.x + rng.gen_range(-30.0..30.0),
                y: rng.gen_range(0.2..10.0),
                z: a.z + rng.gen_range(-30.0..30.0),
            };
            let blocked = o.sight_blocked(a, b);
            let shifts = [
                (0.002f32, 0.0f32, 0.0f32),
                (-0.002, 0.0, 0.0),
                (0.0, 0.002, 0.0),
                (0.0, -0.002, 0.0),
                (0.0, 0.0, 0.002),
                (0.0, 0.0, -0.002),
            ];
            let steady = shifts.iter().all(|(sx, sy, sz)| {
                let moved = Vec3f {
                    x: a.x + sx,
                    y: a.y + sy,
                    z: a.z + sz,
                };
                o.sight_blocked(moved, b) == blocked
            });
            if !steady || (!blocked && sights % 2 == 0) {
                continue;
            }
            writeln!(
                out,
                "sight {:?} {:?} {:?} {:?} {:?} {:?} -> {}",
                a.x,
                a.y,
                a.z,
                b.x,
                b.y,
                b.z,
                u8::from(blocked)
            )
            .unwrap();
            sights += 1;
        }
        out
    }

    /// `data/obstacle_rule_fixtures.txt` is what the rule answers today. The
    /// client's check replays the file through its own copy of the rule, so a
    /// change to the rule on one side alone fails here or there. After a
    /// deliberate change, run once with PD_WRITE_FIXTURES=1 and commit the file.
    #[test]
    fn the_rule_fixtures_are_current() {
        let path = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/data/obstacle_rule_fixtures.txt"
        );
        let text = rule_fixtures();
        if std::env::var("PD_WRITE_FIXTURES").as_deref() == Ok("1") {
            std::fs::write(path, &text).unwrap();
        }
        let held = std::fs::read_to_string(path)
            .unwrap_or_default()
            .replace("\r\n", "\n");
        assert!(
            held == text,
            "data/obstacle_rule_fixtures.txt is not what the rule answers now; \
             if the rule was changed on purpose, rerun with PD_WRITE_FIXTURES=1, \
             commit the file, and make the same change in the client's \
             scripts/zone/zone_obstacles.gd"
        );
        assert!(text.lines().filter(|l| l.starts_with("step ")).count() == 600);
    }

    #[test]
    fn placing_a_body_stands_it_clear_and_on_the_ground() {
        let ground = super::super::terrain::Terrain::flat(20, 20.0, 0.0);
        let o = yard();
        let put = o.place(
            &ground,
            Vec3f {
                x: 6.0,
                y: 37.0,
                z: 0.0,
            },
        );
        assert!((put.x - (5.0 - BODY_RADIUS)).abs() < 1e-3 && put.z.abs() < 1e-4);
        assert_eq!(put.y, 0.0, "on the ground, whatever height it was given");
        let free = Vec3f {
            x: -3.0,
            y: 0.0,
            z: -3.0,
        };
        assert_eq!(o.place(&ground, free).into_tuple(), free.into_tuple());
    }

    #[test]
    fn the_compiled_obstacles_parse_and_their_hash_matches() {
        let o = compiled();
        assert!(o.len() > 1000, "the zone has {} prisms", o.len());
        assert!(o.walk_count() > 1000);
        assert_eq!(o.hash_hex().len(), 12);
        // The starter spawn is free ground.
        assert!(o.is_free(p(0.0, 0.0), BODY_RADIUS));
        // The dist helper used by other tests agrees with the rule on a wall.
        let wall = Obstacles::from_prisms(&[Prism::rect(5.0, -5.0, 15.0, 5.0, 0.0, 4.0)]).unwrap();
        let r = wall.resolve(p(4.0, 4.9), p(4.9, 5.2), BODY_RADIUS);
        assert!(dist_to_rect(p(r.x, r.z), 5.0, -5.0, 15.0, 5.0) >= BODY_RADIUS - 2e-3);
    }
}
