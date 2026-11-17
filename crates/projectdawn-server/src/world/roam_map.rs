//! The roam map: the ground a wandering animal may stand on (wandering
//! wildlife, `docs/design/wandering_wildlife.md` in the client repo).
//!
//! Built once at boot from the server's terrain: the floor triangles a body
//! can walk to from the town without ever crossing a face, in either
//! direction. A mob never falls and the slope rule refuses only a step that
//! rises onto a face, so a mob could walk DOWN a cliff into a pocket it can
//! never leave; an animal confined to this map cannot. It is the one answer
//! to three questions: where a wanderer may appear, where a leg may end, and
//! whether a roam step may be taken.
//!
//! The triangles are the client collider's own, as `Terrain` samples them: two
//! a cell, the lower one on the cell's first corner, cut from `(x1, z0)` to
//! `(x0, z1)`. A floor is ground no steeper than the client's floor angle
//! (`fall::is_floor`, 55 degrees), stricter than the slope rule's 60, so a
//! walk between two neighbouring floors is always a step the slope rule takes.
//!
//! The test-only flood in `terrain_content.rs` answers a different question
//! (where can a PLAYER get to, falls included) and reaches faces and one-way
//! pockets; a test here holds that every roamable triangle is one of its.

use super::{fall, terrain::Terrain};
use std::collections::VecDeque;

#[derive(Debug, Clone)]
pub struct RoamMap {
    /// Cells along one side.
    gs: usize,
    cell: f32,
    /// World x and z of the grid's first vertex.
    min_x: f32,
    min_z: f32,
    /// One flag a triangle: index `(cell z * gs + cell x) * 2`, plus 1 for the
    /// upper one.
    roamable: Vec<bool>,
    /// What stands on the ground (`obstacles.rs`): a wanderer appears only
    /// where a body can be put (`spot_ok`).
    obstacles: std::sync::Arc<crate::world::obstacles::Obstacles>,
}

impl RoamMap {
    /// Flood the floors of `terrain` from the triangle under `from` (the
    /// town), over shared edges only. A `from` that is off the grid or on a
    /// face gives an empty map: nothing roams.
    pub fn build(terrain: &Terrain, from: (f32, f32)) -> Self {
        let gs = terrain.grid_size();
        let cell = terrain.cell_size();
        let (min_x, min_z) = terrain.min_corner();
        let mut map = Self {
            gs,
            cell,
            min_x,
            min_z,
            roamable: vec![false; gs * gs * 2],
            obstacles: terrain.obstacles_handle(),
        };
        let floor: Vec<bool> = (0..gs * gs * 2)
            .map(|tri| {
                let (x, z) = map.centre(tri);
                fall::is_floor(terrain.gradient(x, z))
            })
            .collect();
        let Some(start) = map.triangle_at(from.0, from.1) else {
            return map;
        };
        if !floor[start] {
            return map;
        }
        let mut queue = VecDeque::from([start]);
        map.roamable[start] = true;
        while let Some(tri) = queue.pop_front() {
            for next in map.edge_neighbours(tri).into_iter().flatten() {
                if floor[next] && !map.roamable[next] {
                    map.roamable[next] = true;
                    queue.push_back(next);
                }
            }
        }
        map
    }

    /// The triangle under a point, or `None` off the grid (and for a point
    /// that is not finite). The same split as `Terrain`: the far edge belongs
    /// to the last cell, the diagonal to the lower triangle.
    fn triangle_at(&self, x: f32, z: f32) -> Option<usize> {
        let extent = self.gs as f32 * self.cell;
        let (lx, lz) = (x - self.min_x, z - self.min_z);
        if !(lx >= 0.0 && lx <= extent && lz >= 0.0 && lz <= extent) {
            return None;
        }
        let (fx, fz) = (lx / self.cell, lz / self.cell);
        let cx = (fx.floor() as usize).min(self.gs - 1);
        let cz = (fz.floor() as usize).min(self.gs - 1);
        let upper = usize::from((fx - cx as f32) + (fz - cz as f32) > 1.0);
        Some((cz * self.gs + cx) * 2 + upper)
    }

