//! Wandering wildlife: animals that live in a region instead of at a spawn
//! point (`docs/design/wandering_wildlife.md` in the client repo). This module
//! is the data and the placement rules: the roam table
//! (`data/zone_roam.toml`), a region, where a wanderer may appear and where a
//! leg of its walk may end. The spawner (`spawn_points.rs`) fills the slots,
//! and `Entity::tick_roam` walks the legs.
//!
//! Everything here is decided by the server from its own data and its own
//! ground; a client only ever sees an ordinary enemy that moves.
//!
//! Coordinates are WORLD coordinates: x east, z south (north is -z, the
//! compass mirror of 2026-11-14).

use super::{connection::Vec3f, roam_map::RoamMap, zones::MobTemplate};
use rand::Rng;
use serde::Deserialize;
use std::time::Instant;

const ZONE_ROAM_TOML: &str = include_str!("../../data/zone_roam.toml");

/// No wanderer chooses ground this close to the town's middle (the origin):
/// not a spawn spot, not the end of a leg, and no leg's line crosses the
/// circle. The flat town's edge on the 4 km plate; past the 60 m safe area
/// plus any aggro radius in the table (a test holds that).
pub const ROAM_TOWN_CLEARANCE: f32 = 150.0;
/// A wanderer never appears this close to a player's body (living, dead or
/// linkdead): past every aggro radius, so nothing pops onto a fight, and a
/// camper cannot hold a spawn point.
pub const ROAM_POP_CLEARANCE: f32 = 60.0;
/// A leg of a wanderer's walk, in metres.
pub const ROAM_LEG_MIN: f32 = 20.0;
pub const ROAM_LEG_MAX: f32 = 80.0;
/// A wanderer ambles at this share of its speed.
pub const ROAM_SPEED_MULT: f32 = 0.4;
/// It stands this long between legs, in seconds.
pub const ROAM_PAUSE_MIN: f32 = 5.0;
pub const ROAM_PAUSE_MAX: f32 = 25.0;
/// A slot that found no spot this tick tries again after this long.
pub const ROAM_RETRY_SECS: f32 = 1.0;
/// A respawn timer is rolled this far either side of the slot's `respawn`.
pub const ROAM_RESPAWN_JITTER: f32 = 0.25;
/// A pack's members appear within this of their leader's spot.
pub const PACK_SPREAD: f32 = 5.0;
/// A follower stands once it is this close to its leader (on the ground).
pub const PACK_FOLLOW_DISTANCE: f32 = 4.0;
/// Further than this from its leader a follower runs instead of ambling.
pub const PACK_CATCHUP_DISTANCE: f32 = 12.0;
/// A packmate this close to a member's fight joins it (if its own leash
/// reaches the target).
pub const PACK_SOCIAL_RANGE: f32 = 30.0;
/// A follower that cannot walk straight to its leader roams on its own legs
/// for this long before it tries again.
pub const PACK_SOLO_SECS: f32 = 30.0;
/// The hate a packmate takes for a target it joins a fight against: enough to
/// be on its list, less than any real hit.
pub const PACK_JOIN_THREAT: f32 = 1.0;

/// Where a wanderer lives: a circle, optionally cut down to a ring round the
/// town and to a band of z (so a region can stay on one side of the ridge).
#[derive(Debug, Clone, Copy, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Region {
    /// World x and z of the circle's middle.
    pub center: [f32; 2],
    pub radius: f32,
    /// Nothing nearer the town's middle than this is in the region (the
    /// level bands follow the camp rings). The town clearance always applies.
    #[serde(default)]
    pub min_from_town: f32,
    /// Only ground with z at least this (south of this line).
    #[serde(default)]
    pub z_min: Option<f32>,
    /// Only ground with z at most this (north of this line).
    #[serde(default)]
    pub z_max: Option<f32>,
}

