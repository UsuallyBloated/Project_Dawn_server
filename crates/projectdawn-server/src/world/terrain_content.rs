//! The zone's compiled ground held to the content that stands on it (the first
//! hills, `first_hills_plan.md` in the client repo): every camp spawn and NPC
//! on a floor, the town flat, and the zone closed: the mountains outside the
//! walkable edge are a face at their foot, so nobody walks out.
//! The client's `tools/author_terrain.gd` reports the same before it writes the
//! heights; these hold it on the server side, so a hand edit of `terrain.toml`
//! (which its hash refuses anyway), of `zone_camps.toml` or of `npcs.toml` that
//! puts a mob or an NPC on a face fails here.

use super::connection::Vec3f;
use super::terrain::{compiled, Terrain, SLOPE_LIMIT_DEG, SLOPE_SLACK};
use super::{fall, npcs, safe_areas, zones};

/// The spawn and the four corners of its jitter square: a mob spawns anywhere
/// inside `radius` of its spawn point on x and z (`spawn_points.rs`).
fn jitter_square(x: f32, z: f32, r: f32) -> [(f32, f32); 5] {
    [
        (x, z),
        (x - r, z - r),
        (x + r, z - r),
        (x - r, z + r),
        (x + r, z + r),
    ]
}

fn slope_deg(t: &Terrain, x: f32, z: f32) -> f32 {
    let (gx, gz) = t.gradient(x, z);
    (gx * gx + gz * gz).sqrt().atan().to_degrees()
}

#[test]
fn every_camp_spawns_on_a_floor() {
    let t = compiled();
    let mut bad = Vec::new();
    for c in zones::load_camps() {
        for (x, z) in jitter_square(c.pos[0], c.pos[2], c.radius) {
            if !fall::is_floor(t.gradient(x, z)) {
                bad.push(format!(
                    "{} at ({x}, {z}): {:.1} deg",
                    c.mob.name,
                    slope_deg(t, x, z)
                ));
            }
        }
    }
    assert!(
        bad.is_empty(),
        "camp spawns on ground no body stands on: {bad:?}"
    );
}

#[test]
fn every_npc_stands_on_flat_ground() {
    let t = compiled();
    for n in npcs::all() {
        let (x, z) = (n.pos[0], n.pos[2]);
        assert!(
            fall::is_floor(t.gradient(x, z)),
            "{} stands on {:.1} deg",
            n.name,
            slope_deg(t, x, z)
        );
        assert_eq!(
            t.ground(x, z),
            t.floor_y(),
            "{} is not on the floor",
            n.name
        );
    }
}

#[test]
fn the_town_square_is_flat() {
    let t = compiled();
    for area in safe_areas::areas() {
        let c = area.center();
        let r = area.radius.ceil() as i32;
        for dx in -r..=r {
            for dz in -r..=r {
                let (x, z) = (c.x + dx as f32, c.z + dz as f32);
                if ((dx * dx + dz * dz) as f32).sqrt() > area.radius {
                    continue;
                }
                assert_eq!(
                    t.ground(x, z),
                    t.floor_y(),
                    "{} is not flat at ({x}, {z})",
                    area.name
                );
                assert_eq!(
                    t.gradient(x, z),
                    (0.0, 0.0),
                    "{} slopes at ({x}, {z})",
                    area.name
                );
            }
        }
    }
}

/// The ground as the client's collider splits it: two triangles a cell, the
/// lower one on the cell's first corner and the upper one on its last, cut from
/// `(x1, z0)` to `(x0, z1)`. A triangle's index is `(cell z * grid + cell x) *
/// 2`, plus 1 for the upper one.
struct Walk {
    gs: usize,
    cell: f32,
    half: f32,
    /// Steeper than the slope rule's limit: a step up onto it is refused.
    face: Vec<bool>,
    /// The height of the triangle's middle.
    mid: Vec<f32>,
    /// Whether a walk from the town gets there.
    reached: Vec<bool>,
}

/// A triangle's corners as grid offsets from its cell.
fn corners(which: usize) -> [(i32, i32); 3] {
    if which == 0 {
        [(0, 0), (1, 0), (0, 1)]
    } else {
        [(1, 0), (1, 1), (0, 1)]
    }
}

impl Walk {
    fn triangle_at(&self, x: f32, z: f32) -> usize {
        let max = self.gs as f32 - 0.001;
        let fx = ((x + self.half) / self.cell).clamp(0.0, max);
        let fz = ((z + self.half) / self.cell).clamp(0.0, max);
        let (cx, cz) = (fx as usize, fz as usize);
        let upper = usize::from((fx - cx as f32) + (fz - cz as f32) > 1.0);
        (cz * self.gs + cx) * 2 + upper
    }

