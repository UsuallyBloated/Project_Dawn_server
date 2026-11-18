//! The zone's compiled obstacles held to the content that stands among them
//! (`docs/design/obstacle_collision.md`): the town's people, the arrival
//! points and every camp's spawns on free ground, the town's four gates open,
//! and no pocket: no free ground a body could be put on and never leave. The
//! client's exporter bakes the scenery; these hold it on the server side, so
//! a wall drawn across a gate, a tent pitched on a spawn or a fence closed
//! all the way round fails here before anyone meets it.

use super::connection::Vec3f;
use super::obstacles::{compiled, Obstacles, Prism, BODY_RADIUS, P2};
use super::{npcs, safe_areas, terrain, zones};
use std::collections::HashMap;

/// How far a camp's spawn may be moved to stand clear of its own camp's
/// scenery (`Obstacles::clear_of` at spawn): past this the camp and its set
/// piece need moving apart.
const SPAWN_SHIFT_LIMIT: f32 = 6.0;
/// The pocket search's cell, metres: a passage narrower than a body plus this
/// reads as closed, so a squeeze that tight is refused as well.
const POCKET_CELL: f32 = 0.2;
/// Two solids nearer than a body is wide belong to one cluster: nothing
/// walks between them.
const BODY_WIDTH: f32 = 2.0 * BODY_RADIUS;

fn walk_prisms(o: &Obstacles) -> Vec<Prism> {
    (0..o.len())
        .filter_map(|i| o.prism(i))
        .filter(|p| p.flags & super::obstacles::FLAG_WALK != 0)
        .collect()
}

fn bounds(p: &Prism) -> (f32, f32, f32, f32) {
    let mut b = (
        f32::INFINITY,
        f32::INFINITY,
        f32::NEG_INFINITY,
        f32::NEG_INFINITY,
    );
    for c in &p.corners {
        b = (b.0.min(c.x), b.1.min(c.z), b.2.max(c.x), b.3.max(c.z));
    }
    b
}

/// How far a point is from a convex outline (0 inside it).
fn point_gap(p: &Prism, q: P2) -> f32 {
    let n = p.corners.len();
    let mut inside = true;
    let mut near = f32::INFINITY;
    for k in 0..n {
        let a = p.corners[k];
        let b = p.corners[(k + 1) % n];
        let (ex, ez) = (b.x - a.x, b.z - a.z);
        let len2 = ex * ex + ez * ez;
        if len2 <= 0.0 {
            continue;
        }
        if (q.x - a.x) * ez - (q.z - a.z) * ex > 0.0 {
            inside = false;
        }
        let t = (((q.x - a.x) * ex + (q.z - a.z) * ez) / len2).clamp(0.0, 1.0);
        let (dx, dz) = (q.x - (a.x + ex * t), q.z - (a.z + ez * t));
        near = near.min((dx * dx + dz * dz).sqrt());
    }
    if inside {
        0.0
    } else {
        near
    }
}

/// The gap between two convex outlines, near enough: the least distance from
/// a corner of either to the other (two outlines that cross without a corner
/// inside the other read as their corners' distance, which is small).
fn gap(a: &Prism, b: &Prism) -> f32 {
    let from_a = a.corners.iter().map(|c| point_gap(b, *c));
    let from_b = b.corners.iter().map(|c| point_gap(a, *c));
    from_a.chain(from_b).fold(f32::INFINITY, f32::min)
}

/// The walk prisms grouped into clusters nothing walks through: each
/// cluster's members' indices.
fn clusters(prisms: &[Prism]) -> Vec<Vec<usize>> {
    const CELL: f32 = 8.0;
    let cell = |v: f32| (v / CELL).floor() as i32;
    let mut grid: HashMap<(i32, i32), Vec<usize>> = HashMap::new();
    for (i, p) in prisms.iter().enumerate() {
        let b = bounds(p);
        for cz in cell(b.1 - BODY_WIDTH)..=cell(b.3 + BODY_WIDTH) {
            for cx in cell(b.0 - BODY_WIDTH)..=cell(b.2 + BODY_WIDTH) {
                grid.entry((cx, cz)).or_default().push(i);
            }
        }
    }
    let mut parent: Vec<usize> = (0..prisms.len()).collect();
    fn find(parent: &mut [usize], mut i: usize) -> usize {
        while parent[i] != i {
            parent[i] = parent[parent[i]];
            i = parent[i];
        }
        i
    }
    for held in grid.values() {
        for (k, &i) in held.iter().enumerate() {
            for &j in &held[k + 1..] {
                let (ri, rj) = (find(&mut parent, i), find(&mut parent, j));
                if ri != rj && gap(&prisms[i], &prisms[j]) < BODY_WIDTH {
                    parent[ri] = rj;
                }
            }
        }
    }
    let mut by_root: HashMap<usize, Vec<usize>> = HashMap::new();
    for i in 0..prisms.len() {
        let r = find(&mut parent, i);
        by_root.entry(r).or_default().push(i);
    }
    by_root.into_values().filter(|m| m.len() >= 2).collect()
}