impl Region {
    /// Keep-only-when-inside, so a point that is not finite is outside.
    pub fn contains(&self, x: f32, z: f32) -> bool {
        let (dx, dz) = (x - self.center[0], z - self.center[1]);
        let from_town = (x * x + z * z).sqrt();
        (dx * dx + dz * dz).sqrt() <= self.radius
            && from_town >= self.min_from_town.max(ROAM_TOWN_CLEARANCE)
            && self.z_min.is_none_or(|lo| z >= lo)
            && self.z_max.is_none_or(|hi| z <= hi)
    }
}

/// One roam slot as authored: an animal (or a pack) that lives in a region.
#[derive(Debug, Clone)]
pub struct RoamDef {
    pub mob: MobTemplate,
    pub region: Region,
    /// Members in the slot: 1 for a lone animal.
    pub pack: usize,
    pub respawn_secs: f32,
    /// Alive only at night (`clock::is_night`).
    pub night_only: bool,
}

#[derive(Debug, Deserialize)]
struct RoamFile {
    #[serde(default, rename = "roam")]
    roams: Vec<RawRoam>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawRoam {
    #[allow(dead_code)] // for the file's reader
    desc: String,
    mob: MobTemplate,
    region: Region,
    /// How many slots (for a pack: how many packs).
    count: usize,
    #[serde(default = "one")]
    pack: usize,
    respawn: f32,
    /// "night" for a night-only slot; absent for any time.
    #[serde(default)]
    time: Option<String>,
}

fn one() -> usize {
    1
}

/// Parse a roam table and flatten it to one `RoamDef` a slot, in file order
/// (so slot indices are stable across restarts). Refuses data that would
/// misbehave rather than run it.
pub fn parse(text: &str) -> Result<Vec<RoamDef>, String> {
    let file: RoamFile = toml::from_str(text).map_err(|e| e.to_string())?;
    let mut out = Vec::new();
    for raw in file.roams {
        let name = raw.mob.name.clone();
        if raw.count == 0 || raw.pack == 0 {
            return Err(format!("{name}: count and pack must be at least 1"));
        }
        if !(raw.respawn.is_finite() && raw.respawn > 0.0) {
            return Err(format!("{name}: respawn must be a positive number"));
        }
        let r = &raw.region;
        if !(r.radius.is_finite() && r.radius > 0.0 && r.center.iter().all(|c| c.is_finite())) {
            return Err(format!(
                "{name}: the region needs a finite centre and radius"
            ));
        }
        let night_only = match raw.time.as_deref() {
            None => false,
            Some("night") => true,
            Some(other) => return Err(format!("{name}: time = {other:?}; only \"night\" exists")),
        };
        // A passive animal (aggro 0) has the leash it is given and no floor
        // under it (`Entity::leash_range`): without one it could be hit from
        // anywhere and never turn.
        if raw.mob.aggro <= 0.0 && raw.mob.leash.is_none_or(|l| l < super::MIN_LEASH_RANGE) {
            return Err(format!(
                "{name}: a passive mob (aggro 0) needs leash >= {}",
                super::MIN_LEASH_RANGE
            ));
        }
        for _ in 0..raw.count {
            out.push(RoamDef {
                mob: raw.mob.clone(),
                region: raw.region,
                pack: raw.pack,
                respawn_secs: raw.respawn,
                night_only,
            });
        }
    }
    Ok(out)
}

/// The zone's roam table (`data/zone_roam.toml`), or the table a test hands in
/// (`Config::roam_table`). A table that does not parse is a bug in shipped
/// data, as for the camps: it stops the boot.
pub fn load(table: Option<&str>) -> Vec<RoamDef> {
    let text = table.unwrap_or(ZONE_ROAM_TOML);
    parse(text).unwrap_or_else(|e| panic!("zone_roam.toml must be valid; fix the roam table: {e}"))
}

/// What a wanderer carries: where it lives, and where it is in its walk.
#[derive(Debug, Clone, Copy)]
pub struct Roam {
    pub region: Region,
    pub night_only: bool,
    /// One of a pack (the slot holds more than one).
    pub pack: bool,
    /// Standing until then, between legs.
    pub pause_until: Option<Instant>,
    /// Where the current leg ends (world x and z).
    pub goal: Option<(f32, f32)>,
    /// A follower walking its own legs until then (`PACK_SOLO_SECS`).
    pub solo_until: Option<Instant>,
}

impl Roam {
    pub fn of(def: &RoamDef) -> Self {
        Self {
            region: def.region,
            night_only: def.night_only,
            pack: def.pack > 1,
            pause_until: None,
            goal: None,
            solo_until: None,
        }
    }

