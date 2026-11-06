//! The zone's compiled ground held to the content that stands on it (the first
//! hills, `first_hills_plan.md` in the client repo): every camp spawn and NPC
//! on a floor, the town flat, the plate's rim a face nobody climbs out over.
//! The client's `tools/author_terrain.gd` reports the same before it writes the
//! heights; these hold it on the server side, so a hand edit of `terrain.toml`
//! (which its hash refuses anyway), of `zone_camps.toml` or of `npcs.toml` that
//! puts a mob or an NPC on a face fails here.

use super::connection::Vec3f;
use super::terrain::{compiled, Terrain};
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

/// The rim's outer cell row is a face all the way round, so no client walks
/// or climbs off the plate into the flat beyond it. Walkers start every metre
/// along the rim's inner cell row (half a cell in from the outer row) and
/// head outward, and 45 degrees either side of outward (the zigzag the slope
/// rule's face condition closes), each step as long as a hasted run's; none
/// gets more than a hair into the outer row. The plate is centred on the
/// origin (the exporter and the client's lockstep check hold the origin).
#[test]
fn nobody_climbs_out_over_the_rim() {
    // Selos' Melody at its full 1.65 on the 5 m/s run, one 20 Hz tick.
    const STEP: f32 = 0.42;
    let t = compiled();
    let half = t.grid_size() as f32 * t.cell_size() * 0.5;
    let start = half - 1.5 * t.cell_size();
    let outer_row = half - t.cell_size();
    let mut escaped = Vec::new();
    let mut s = -start;
    while s <= start {
        // (where it starts, which way is out) for each side of the plate.
        for (x, z, ox, oz) in [
            (s, start, 0.0, 1.0),
            (s, -start, 0.0, -1.0),
            (start, s, 1.0, 0.0),
            (-start, s, -1.0, 0.0),
        ] {
            for turn in [-45f32, 0.0, 45.0] {
                let (sin, cos) = turn.to_radians().sin_cos();
                let (dx, dz) = ((ox * cos - oz * sin) * STEP, (ox * sin + oz * cos) * STEP);
                let mut p = t.ground_vec(Vec3f { x, y: 0.0, z });
                for _ in 0..200 {
                    p = t.player_step(p, dx, dz).pos;
                }
                if p.x.abs().max(p.z.abs()) > outer_row + 0.5 {
                    escaped.push(((x, z), turn, (p.x, p.y, p.z)));
                }
            }
        }
        s += 1.0;
    }
    assert!(
        escaped.is_empty(),
        "walkers that climbed the rim: {escaped:?}"
    );
}
