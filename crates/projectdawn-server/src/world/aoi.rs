//! Area-of-Interest grid for spatial broadcast filtering.
//!
//! The world is partitioned into square cells of side [`CELL_SIZE`] metres.
//! Each entity (player, enemy, loot bag) lives in exactly one cell at any
//! given moment. A player can see — and therefore should receive broadcasts
//! about — every entity whose cell is within the 3×3 neighbourhood centred
//! on the player's own cell. That gives a square visibility area of
//! 3 × CELL_SIZE per side (~360 m at the default 120 m cell).
//!
//! # Design choices
//!
//! - Entity ids are `u64` throughout (`char_id` for players cast to u64,
//!   `EntityId` for enemies and bags — all fit in the same namespace because
//!   enemy ids start at `ENEMY_ID_BASE` and bag ids above that).
//! - The grid is sparse: only occupied cells allocate storage.
//! - `update` returns the gained and lost cell lists so the caller can drive
//!   the spawn/despawn fan-out without re-querying the grid.

use std::collections::{HashMap, HashSet};

use protocol::world::EntityId;

/// Side length of one grid cell in metres. All entities within the 3×3
/// neighbourhood of cells centred on a player's cell are visible to that
/// player (and receive that player's position broadcasts).
pub const CELL_SIZE: f32 = 120.0;

/// A 2-D integer cell coordinate derived from a world XZ position.
pub type Cell = (i32, i32);

/// Returns the cell that contains world position `(x, z)`.
#[inline]
pub fn cell_for(x: f32, z: f32) -> Cell {
    (x.div_euclid(CELL_SIZE) as i32, z.div_euclid(CELL_SIZE) as i32)
}

/// Returns the 9 cells in the 3×3 neighbourhood centred on `cell`
/// (including `cell` itself).
pub fn neighbors(cell: Cell) -> [Cell; 9] {
    let (cx, cz) = cell;
    [
        (cx - 1, cz - 1), (cx, cz - 1), (cx + 1, cz - 1),
        (cx - 1, cz    ), (cx, cz    ), (cx + 1, cz    ),
        (cx - 1, cz + 1), (cx, cz + 1), (cx + 1, cz + 1),
    ]
}

/// Spatial index mapping cells to the set of entity ids currently in them.
#[derive(Default)]
pub struct AoiGrid {
    cells: HashMap<Cell, HashSet<EntityId>>,
}

impl AoiGrid {
    pub fn new() -> Self {
        Self::default()
    }

    /// Place `id` into `cell`. No-op if already present.
    pub fn insert(&mut self, id: EntityId, cell: Cell) {
        self.cells.entry(cell).or_default().insert(id);
    }

    /// Remove `id` from `cell`. Drops the cell entry when it becomes empty.
    pub fn remove(&mut self, id: EntityId, cell: Cell) {
        if let Some(set) = self.cells.get_mut(&cell) {
            set.remove(&id);
            if set.is_empty() {
                self.cells.remove(&cell);
            }
        }
    }

    /// Move `id` from `old_cell` to `new_cell`. Returns `(gained, lost)`:
    /// - `gained` — cells that are in the new neighbourhood but not the old
    /// - `lost`   — cells that were in the old neighbourhood but not the new
    ///
    /// The caller uses these sets to determine which entities become newly
    /// visible (and need an EntitySpawn fan-out) and which leave visibility
    /// (and need an EntityDespawn fan-out).
    ///
    /// Returns `([], [])` when `old_cell == new_cell` (common case — avoids
    /// spurious work on every tick for stationary entities).
    pub fn update(&mut self, id: EntityId, old_cell: Cell, new_cell: Cell) -> (Vec<Cell>, Vec<Cell>) {
        if old_cell == new_cell {
            return (Vec::new(), Vec::new());
        }
        self.remove(id, old_cell);
        self.insert(id, new_cell);

        let old_nbrs: HashSet<Cell> = neighbors(old_cell).into_iter().collect();
        let new_nbrs: HashSet<Cell> = neighbors(new_cell).into_iter().collect();

        let gained: Vec<Cell> = new_nbrs.difference(&old_nbrs).copied().collect();
        let lost: Vec<Cell> = old_nbrs.difference(&new_nbrs).copied().collect();
        (gained, lost)
    }

    /// Returns the set of all entity ids visible from `cell` — i.e. every
    /// entity in any of the 9 cells in the 3×3 neighbourhood (including
    /// `cell` itself).
    pub fn entities_visible_from(&self, cell: Cell) -> HashSet<EntityId> {
        let mut out = HashSet::new();
        for nbr in neighbors(cell) {
            if let Some(set) = self.cells.get(&nbr) {
                out.extend(set);
            }
        }
        out
    }