    /// The same animal with its walk forgotten (a fight began, a charm ended).
    pub fn rested(self) -> Self {
        Self {
            pause_until: None,
            goal: None,
            solo_until: None,
            ..self
        }
    }
}

/// Whether the straight line from `a` to `b` stays out of the town's circle.
pub fn segment_clears_town(a: (f32, f32), b: (f32, f32)) -> bool {
    let (dx, dz) = (b.0 - a.0, b.1 - a.1);
    let len2 = dx * dx + dz * dz;
    // The point of the segment nearest the origin.
    let t = if len2 > 0.0 {
        (-(a.0 * dx + a.1 * dz) / len2).clamp(0.0, 1.0)
    } else {
        0.0
    };
    let (nx, nz) = (a.0 + dx * t, a.1 + dz * t);
    (nx * nx + nz * nz).sqrt() >= ROAM_TOWN_CLEARANCE
}

/// Whether a point is `ROAM_POP_CLEARANCE` clear of every body, on the ground.
pub fn clear_of_bodies(x: f32, z: f32, bodies: &[Vec3f]) -> bool {
    bodies.iter().all(|b| {
        let (dx, dz) = (b.x - x, b.z - z);
        (dx * dx + dz * dz).sqrt() >= ROAM_POP_CLEARANCE
    })
}

/// A spot a wanderer may stand on: on the roam map and inside its region
/// (which is never inside the town clearance).
fn standable(map: &RoamMap, region: &Region, x: f32, z: f32) -> bool {
    region.contains(x, z) && map.roamable(x, z)
}

/// A random spot in `region` for a wanderer to appear at: on the roam map,
/// outside the town clearance and clear of every body. `None` when 32 draws
/// found none (the slot tries again shortly).
pub fn pick_spot(
    map: &RoamMap,
    region: &Region,
    bodies: &[Vec3f],
    rng: &mut impl Rng,
) -> Option<(f32, f32)> {
    // Nothing nearer the circle's middle than this can be far enough from the
    // town, so the draws skip that disc (for a ring about the town itself,
    // exactly the hole in it).
    let centre_from_town = (region.center[0].powi(2) + region.center[1].powi(2)).sqrt();
    let inner = (region.min_from_town.max(ROAM_TOWN_CLEARANCE) - centre_from_town)
        .clamp(0.0, region.radius);
    for _ in 0..32 {
        // Uniform over the area between `inner` and the circle's edge.
        let u = rng.gen_range(0.0_f32..=1.0);
        let r = (inner * inner + (region.radius * region.radius - inner * inner) * u).sqrt();
        let a = rng.gen_range(0.0..std::f32::consts::TAU);
        let (x, z) = (
            region.center[0] + r * a.cos(),
            region.center[1] + r * a.sin(),
        );
        if standable(map, region, x, z) && clear_of_bodies(x, z, bodies) {
            return Some((x, z));
        }
    }
    None
}

/// A spot within `PACK_SPREAD` of `anchor` for a pack member, by the same
/// rules as `pick_spot`.
pub fn pick_near(
    map: &RoamMap,
    region: &Region,
    anchor: (f32, f32),
    bodies: &[Vec3f],
    rng: &mut impl Rng,
) -> Option<(f32, f32)> {
    for _ in 0..16 {
        let r = PACK_SPREAD * rng.gen_range(0.2_f32..=1.0);
        let a = rng.gen_range(0.0..std::f32::consts::TAU);
        let (x, z) = (anchor.0 + r * a.cos(), anchor.1 + r * a.sin());
        if standable(map, region, x, z) && clear_of_bodies(x, z, bodies) {
            return Some((x, z));
        }
    }
    None
}

/// Where the next leg of a walk from `from` ends: `ROAM_LEG_MIN` to
/// `ROAM_LEG_MAX` away, inside the region, on the roam map, and reached by a
/// line that stays out of the town. A wanderer outside its region (it should
/// never be: a fight ends with the walk home) heads for the region's middle.
/// `None` when 8 draws found none (it pauses and tries again).
pub fn pick_leg(
    map: &RoamMap,
    region: &Region,
    from: Vec3f,
    rng: &mut impl Rng,
) -> Option<(f32, f32)> {
    let here = (from.x, from.z);
    let inside = region.contains(from.x, from.z);
    for _ in 0..8 {
        let len = rng.gen_range(ROAM_LEG_MIN..=ROAM_LEG_MAX);
        let angle = if inside {
            rng.gen_range(0.0..std::f32::consts::TAU)
        } else {
            let home = (region.center[1] - from.z).atan2(region.center[0] - from.x);
            home + rng.gen_range(-0.5_f32..=0.5)
        };
        let goal = (from.x + len * angle.cos(), from.z + len * angle.sin());
        let ok_end = if inside {
            standable(map, region, goal.0, goal.1)
        } else {
            map.roamable(goal.0, goal.1)
        };
        if ok_end && segment_clears_town(here, goal) {
            return Some(goal);
        }
    }
    None
}

/// Pack social aggro: the target an idle pack member takes from its
/// packmates' fights, if any. `fights` are the pack's members in combat (where
/// each stands, and its target); `targets` are the bodies a mob may fight (the
/// AI's list: living players and pets); `hidden` are the players a mob's own
/// scan passes over (Hide, feign death, a GM in flight).
///
/// A member joins a fight within `PACK_SOCIAL_RANGE` of it, against a target
/// that is present, not hidden, and within the member's OWN `leash`:
/// `tick_chase` drops a target past the leash on its first tick, so one handed
/// over from further out would send the member walking home, evading, for
/// nothing. Asked every tick, so a member that arrives late still joins.
pub fn pack_fight_to_join(
    pos: Vec3f,
    leash: f32,
    fights: &[(Vec3f, protocol::world::EntityId)],
    targets: &[(protocol::world::EntityId, Vec3f)],
    hidden: &[protocol::world::EntityId],
) -> Option<protocol::world::EntityId> {
    fights.iter().find_map(|(mate_pos, target)| {
        if hidden.contains(target) || pos.distance_to(*mate_pos) > PACK_SOCIAL_RANGE {
            return None;
        }
        let (_, target_pos) = targets.iter().find(|(id, _)| id == target)?;
        (pos.distance_to(*target_pos) <= leash).then_some(*target)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::world::terrain::Terrain;
    use crate::world::{loot, safe_areas, zones};
    use rand::SeedableRng;

    fn plate() -> RoamMap {
        // 800 m across, so there is ground outside the town clearance.
        RoamMap::build(&Terrain::flat(40, 20.0, 0.0), (0.0, 0.0))
    }

    fn ring() -> Region {
        Region {
            center: [0.0, 0.0],
            radius: 390.0,
            min_from_town: 200.0,
            z_min: None,
            z_max: None,
        }
    }

    fn at(x: f32, z: f32) -> Vec3f {
        Vec3f { x, y: 0.0, z }
    }

    #[test]
    fn a_region_is_its_circle_less_the_town_and_outside_its_band() {
        let r = Region {
            center: [300.0, 0.0],
            radius: 200.0,
            min_from_town: 0.0,
            z_min: Some(-50.0),
            z_max: Some(120.0),
        };
        assert!(r.contains(300.0, 0.0));
        assert!(r.contains(160.0, 0.0), "151 m and more from the town");
        assert!(!r.contains(140.0, 0.0), "inside the town clearance");
        assert!(!r.contains(510.0, 0.0), "outside the circle");
        assert!(!r.contains(300.0, -60.0) && !r.contains(300.0, 130.0));
        assert!(!r.contains(f32::NAN, 0.0));
        // A ring: nothing nearer the town than its inner edge.
        assert!(!ring().contains(180.0, 0.0) && ring().contains(210.0, 0.0));
    }

    /// The town rule (invariant: a wanderer never chooses ground inside the
    /// town ring): thousands of spawn spots and legs, none in the clearance,
    /// none off the roam map, no spot within reach of a body, no leg whose
    /// line cuts the town.
    #[test]
    fn a_wanderer_never_chooses_ground_inside_the_town_ring() {
        let map = plate();
        let mut rng = rand::rngs::StdRng::seed_from_u64(7);
        // Regions that cover the town, about it and beside it: the rule must
        // hold anyway.
        let over_town = Region {
            center: [0.0, 0.0],
            radius: 390.0,
            min_from_town: 0.0,
            z_min: None,
            z_max: None,
        };
        let beside_town = Region {
            center: [180.0, 0.0],
            radius: 200.0,
            ..over_town
        };
        let bodies = [at(250.0, 0.0), at(-200.0, 200.0)];
        let mut spots = 0;
        for i in 0..4000 {
            let region = if i % 2 == 0 { &over_town } else { &beside_town };
            let Some((x, z)) = pick_spot(&map, region, &bodies, &mut rng) else {
                continue;
            };
            spots += 1;
            assert!((x * x + z * z).sqrt() >= ROAM_TOWN_CLEARANCE, "({x}, {z})");
            assert!(map.roamable(x, z));
            assert!(clear_of_bodies(x, z, &bodies), "({x}, {z}) is on a body");
        }
        assert!(spots > 3000, "most draws find a spot ({spots})");

        let mut legs = 0;
        // Walk a chain of legs from just outside the ring, as a wanderer does.
        let mut here = at(160.0, 10.0);
        for _ in 0..4000 {
            let Some(goal) = pick_leg(&map, &over_town, here, &mut rng) else {
                continue;
            };
            legs += 1;
            let len = ((goal.0 - here.x).powi(2) + (goal.1 - here.z).powi(2)).sqrt();
            assert!((ROAM_LEG_MIN - 0.01..=ROAM_LEG_MAX + 0.01).contains(&len));
            assert!(over_town.contains(goal.0, goal.1) && map.roamable(goal.0, goal.1));
            assert!(
                segment_clears_town((here.x, here.z), goal),
                "a leg from ({}, {}) to {goal:?} cuts the town",
                here.x,
                here.z
            );
            here = at(goal.0, goal.1);
        }
        assert!(legs > 2000, "most draws find a leg ({legs})");
    }

    #[test]
    fn a_line_through_the_town_is_refused_and_one_round_it_is_not() {
        assert!(!segment_clears_town((-300.0, 0.0), (300.0, 0.0)));
        assert!(!segment_clears_town((-300.0, 100.0), (300.0, 100.0)));
        assert!(segment_clears_town((-300.0, 160.0), (300.0, 160.0)));
        assert!(segment_clears_town((200.0, 0.0), (260.0, 30.0)));
        assert!(!segment_clears_town((100.0, 0.0), (100.0, 0.0)));
    }

    /// A wanderer that finds itself outside its region walks back toward it.
    #[test]
    fn a_leg_from_outside_the_region_heads_for_it() {
        let map = plate();
        let mut rng = rand::rngs::StdRng::seed_from_u64(3);
        let far = Region {
            center: [300.0, 300.0],
            radius: 60.0,
            min_from_town: 0.0,
            z_min: None,
            z_max: None,
        };
        let from = at(-250.0, 300.0);
        for _ in 0..200 {
            let goal = pick_leg(&map, &far, from, &mut rng).expect("open ground");
            assert!(goal.0 > from.x + 10.0, "it heads east, toward its region");
        }
    }

    #[test]
    fn a_pack_member_appears_beside_its_leader() {
        let map = plate();
        let mut rng = rand::rngs::StdRng::seed_from_u64(5);
        for _ in 0..500 {
            let (x, z) = pick_near(&map, &ring(), (300.0, 0.0), &[], &mut rng).expect("a spot");
            let d = ((x - 300.0).powi(2) + z.powi(2)).sqrt();
            assert!((PACK_SPREAD * 0.2 - 0.01..=PACK_SPREAD + 0.01).contains(&d));
        }
        // With a body on the leader there is no spot.
        assert!(pick_near(&map, &ring(), (300.0, 0.0), &[at(300.0, 10.0)], &mut rng).is_none());
    }

    /// Pack social aggro: a member joins a packmate's fight only when it is
    /// near the fight, its own leash reaches the target, and the target is
    /// one a mob may notice (present, not hidden).
    #[test]
    fn a_packmate_joins_a_fight_its_own_leash_reaches_and_no_other() {
        let me = at(0.0, 0.0);
        let player = 7;
        let targets = [(player, at(20.0, 0.0))];
        let join = |fights: &[(Vec3f, u64)], targets: &[(u64, Vec3f)], hidden: &[u64]| {
            pack_fight_to_join(me, 35.0, fights, targets, hidden)
        };
        // A mate 10 m away fighting a player 20 m away: joined.
        assert_eq!(
            join(&[(at(10.0, 0.0), player)], &targets, &[]),
            Some(player)
        );
        // The fight is too far off to hear.
        assert_eq!(join(&[(at(31.0, 0.0), player)], &targets, &[]), None);
        // The target is past this member's own leash (the mate, 30 m nearer
        // to it, is within its own): handed over, `tick_chase` would drop it
        // at once and the member would walk home evading.
        let far = [(player, at(44.0, 0.0))];
        assert_eq!(join(&[(at(30.0, 0.0), player)], &far, &[]), None);
        // A hidden target is never passed on; nor one that is gone.
        assert_eq!(join(&[(at(10.0, 0.0), player)], &targets, &[player]), None);
        assert_eq!(join(&[(at(10.0, 0.0), 99)], &targets, &[]), None);
        // No fight, no target.
        assert_eq!(join(&[], &targets, &[]), None);
        // Two fights: the first it may join.
        let two = [(player, at(44.0, 0.0)), (8, at(5.0, 5.0))];
        assert_eq!(
            join(&[(at(30.0, 0.0), player), (at(6.0, 0.0), 8)], &two, &[]),
            Some(8)
        );
    }

    // ── The shipped table ────────────────────────────────────────────────

    #[test]
    fn the_shipped_table_parses_and_counts() {
        let defs = load(None);
        let day: usize = defs.iter().filter(|d| !d.night_only).map(|d| d.pack).sum();
        let night: usize = defs.iter().filter(|d| d.night_only).map(|d| d.pack).sum();
        // If these change, change them on purpose (the design's section 6).
        assert_eq!(day, 70, "day animals");
        assert_eq!(night, 14, "night animals");
        for d in &defs {
            assert!(
                d.pack == 1 || d.mob.name.contains("Wolf"),
                "only wolves pack"
            );
        }
    }

    #[test]
    fn a_table_that_would_misbehave_is_refused() {
        let ok = r#"
            [[roam]]
            desc = "x"
            mob = { name = "Grass Snake", level = 2, hp = 20.0, dmg = 4, speed = 2.4, aggro = 0.0, leash = 35.0 }
            region = { center = [200.0, 0.0], radius = 40.0 }
            count = 2
            respawn = 60.0
        "#;
        assert_eq!(parse(ok).expect("valid").len(), 2);
        // A passive mob with no leash, or one under spell reach, could be
        // hit from beyond its own reach and never turn.
        let bad = [
            ok.replace(", leash = 35.0", ""),
            ok.replace("leash = 35.0", "leash = 20.0"),
            ok.replace("count = 2", "count = 0"),
            ok.replace("respawn = 60.0", "respawn = 0.0"),
            ok.replace("radius = 40.0", "radius = 0.0"),
            ok.replace("count = 2", "count = 2\ntime = \"dusk\""),
            ok.replace("count = 2", "count = 2\nrespwan = 5.0"),
        ];
        for text in bad {
            assert!(parse(&text).is_err(), "should be refused:\n{text}");
        }
        assert_eq!(
            parse(&ok.replace("count = 2", "count = 2\ntime = \"night\""))
                .expect("valid")
                .iter()
                .filter(|d| d.night_only)
                .count(),
            2
        );
    }

    /// Every wandering animal resolves the way its species should under the
    /// server's name rules (substring matches, either way): a beast drops no
    /// coin and finds its own item table; a shade is no beast (it drops coin,
    /// as the undead do) and finds no animal's table. The camps are held to
    /// the same, so a later rename is caught wherever it lands.
    #[test]
    fn every_roam_and_camp_name_resolves_to_its_species() {
        let species: &[(&str, &str)] = &[
            ("Wolf", "Wolf"),
            ("Rat", "Rat"),
            ("Snake", "Snake"),
            ("Adder", "Snake"),
            ("Viper", "Snake"),
            ("Bear", "Bear"),
            ("Bat", "Bat"),
            ("Boar", "Boar"),
            ("Spider", "Spider"),
            ("Crawler", "Spider"),
        ];
        let names: Vec<String> = load(None)
            .iter()
            .map(|d| d.mob.name.clone())
            .chain(zones::load_camps().iter().map(|c| c.mob.name.clone()))
            .collect();
        let mut animals = 0;
        for name in &names {
            let animal = species.iter().find(|(word, _)| name.contains(word));
            match animal {
                Some((word, table)) => {
                    animals += 1;
                    assert!(loot::is_beast(name), "{name} should drop no coin");
                    // The Ancient Crawler's den reuses the spider camp's
                    // template name; a Crawler by name has no table of its own.
                    if *word != "Crawler" {
                        assert_eq!(
                            loot::table_key_for(name),
                            Some(*table),
                            "{name} should roll the {table} table"
                        );
                    }
                }
                None => {
                    assert!(
                        !loot::is_beast(name),
                        "{name} is no animal: it carries coin"
                    );
                    assert!(
                        !species
                            .iter()
                            .any(|(_, table)| loot::table_key_for(name) == Some(*table)),
                        "{name} should not roll an animal's table"
                    );
                }
            }
        }
        assert!(animals >= 20, "the check saw the animals ({animals})");
    }

    /// No quest counts a wanderer it should not: the wolves count for the wolf
    /// quests and nothing else here ticks any quest (the user's decision 6:
    /// "Wolves yes, Rats no"; the rat quest is Brom's Plague Rats).
    #[test]
    fn wolves_count_for_the_wolf_quests_and_nothing_else_ticks_a_quest() {
        let targets = crate::world::quests::kill_targets();
        assert!(targets.iter().any(|t| t == "Plague Rat"));
        for d in load(None) {
            let name = &d.mob.name;
            let ticked: Vec<&String> = targets
                .iter()
                .filter(|t| crate::world::quests::kill_matches(t, name))
                .collect();
            if name.contains("Wolf") {
                assert!(
                    ticked.iter().all(|t| t.as_str() == "Wolf") && !ticked.is_empty(),
                    "{name} ticks {ticked:?}"
                );
            } else {
                assert!(ticked.is_empty(), "{name} ticks {ticked:?}");
            }
        }
    }

    /// The table's factions and alignment tiers exist, a dev spawn of a
    /// wanderer carries its faction, and no wanderer's aggro reaches from the
    /// town clearance into a safe area.
    #[test]
    fn the_tables_factions_exist_and_the_town_is_out_of_reach() {
        let tiers = [
            "undead", "bad", "evil", "good", "exalted", "innocent", "neutral",
        ];
        for d in load(None) {
            let faction = d
                .mob
                .faction
                .as_deref()
                .expect("every wanderer has a faction");
            assert!(
                crate::world::factions::lookup(faction).is_some(),
                "{}: no faction {faction:?}",
                d.mob.name
            );
            let tier = d.mob.alignment.as_deref().unwrap_or("neutral");
            assert!(tiers.contains(&tier), "{}: tier {tier:?}", d.mob.name);
            assert_eq!(
                zones::faction_of_camp_mob(&d.mob.name, None).0.as_deref(),
                Some(faction),
                "a dev spawn of {} takes its faction",
                d.mob.name
            );
            for area in safe_areas::areas() {
                let c = area.center();
                let from_town = (c.x * c.x + c.z * c.z).sqrt();
                assert!(
                    from_town + area.radius + d.mob.aggro < ROAM_TOWN_CLEARANCE,
                    "{}'s aggro reaches {} from the town clearance",
                    d.mob.name,
                    area.name
                );
            }
        }
    }

    /// Every region has ground: its middle band lies on the compiled zone's
    /// roam map with room for its animals (a few thousand square metres
    /// each), and its basin is the one it is meant for (a region with a z
    /// band holds no ground on the far side of the ridge by construction).
    #[test]
    fn every_region_has_room_on_the_compiled_ground() {
        let map = RoamMap::build(crate::world::terrain::compiled(), (0.0, 0.0));
        let defs = load(None);
        let mut seen: Vec<(String, Region)> = Vec::new();
        for d in &defs {
            if seen.iter().any(|(n, r)| *n == d.mob.name && *r == d.region) {
                continue;
            }
            seen.push((d.mob.name.clone(), d.region));
            let animals: usize = defs
                .iter()
                .filter(|o| o.mob.name == d.mob.name && o.region == d.region)
                .map(|o| o.pack)
                .sum();
            let area = map.area_where(|x, z| d.region.contains(x, z));
            assert!(
                area >= 5_000.0 * animals as f32,
                "{} x{animals}: only {area:.0} square metres of roam map in its region",
                d.mob.name
            );
        }
    }

    /// An authoring aid, not a check: prints the compiled zone's roam map
    /// (north up, 40 m a character; `.` roamable, `#` not, `T` the town, a
    /// digit for each camp's level band) and the room each region of the
    /// shipped table has. Run it when placing regions:
    /// `cargo test -p projectdawn-server --lib print_the_roam_map -- --ignored --nocapture`
    #[test]
    #[ignore = "an authoring aid; prints a map"]
    fn print_the_roam_map() {
        let map = RoamMap::build(crate::world::terrain::compiled(), (0.0, 0.0));
        let camps = zones::load_camps();
        let step = 40.0;
        let n = 100;
        for row in 0..n {
            let z = -2000.0 + (row as f32 + 0.5) * step;
            let line: String = (0..n)
                .map(|col| {
                    let x = -2000.0 + (col as f32 + 0.5) * step;
                    if x.abs() < step && z.abs() < step {
                        return 'T';
                    }
                    let camp = camps.iter().find(|c| {
                        (c.pos[0] - x).abs() <= step / 2.0 && (c.pos[2] - z).abs() <= step / 2.0
                    });
                    match camp {
                        Some(c) => char::from_digit((c.mob.level / 2).min(9), 10).unwrap_or('?'),
                        None if map.roamable(x, z) => '.',
                        None => '#',
                    }
                })
                .collect();
            println!("{line}  z={z:6.0}");
        }
        println!("roam map: {:.1}% of the plate", map.share() * 100.0);
        let mut seen: Vec<(String, Region)> = Vec::new();
        for d in load(None) {
            if seen
                .iter()
                .any(|(nm, r)| *nm == d.mob.name && *r == d.region)
            {
                continue;
            }
            seen.push((d.mob.name.clone(), d.region));
            let area = map.area_where(|x, z| d.region.contains(x, z));
            println!(
                "{:>16}: {:>9.0} m2 of roam map in its region",
                d.mob.name, area
            );
        }
    }
}
