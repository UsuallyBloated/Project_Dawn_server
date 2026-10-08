//! Safe areas (spell batch step 2, 2026-10-08): the places where a caster may
//! bind a GROUP MEMBER (a self-bind works anywhere), and where Succor and
//! Evacuate arrive. Read from `data/safe_areas.toml` at startup, like the camps
//! and the NPCs. A one-zone world has one; more are a data edit.

use super::connection::Vec3f;
use serde::Deserialize;
use std::sync::OnceLock;

const SAFE_AREAS_TOML: &str = include_str!("../../data/safe_areas.toml");

#[derive(Debug, Clone, Deserialize)]
pub struct SafeArea {
    /// For the data file's reader and the tests' messages; nothing in the
    /// tick names an area yet (a "Bound near: ..." readout would).
    #[allow(dead_code)]
    pub name: String,
    pub center: [f32; 3],
    pub radius: f32,
    pub arrival: [f32; 3],
}

#[derive(Deserialize)]
struct Parsed {
    #[serde(default)]
    area: Vec<SafeArea>,
}

impl SafeArea {
    pub fn center(&self) -> Vec3f {
        Vec3f { x: self.center[0], y: self.center[1], z: self.center[2] }
    }
    pub fn arrival(&self) -> Vec3f {
        Vec3f { x: self.arrival[0], y: self.arrival[1], z: self.arrival[2] }
    }
    pub fn contains(&self, pos: Vec3f) -> bool {
        // Keep-only-when-inside: a non-finite position is outside.
        pos.distance_to(self.center()) <= self.radius
    }
}

pub fn areas() -> &'static [SafeArea] {
    static AREAS: OnceLock<Vec<SafeArea>> = OnceLock::new();
    AREAS.get_or_init(|| {
        let parsed: Parsed = toml::from_str(SAFE_AREAS_TOML).expect("safe_areas.toml parses");
        parsed.area
    })
}

/// The safe area `pos` stands in, if any.
pub fn containing(pos: Vec3f) -> Option<&'static SafeArea> {
    areas().iter().find(|a| a.contains(pos))
}

/// Where a safe-area port from `pos` lands: the arrival point of the nearest
/// area. None only if no area is defined at all.
pub fn nearest_arrival(pos: Vec3f) -> Option<Vec3f> {
    areas()
        .iter()
        .min_by(|a, b| {
            let da = pos.distance_to(a.center());
            let db = pos.distance_to(b.center());
            da.partial_cmp(&db).unwrap_or(std::cmp::Ordering::Equal)
        })
        .map(|a| a.arrival())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_town_square_is_a_safe_area_and_the_spawn_is_inside_it() {
        assert!(!areas().is_empty(), "at least one safe area");
        let spawn = crate::world::STARTER_SPAWN;
        assert!(containing(spawn).is_some(), "the starter spawn is in a safe area");
        assert!(containing(Vec3f { x: 500.0, y: 0.0, z: 500.0 }).is_none());
        assert!(
            containing(Vec3f { x: f32::NAN, y: 0.0, z: 0.0 }).is_none(),
            "a non-finite position is nowhere"
        );
        assert_eq!(
            nearest_arrival(Vec3f { x: 300.0, y: 0.0, z: -40.0 }).map(|p| (p.x, p.z)),
            Some((0.0, 0.0))
        );
    }

    /// No camp spawn's aggro circle (plus the 3 m spawn jitter) reaches into
    /// any safe area, so nobody is bound or evacuated into a mob's reach. The
    /// test prints the tightest margin so a camp move can be judged.
    #[test]
    fn no_hostile_aggro_reaches_into_a_safe_area() {
        const SPAWN_JITTER: f32 = 3.0;
        let camps = crate::world::zones::load_camps();
        let mut tightest: Option<(String, String, f32)> = None;
        for area in areas() {
            for camp in &camps {
                if camp.mob.aggro <= 0.0 {
                    continue;
                }
                let s = camp.pos;
                let spawn = Vec3f { x: s[0], y: s[1], z: s[2] };
                let reach = camp.mob.aggro + camp.radius.max(SPAWN_JITTER);
                let margin = spawn.distance_to(area.center()) - reach - area.radius;
                if tightest.as_ref().map_or(true, |t| margin < t.2) {
                    tightest = Some((area.name.clone(), camp.mob.name.clone(), margin));
                }
                assert!(
                    margin >= 0.0,
                    "{} at {:?} (aggro {} + jitter) reaches into {} by {:.1} m",
                    camp.mob.name, s, camp.mob.aggro, area.name, -margin
                );
            }
        }
        if let Some((area, mob, margin)) = tightest {
            eprintln!("tightest safe-area margin: {area} vs {mob}, {margin:.1} m to spare");
        }
    }
}