    /// The middle of a triangle, in world x and z.
    fn centre(&self, tri: usize) -> (f32, f32) {
        let (cell, upper) = (tri / 2, tri % 2 == 1);
        let (cx, cz) = ((cell % self.gs) as f32, (cell / self.gs) as f32);
        // Lower: corners (0,0) (1,0) (0,1); upper: (1,0) (1,1) (0,1).
        let third = if upper { 2.0 / 3.0 } else { 1.0 / 3.0 };
        (
            self.min_x + (cx + third) * self.cell,
            self.min_z + (cz + third) * self.cell,
        )
    }

    /// The triangles that share an edge with this one (up to three; fewer at
    /// the grid's rim). A lower triangle meets its cell's upper one across the
    /// diagonal, the upper one of the cell before it in z across its z0 edge
    /// and of the cell before it in x across its x0 edge; an upper triangle
    /// the mirror of that.
    fn edge_neighbours(&self, tri: usize) -> [Option<usize>; 3] {
        let (cell, upper) = (tri / 2, tri % 2 == 1);
        let (cx, cz) = (cell % self.gs, cell / self.gs);
        let at = |x: usize, z: usize, up: usize| (z * self.gs + x) * 2 + up;
        if upper {
            [
                Some(at(cx, cz, 0)),
                (cx + 1 < self.gs).then(|| at(cx + 1, cz, 0)),
                (cz + 1 < self.gs).then(|| at(cx, cz + 1, 0)),
            ]
        } else {
            [
                Some(at(cx, cz, 1)),
                (cx > 0).then(|| at(cx - 1, cz, 1)),
                (cz > 0).then(|| at(cx, cz - 1, 1)),
            ]
        }
    }

    /// Whether a wanderer may stand at this point.
    pub fn roamable(&self, x: f32, z: f32) -> bool {
        self.triangle_at(x, z).is_some_and(|tri| self.roamable[tri])
    }

    /// Whether a wanderer may appear at this point: roamable ground, clear of
    /// every tree, rock and wall, and not a gap closed all round
    /// (`Obstacles::stands`).
    pub fn spot_ok(&self, x: f32, z: f32) -> bool {
        self.roamable(x, z)
            && self.obstacles.stands(
                crate::world::obstacles::P2::new(x, z),
                crate::world::obstacles::BODY_RADIUS,
            )
    }

    /// The roamable share of the whole grid, for the boot line.
    pub fn share(&self) -> f32 {
        if self.roamable.is_empty() {
            return 0.0;
        }
        self.roamable.iter().filter(|r| **r).count() as f32 / self.roamable.len() as f32
    }