    /// Returns all entity ids in exactly the cells in `cells` (no
    /// neighbourhood expansion). Used when the caller has already computed
    /// the relevant cells (e.g. the gained/lost sets from `update`).
    pub fn entities_in_cells<'a>(&self, cells: impl IntoIterator<Item = &'a Cell>) -> HashSet<EntityId> {
        let mut out = HashSet::new();
        for cell in cells {
            if let Some(set) = self.cells.get(cell) {
                out.extend(set);
            }
        }
        out
    }

    /// True if `observer` can see `target` — i.e. `target`'s cell is in
    /// the 3×3 neighbourhood of `observer`'s cell.
    pub fn can_see(&self, observer_cell: Cell, target_cell: Cell) -> bool {
        let (ox, oz) = observer_cell;
        let (tx, tz) = target_cell;
        (ox - tx).abs() <= 1 && (oz - tz).abs() <= 1
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── cell_for ─────────────────────────────────────────────────────────────

    #[test]
    fn cell_for_origin() {
        assert_eq!(cell_for(0.0, 0.0), (0, 0));
    }

    #[test]
    fn cell_for_mid_cell() {
        assert_eq!(cell_for(60.0, 60.0), (0, 0));
        assert_eq!(cell_for(119.9, 119.9), (0, 0));
    }

    #[test]
    fn cell_for_next_cell() {
        assert_eq!(cell_for(120.0, 0.0), (1, 0));
        assert_eq!(cell_for(0.0, 120.0), (0, 1));
    }

    #[test]
    fn cell_for_negative() {
        // div_euclid maps negative x into the cell below 0, not truncation.
        assert_eq!(cell_for(-1.0, 0.0), (-1, 0));
        assert_eq!(cell_for(-120.0, -120.0), (-1, -1));
        assert_eq!(cell_for(-121.0, 0.0), (-2, 0));
    }

    // ── neighbors ────────────────────────────────────────────────────────────

    #[test]
    fn neighbors_count() {
        assert_eq!(neighbors((0, 0)).len(), 9);
    }

    #[test]
    fn neighbors_includes_self() {
        let nbrs = neighbors((3, 5));
        assert!(nbrs.contains(&(3, 5)));
    }

    #[test]
    fn neighbors_span() {
        let nbrs = neighbors((0, 0));
        let set: HashSet<Cell> = nbrs.into_iter().collect();
        for dx in -1i32..=1 {
            for dz in -1i32..=1 {
                assert!(set.contains(&(dx, dz)), "missing ({dx},{dz})");
            }
        }
    }

    #[test]
    fn neighbors_are_unique() {
        let nbrs = neighbors((7, -3));
        let set: HashSet<Cell> = nbrs.into_iter().collect();
        assert_eq!(set.len(), 9);
    }

    // ── AoiGrid insert / remove ───────────────────────────────────────────────

    #[test]
    fn insert_and_visible() {
        let mut g = AoiGrid::new();
        g.insert(1, (0, 0));
        let vis = g.entities_visible_from((0, 0));
        assert!(vis.contains(&1));
    }

    #[test]
    fn remove_cleans_up_empty_cell() {
        let mut g = AoiGrid::new();
        g.insert(1, (0, 0));
        g.remove(1, (0, 0));
        assert!(g.cells.is_empty());
    }

    #[test]
    fn remove_nonexistent_is_noop() {
        let mut g = AoiGrid::new();
        g.remove(99, (5, 5)); // must not panic
    }

    #[test]
    fn entities_in_adjacent_cell_are_visible() {
        let mut g = AoiGrid::new();
        g.insert(42, (1, 0)); // one cell right of (0,0)
        let vis = g.entities_visible_from((0, 0));
        assert!(vis.contains(&42));
    }

    #[test]
    fn entity_two_cells_away_is_not_visible() {
        let mut g = AoiGrid::new();
        g.insert(7, (2, 0)); // two cells right — outside 3×3
        let vis = g.entities_visible_from((0, 0));
        assert!(!vis.contains(&7));
    }

    #[test]
    fn visibility_is_symmetric() {
        let mut g = AoiGrid::new();
        g.insert(1, (0, 0));
        g.insert(2, (1, 1));
        // Both cells are within each other's 3×3 neighbourhood.
        assert!(g.entities_visible_from((0, 0)).contains(&2));
        assert!(g.entities_visible_from((1, 1)).contains(&1));
    }

    // ── AoiGrid update ────────────────────────────────────────────────────────

    #[test]
    fn update_same_cell_returns_empty_diff() {
        let mut g = AoiGrid::new();
        g.insert(1, (0, 0));
        let (gained, lost) = g.update(1, (0, 0), (0, 0));
        assert!(gained.is_empty());
        assert!(lost.is_empty());
    }

    #[test]
    fn update_moves_entity_to_new_cell() {
        let mut g = AoiGrid::new();
        g.insert(1, (0, 0));
        g.update(1, (0, 0), (5, 5));
        // Entity no longer in (0,0).
        assert!(!g.entities_visible_from((0, 0)).contains(&1));
        // Entity now in (5,5).
        assert!(g.entities_visible_from((5, 5)).contains(&1));
    }

    #[test]
    fn update_diff_gained_minus_lost_is_9() {
        // Moving from (0,0) to (10,10) — neighbourhoods are fully disjoint.
        let mut g = AoiGrid::new();
        g.insert(1, (0, 0));
        let (gained, lost) = g.update(1, (0, 0), (10, 10));
        assert_eq!(gained.len(), 9, "should gain 9 entirely new cells");
        assert_eq!(lost.len(), 9, "should lose 9 entirely old cells");
    }

    #[test]
    fn update_diff_one_step_overlap() {
        // Moving one cell right: (0,0) → (1,0). Neighbourhoods share 6 cells;
        // 3 gained on the right, 3 lost on the left.
        let mut g = AoiGrid::new();
        g.insert(1, (0, 0));
        let (gained, lost) = g.update(1, (0, 0), (1, 0));
        assert_eq!(gained.len(), 3);
        assert_eq!(lost.len(), 3);
    }

    // ── can_see ───────────────────────────────────────────────────────────────

    #[test]
    fn can_see_same_cell() {
        let g = AoiGrid::new();
        assert!(g.can_see((0, 0), (0, 0)));
    }

    #[test]
    fn can_see_adjacent() {
        let g = AoiGrid::new();
        assert!(g.can_see((0, 0), (1, 1)));
        assert!(g.can_see((0, 0), (-1, -1)));
    }

    #[test]
    fn cannot_see_two_away() {
        let g = AoiGrid::new();
        assert!(!g.can_see((0, 0), (2, 0)));
        assert!(!g.can_see((0, 0), (0, 2)));
    }
}