/// The pockets a cluster closes: each a set of free cells (where a body could
/// stand) from which no walk reaches the window's edge. Returned as the
/// middles of each pocket's cells.
fn pockets_of(prisms: &[Prism], members: &[usize]) -> Vec<Vec<P2>> {
    let mut b = (
        f32::INFINITY,
        f32::INFINITY,
        f32::NEG_INFINITY,
        f32::NEG_INFINITY,
    );
    for &i in members {
        let pb = bounds(&prisms[i]);
        b = (b.0.min(pb.0), b.1.min(pb.1), b.2.max(pb.2), b.3.max(pb.3));
    }
    // A margin a body can walk round the whole cluster in.
    let margin = 2.0;
    let (x0, z0) = (b.0 - margin, b.1 - margin);
    let w = ((b.2 + margin - x0) / POCKET_CELL).ceil() as usize + 1;
    let h = ((b.3 + margin - z0) / POCKET_CELL).ceil() as usize + 1;
    let at =
        |cx: usize, cz: usize| P2::new(x0 + cx as f32 * POCKET_CELL, z0 + cz as f32 * POCKET_CELL);
    // Blocked: a body's centre there touches a solid. Marked prism by prism
    // over each one's own neighbourhood, not asked cell by cell.
    let mut blocked = vec![false; w * h];
    for p in prisms {
        let pb = bounds(p);
        if pb.2 < x0 - 1.0
            || pb.0 > x0 + w as f32 * POCKET_CELL + 1.0
            || pb.3 < z0 - 1.0
            || pb.1 > z0 + h as f32 * POCKET_CELL + 1.0
        {
            continue;
        }
        let lo_x = (((pb.0 - BODY_RADIUS - x0) / POCKET_CELL).floor().max(0.0)) as usize;
        let lo_z = (((pb.1 - BODY_RADIUS - z0) / POCKET_CELL).floor().max(0.0)) as usize;
        let hi_x =
            ((((pb.2 + BODY_RADIUS - x0) / POCKET_CELL).ceil()).max(0.0) as usize).min(w - 1);
        let hi_z =
            ((((pb.3 + BODY_RADIUS - z0) / POCKET_CELL).ceil()).max(0.0) as usize).min(h - 1);
        for cz in lo_z..=hi_z {
            for cx in lo_x..=hi_x {
                if !blocked[cz * w + cx] && point_gap(p, at(cx, cz)) < BODY_RADIUS {
                    blocked[cz * w + cx] = true;
                }
            }
        }
    }
    // Flood the free cells from the window's edge.
    let mut reached = vec![false; w * h];
    let mut stack: Vec<(usize, usize)> = Vec::new();
    for cx in 0..w {
        stack.push((cx, 0));
        stack.push((cx, h - 1));
    }
    for cz in 0..h {
        stack.push((0, cz));
        stack.push((w - 1, cz));
    }
    while let Some((cx, cz)) = stack.pop() {
        let i = cz * w + cx;
        if blocked[i] || reached[i] {
            continue;
        }
        reached[i] = true;
        if cx > 0 {
            stack.push((cx - 1, cz));
        }
        if cx + 1 < w {
            stack.push((cx + 1, cz));
        }
        if cz > 0 {
            stack.push((cx, cz - 1));
        }
        if cz + 1 < h {
            stack.push((cx, cz + 1));
        }
    }
    // What is free and unreached, gathered into pockets.
    let mut out = Vec::new();
    let mut seen = vec![false; w * h];
    for start in 0..w * h {
        if blocked[start] || reached[start] || seen[start] {
            continue;
        }
        let mut cells = Vec::new();
        let mut stack = vec![start];
        while let Some(i) = stack.pop() {
            if blocked[i] || reached[i] || seen[i] {
                continue;
            }
            seen[i] = true;
            let (cx, cz) = (i % w, i / w);
            cells.push(at(cx, cz));
            if cx > 0 {
                stack.push(i - 1);
            }
            if cx + 1 < w {
                stack.push(i + 1);
            }
            if cz > 0 {
                stack.push(i - w);
            }
            if cz + 1 < h {
                stack.push(i + w);
            }
        }
        out.push(cells);
    }
    out
}