    fn centre(&self, tri: usize) -> (f32, f32) {
        let (cell, which) = (tri / 2, tri % 2);
        let (cx, cz) = ((cell % self.gs) as f32, (cell / self.gs) as f32);
        let (sx, sz) = corners(which)
            .iter()
            .fold((0.0, 0.0), |(x, z), c| (x + c.0 as f32, z + c.1 as f32));
        (
            -self.half + (cx + sx / 3.0) * self.cell,
            -self.half + (cz + sz / 3.0) * self.cell,
        )
    }

    /// Every other triangle that shares a corner with this one.
    fn neighbours(&self, tri: usize) -> Vec<usize> {
        let (cell, which) = (tri / 2, tri % 2);
        let (cx, cz) = ((cell % self.gs) as i32, (cell / self.gs) as i32);
        let mine = corners(which);
        let mut out = Vec::new();
        for dz in -1..=1 {
            for dx in -1..=1 {
                let (nx, nz) = (cx + dx, cz + dz);
                if nx < 0 || nz < 0 || nx >= self.gs as i32 || nz >= self.gs as i32 {
                    continue;
                }
                for other in 0..2 {
                    if dx == 0 && dz == 0 && other == which {
                        continue;
                    }
                    let shares = corners(other)
                        .iter()
                        .any(|c| mine.contains(&(c.0 + dx, c.1 + dz)));
                    if shares {
                        out.push((nz as usize * self.gs + nx as usize) * 2 + other);
                    }
                }
            }
        }
        out
    }
}

/// Who can walk where on the compiled ground, by the slope rule's own reading
/// (`Terrain::slope_ok`): a step that rises onto a face is refused, so a face
/// is entered only from above, and from a face (which no body stands on) the
/// only way is down. Triangle middles stand in for the heights; the walkers in
/// `nobody_walks_out_of_the_zone` hold this reading to the real step.
fn walk_from_town(t: &Terrain) -> Walk {
    let gs = t.grid_size();
    let cell = t.cell_size();
    let half = gs as f32 * cell * 0.5;
    let limit = SLOPE_LIMIT_DEG.to_radians().tan() * (1.0 + SLOPE_SLACK);
    let h = |ix: usize, iz: usize| {
        t.mesh_height(-half + ix as f32 * cell, -half + iz as f32 * cell)
            .expect("a grid vertex is on the mesh")
    };
    let mut walk = Walk {
        gs,
        cell,
        half,
        face: vec![false; gs * gs * 2],
        mid: vec![0.0; gs * gs * 2],
        reached: vec![false; gs * gs * 2],
    };
    for cz in 0..gs {
        for cx in 0..gs {
            let (h00, h10, h01, h11) = (h(cx, cz), h(cx + 1, cz), h(cx, cz + 1), h(cx + 1, cz + 1));
            let tri = (cz * gs + cx) * 2;
            let lower = ((h10 - h00).powi(2) + (h01 - h00).powi(2)).sqrt() / cell;
            let upper = ((h11 - h01).powi(2) + (h11 - h10).powi(2)).sqrt() / cell;
            walk.face[tri] = lower > limit;
            walk.face[tri + 1] = upper > limit;
            walk.mid[tri] = (h00 + h10 + h01) / 3.0;
            walk.mid[tri + 1] = (h10 + h11 + h01) / 3.0;
        }
    }
    let start = walk.triangle_at(0.0, 0.0);
    walk.reached[start] = true;
    let mut queue = std::collections::VecDeque::from([start]);
    while let Some(tri) = queue.pop_front() {
        for next in walk.neighbours(tri) {
            if walk.reached[next] {
                continue;
            }
            // Onto a face, or off one: downhill only.
            if (walk.face[tri] || walk.face[next]) && walk.mid[next] > walk.mid[tri] {
                continue;
            }
            walk.reached[next] = true;
            queue.push_back(next);
        }
    }
    walk
}