    /// The roamable area, in square metres, whose triangle middles pass
    /// `inside`: how much room a region has for its animals.
    pub fn area_where(&self, inside: impl Fn(f32, f32) -> bool) -> f32 {
        let tri_area = self.cell * self.cell * 0.5;
        (0..self.roamable.len())
            .filter(|tri| self.roamable[*tri])
            .filter(|tri| {
                let (x, z) = self.centre(*tri);
                inside(x, z)
            })
            .count() as f32
            * tri_area
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A 6 x 6 grid of 10 m cells centred on the origin (x and z from -30 to
    /// 30) with a wall across it: the vertex row at z = 10 raised `wall`
    /// metres, so the cells either side of that row are faces.
    fn walled(wall: f32) -> Terrain {
        let gs = 6;
        let mut heights = vec![0.0; (gs + 1) * (gs + 1)];
        for ix in 0..=gs {
            heights[4 * (gs + 1) + ix] = wall;
        }
        Terrain::from_heights(gs, 10.0, [0.0, 0.0, 0.0], 0.0, heights).expect("a valid grid")
    }

    #[test]
    fn a_flat_plate_is_roamable_everywhere_and_nowhere_off_it() {
        let map = RoamMap::build(&Terrain::flat(20, 20.0, 0.0), (0.0, 0.0));
        assert_eq!(map.share(), 1.0);
        assert!(map.roamable(0.0, 0.0));
        assert!(map.roamable(199.0, -199.0));
        assert!(
            map.roamable(200.0, 200.0),
            "the far edge is the last cell's"
        );
        assert!(!map.roamable(200.5, 0.0), "off the plate");
        assert!(!map.roamable(f32::NAN, 0.0));
        assert!((map.area_where(|_, _| true) - 400.0 * 400.0).abs() < 1.0);
    }

    /// A wall no body climbs cuts the plate in two: the flood from the town
    /// side never reaches the far side, and the wall's own triangles are not
    /// roamable from either side. A wall low enough to walk over joins them.
    #[test]
    fn a_face_is_never_roamable_and_the_ground_behind_it_is_not_reached() {
        let map = RoamMap::build(&walled(40.0), (0.0, -20.0));
        assert!(map.roamable(0.0, -20.0), "the town side");
        assert!(map.roamable(-25.0, -5.0));
        assert!(!map.roamable(0.0, 5.0), "the wall's near face");
        assert!(!map.roamable(0.0, 15.0), "the wall's far face");
        assert!(!map.roamable(0.0, 25.0), "floor, but behind the wall");

        let low = RoamMap::build(&walled(3.0), (0.0, -20.0));
        assert!(low.roamable(0.0, 5.0) && low.roamable(0.0, 25.0));
    }

    /// One-way ground is left out: a pit a body could walk down into (the
    /// slope rule refuses only a rising step) but never out of.
    #[test]
    fn a_pit_reachable_only_downhill_is_not_roamable() {
        // The middle vertex of a 4 x 4 grid dropped 30 m: the eight triangles
        // round it are faces, and there is no floor at its bottom at all; with
        // the four middle vertices dropped, the middle cell is a floor ringed
        // by faces.
        let gs = 6;
        let mut heights = vec![40.0; (gs + 1) * (gs + 1)];
        for iz in 3..=4 {
            for ix in 3..=4 {
                heights[iz * (gs + 1) + ix] = 0.0;
            }
        }
        let t = Terrain::from_heights(gs, 10.0, [0.0, 0.0, 0.0], 0.0, heights).expect("grid");
        let map = RoamMap::build(&t, (-25.0, -25.0));
        assert!(map.roamable(-25.0, -25.0), "the rim");
        assert!(
            fall::is_floor(t.gradient(5.0, 5.0)),
            "the pit's bottom is a floor"
        );
        assert!(
            !map.roamable(5.0, 5.0),
            "and no walk reaches it without crossing a face"
        );
    }

    #[test]
    fn a_start_on_a_face_or_off_the_grid_roams_nowhere() {
        assert_eq!(RoamMap::build(&walled(40.0), (0.0, 5.0)).share(), 0.0);
        assert_eq!(RoamMap::build(&walled(40.0), (500.0, 0.0)).share(), 0.0);
    }

    /// Every triangle is in exactly the neighbour lists of the triangles in
    /// its own list (edges are shared both ways), and no list names a triangle
    /// off the grid.
    #[test]
    fn edge_neighbours_are_mutual() {
        let map = RoamMap::build(&Terrain::flat(5, 10.0, 0.0), (0.0, 0.0));
        for tri in 0..5 * 5 * 2 {
            for n in map.edge_neighbours(tri).into_iter().flatten() {
                assert!(n < 5 * 5 * 2);
                assert!(
                    map.edge_neighbours(n).contains(&Some(tri)),
                    "{tri} lists {n}, which does not list it back"
                );
            }
        }
    }

    /// The point lookup agrees with the terrain's own triangle split: a point
    /// just either side of a cell's diagonal reads the gradient of the
    /// triangle the map says it is in.
    #[test]
    fn the_point_lookup_uses_the_terrains_triangles() {
        let t = walled(40.0);
        let map = RoamMap::build(&t, (0.0, -20.0));
        for i in 0..2000 {
            let x = -30.0 + (i as f32 * 0.7919) % 60.0;
            let z = -30.0 + (i as f32 * 1.3217) % 60.0;
            let tri = map.triangle_at(x, z).expect("on the grid");
            let (cx, cz) = map.centre(tri);
            assert_eq!(
                t.gradient(x, z),
                t.gradient(cx, cz),
                "({x}, {z}) is not in the triangle whose middle is ({cx}, {cz})"
            );
        }
    }
}