fn xz(v: Vec3f) -> P2 {
    P2::new(v.x, v.z)
}

#[test]
fn the_towns_people_and_every_arrival_point_stand_on_free_ground() {
    let o = compiled();
    let mut bad = Vec::new();
    for n in npcs::all() {
        if !o.is_free(xz(n.position()), BODY_RADIUS) {
            bad.push(format!("{} at {:?}", n.name, n.pos));
        }
    }
    for area in safe_areas::areas() {
        if !o.is_free(xz(area.arrival()), BODY_RADIUS) {
            bad.push(format!("the arrival point of {}", area.name));
        }
    }
    if !o.is_free(xz(super::STARTER_SPAWN), BODY_RADIUS) {
        bad.push("the starter spawn".into());
    }
    assert!(bad.is_empty(), "standing inside the scenery: {bad:?}");
}

/// A camp's spawns may fall on its own set piece (the camp is drawn where the
/// camp is); the spawner stands each mob at the nearest free point. That
/// point must be near: a spawn moved further than `SPAWN_SHIFT_LIMIT` means a
/// set piece covers its camp.
#[test]
fn every_camp_spawn_is_on_or_beside_free_ground() {
    let o = compiled();
    let mut bad = Vec::new();
    let mut moved = 0;
    for c in zones::load_camps() {
        let r = c.radius;
        for (dx, dz) in [(0.0, 0.0), (-r, -r), (r, -r), (-r, r), (r, r)] {
            let at = P2::new(c.pos[0] + dx, c.pos[2] + dz);
            let put = o.clear_of(at, BODY_RADIUS);
            let shift = ((put.x - at.x).powi(2) + (put.z - at.z).powi(2)).sqrt();
            if shift > 0.0 {
                moved += 1;
            }
            if !o.is_free(put, BODY_RADIUS) || shift > SPAWN_SHIFT_LIMIT {
                bad.push(format!(
                    "{} at ({}, {}) moved {shift:.1} m",
                    c.mob.name, at.x, at.z
                ));
            }
        }
    }
    println!("{moved} spawn points and jitter corners are moved clear of scenery");
    assert!(bad.is_empty(), "camp spawns buried in scenery: {bad:?}");
}

/// A body walks out of the square through each of the four gates and back in,
/// with the real step on the real ground and the real obstacles, and a body
/// walked at the wall beside each gate is held outside it.
#[test]
fn the_four_gates_are_open_and_the_wall_beside_them_is_not() {
    let o = compiled();
    let t = terrain::compiled();
    let walk = |from: P2, to: P2| -> P2 {
        let mut at = t.ground_vec(Vec3f {
            x: from.x,
            y: 0.0,
            z: from.z,
        });
        for _ in 0..2000 {
            let (dx, dz) = (to.x - at.x, to.z - at.z);
            let left = (dx * dx + dz * dz).sqrt();
            if left < 0.3 {
                break;
            }
            let step = t.player_step(at, dx / left * 0.25, dz / left * 0.25);
            let r = o.resolve(xz(at), xz(step.pos), BODY_RADIUS);
            let next = t.ground_vec(Vec3f {
                x: r.x,
                y: 0.0,
                z: r.z,
            });
            if next.xz_distance_to(at) < 1e-4 {
                break;
            }
            at = next;
        }
        xz(at)
    };
    // The town's wall is a square 90 m from the middle (ZoneLayout.TOWN_HALF),
    // a gate in the middle of each side.
    let wall = 90.0;
    for (name, out) in [
        ("east", (1.0f32, 0.0f32)),
        ("west", (-1.0, 0.0)),
        ("south", (0.0, 1.0)),
        ("north", (0.0, -1.0)),
    ] {
        let inside = P2::new(out.0 * (wall - 25.0), out.1 * (wall - 25.0));
        let outside = P2::new(out.0 * (wall + 25.0), out.1 * (wall + 25.0));
        let reached = walk(inside, outside);
        assert!(
            reached.dist_to(outside) < 0.5,
            "the {name} gate is shut going out: stopped at {reached:?}"
        );
        let back = walk(outside, inside);
        assert!(
            back.dist_to(inside) < 0.5,
            "the {name} gate is shut coming in: stopped at {back:?}"
        );
        // 30 m along the wall from the gate: no way through.
        let side = (out.1, out.0);
        let a = P2::new(outside.x + side.0 * 30.0, outside.z + side.1 * 30.0);
        let b = P2::new(inside.x + side.0 * 30.0, inside.z + side.1 * 30.0);
        let held = walk(a, b);
        let from_middle = held.x * out.0 + held.z * out.1;
        assert!(
            from_middle > wall,
            "a body walked through the {name} wall: reached {held:?}"
        );
    }
}