/// Every way out of the zone the ground offers, as lines to read (none, on
/// the compiled ground). Two halves. The walk from the town (the slope rule
/// read over every triangle) must reach about half the plate and no part of
/// its outer rows. Then the real step holds that reading: from every walked
/// triangle that touches one the walk did not reach, walkers head for the
/// unreached ground and 45 degrees either side of it (the zigzag the slope
/// rule's face condition closes), each step as long as a hasted run's; none
/// may ever stand on ground the walk did not reach (a walker may press against
/// a face, which holds nobody).
fn ways_out(t: &Terrain) -> Vec<String> {
    // Selos' Melody at its full 1.65 on the 5 m/s run, one 20 Hz tick.
    const STEP: f32 = 0.42;
    const STEPS: usize = 160;
    let walk = walk_from_town(t);
    let gs = walk.gs;
    let mut out = Vec::new();

    let reached = walk.reached.iter().filter(|r| **r).count();
    let share = reached as f32 / walk.reached.len() as f32;
    if !(0.45..0.56).contains(&share) {
        out.push(format!(
            "a walk from the town reaches {:.1}% of the plate (the two basins are about half of it)",
            share * 100.0
        ));
    }
    let mut edge_cells = 0usize;
    for cz in 0..gs {
        for cx in 0..gs {
            let outer = cx < 3 || cz < 3 || cx >= gs - 3 || cz >= gs - 3;
            let tri = (cz * gs + cx) * 2;
            if outer && (walk.reached[tri] || walk.reached[tri + 1]) {
                edge_cells += 1;
            }
        }
    }
    if edge_cells > 0 {
        out.push(format!(
            "a walk from the town reaches {edge_cells} cells of the plate's outer rows"
        ));
    }

    let mut walkers = 0usize;
    let mut stood = 0usize;
    for tri in 0..walk.reached.len() {
        if !walk.reached[tri] || walk.face[tri] {
            continue;
        }
        let (x, z) = walk.centre(tri);
        let (mut ox, mut oz) = (0.0, 0.0);
        for n in walk.neighbours(tri) {
            if !walk.reached[n] {
                let (nx, nz) = walk.centre(n);
                ox += nx - x;
                oz += nz - z;
            }
        }
        let len = (ox * ox + oz * oz).sqrt();
        if len < 1e-3 {
            continue;
        }
        let (ox, oz) = (ox / len, oz / len);
        for turn in [-45f32, 0.0, 45.0] {
            walkers += 1;
            let (sin, cos) = turn.to_radians().sin_cos();
            let (dx, dz) = ((ox * cos - oz * sin) * STEP, (ox * sin + oz * cos) * STEP);
            let mut p = t.ground_vec(Vec3f { x, y: 0.0, z });
            for _ in 0..STEPS {
                p = t.player_step(p, dx, dz).pos;
                let at = walk.triangle_at(p.x, p.z);
                if !walk.reached[at] && !walk.face[at] {
                    stood += 1;
                    if stood <= 8 {
                        out.push(format!(
                            "a walker from ({x:.0}, {z:.0}), {turn} degrees off outward, stood at \
                             ({:.1}, {:.1}, {:.1}), where no walk from the town reaches",
                            p.x, p.y, p.z
                        ));
                    }
                    break;
                }
            }
        }
    }
    if walkers < 3000 {
        out.push(format!(
            "only {walkers} walkers: the zone's edge is longer than that"
        ));
    }
    out
}

/// The walkable world is closed: the mountains stand outside its edge (the
/// client's `data/zone_layout.gd` BASINS; `tools/author_terrain.gd` raises
/// them), their foot a face all the way round, so no client walks or climbs
/// out of the zone or off the plate.
#[test]
fn nobody_walks_out_of_the_zone() {
    let found = ways_out(compiled());
    assert!(found.is_empty(), "ways out of the zone: {found:#?}");
}

/// The test above reads a real hole: the same ground with a level corridor cut
/// through the mountains behind the east door, out to the plate's edge.
#[test]
fn a_gap_in_the_mountains_is_a_way_out() {
    let t = compiled();
    let gs = t.grid_size();
    let cell = t.cell_size();
    let half = gs as f32 * cell * 0.5;
    let floor = t.ground(1880.0, -900.0);
    let mut heights = Vec::with_capacity((gs + 1) * (gs + 1));
    for iz in 0..=gs {
        for ix in 0..=gs {
            let (x, z) = (-half + ix as f32 * cell, -half + iz as f32 * cell);
            let h = t.mesh_height(x, z).expect("a grid vertex is on the mesh");
            let in_corridor = x >= 1880.0 && (-930.0..=-870.0).contains(&z);
            heights.push(if in_corridor { floor } else { h });
        }
    }
    let holed = Terrain::from_heights(gs, cell, [0.0, 0.0, 0.0], t.floor_y(), heights)
        .expect("the same grid with other heights");
    let found = ways_out(&holed);
    assert!(
        found.iter().any(|line| line.contains("outer rows")),
        "a corridor to the plate's edge went unseen: {found:#?}"
    );
}