/// No ground a body can be put on is closed off. Thousands of scattered
/// trees, bushes and rocks close a few gaps a body fits in and cannot leave;
/// the placement rule refuses those (`Obstacles::stands`), so they hold
/// nobody. What must never exist is a pocket the rule would use: closed
/// ground big enough to walk about in (a fence all the way round a field, a
/// ring of stones with no way in), where a body put there, or a mob spawned
/// there, is out of the world for good.
#[test]
fn nothing_in_the_zone_closes_a_pocket_a_body_could_be_put_in() {
    let o = compiled();
    let prisms = walk_prisms(o);
    let groups = clusters(&prisms);
    let mut found = Vec::new();
    let mut gaps = 0;
    for members in &groups {
        for cells in pockets_of(&prisms, members) {
            // A pocket smaller across than the walk `can_leave` asks for is
            // safe either way: a cell the rule would use is one a body walked
            // `ESCAPE_NEED` away from, so out of the pocket (the search's
            // cells are coarser than the tightest squeeze a body fits
            // through), and a cell it would not use holds nobody.
            let (mut lo, mut hi) = (cells[0], cells[0]);
            for c in &cells {
                lo = P2::new(lo.x.min(c.x), lo.z.min(c.z));
                hi = P2::new(hi.x.max(c.x), hi.z.max(c.z));
            }
            let usable = cells.iter().find(|c| o.stands(**c, BODY_RADIUS));
            match usable {
                Some(at) if lo.dist_to(hi) >= super::obstacles::ESCAPE_NEED => found.push(format!(
                    "{:.1} m2 at ({:.1}, {:.1}) in a cluster of {}",
                    cells.len() as f32 * POCKET_CELL * POCKET_CELL,
                    at.x,
                    at.z,
                    members.len()
                )),
                _ => gaps += 1,
            }
        }
    }
    println!(
        "{} solids stand in a body's way, in {} clusters of two or more; \
         {gaps} gaps too small to hold anyone",
        prisms.len(),
        groups.len()
    );
    assert!(found.is_empty(), "{} pockets: {found:#?}", found.len());
}

/// The pocket search finds a pocket when there is one (four walls with no
/// door) and none when a door is cut.
#[test]
fn the_pocket_search_finds_a_closed_yard_and_passes_an_open_one() {
    let closed = vec![
        Prism::rect(0.0, 0.0, 10.0, 0.5, 0.0, 3.0),
        Prism::rect(0.0, 9.5, 10.0, 10.0, 0.0, 3.0),
        Prism::rect(0.0, 0.0, 0.5, 10.0, 0.0, 3.0),
        Prism::rect(9.5, 0.0, 10.0, 10.0, 0.0, 3.0),
    ];
    let o = Obstacles::from_prisms(&closed).unwrap();
    let groups = clusters(&closed);
    assert_eq!(groups.len(), 1);
    let pockets = pockets_of(&closed, &groups[0]);
    assert_eq!(pockets.len(), 1, "the yard is one pocket");
    assert!(pockets[0].len() as f32 * POCKET_CELL * POCKET_CELL > 50.0);
    assert!(
        pockets[0].iter().any(|c| o.stands(*c, BODY_RADIUS)),
        "and a body could be put in it"
    );

    let mut open = closed.clone();
    // The east wall stops 2 m short: a door.
    open[3] = Prism::rect(9.5, 0.0, 10.0, 8.0, 0.0, 3.0);
    let groups = clusters(&open);
    let pockets: Vec<_> = groups.iter().flat_map(|m| pockets_of(&open, m)).collect();
    assert!(pockets.is_empty(), "a yard with a door holds no pocket");
}
