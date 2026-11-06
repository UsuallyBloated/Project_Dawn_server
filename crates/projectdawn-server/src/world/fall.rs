//! Falls (docs/design/server_fall_damage.md, slice 1): a player's vertical
//! motion, simulated by the server off its own ground (the height model,
//! `terrain.rs`), and the landing's cost.
//!
//! On the ground the body follows the ground down as far as the client's own
//! floor snap reaches in one tick, and only onto ground it can stand on; past
//! that it has walked off an edge and falls at `GRAVITY` until a floor catches
//! it. Ground too steep to stand on is a wall: a falling body rests against
//! it for a tick and slides down it (the movement step moves it clear), the
//! fall still open from where it began. The landing's damage is a share of
//! max HP by the height fallen (the user's call of 2026-10-24: a drop near
//! 2 m is safe, and the rule hurts every level alike); nothing a client sends
//! enters it. The jump stays cosmetic: it never adds to a fall. Mobs and pets
//! never fall (they are pinned to the ground as they move).
//!
//! The pure rules live here with their unit tests; `tick/movement.rs` applies
//! them each tick and `tick/casting.rs` settles a fall before a port.

/// Gravity, m/s². Lockstep with `player.gd`'s `GRAVITY` (which carries the
/// sign; `tools/check_constant_lockstep.gd` compares the magnitude).
pub const GRAVITY: f32 = 20.0;
/// The steepest floor the client's body walks, degrees (`player.gd`'s
/// `floor_max_angle`). Steeper ground is a wall: the body never stands on it
/// and slides down it.
pub const FLOOR_MAX_ANGLE_DEG: f32 = 55.0;
/// How far down the client's body snaps to keep its floor, metres
/// (`player.gd`'s `floor_snap_length`).
pub const FLOOR_SNAP_LENGTH: f32 = 0.5;
/// A drop this high or less hurts nobody (the design's safe drop).
pub const SAFE_DROP: f32 = 2.0;
/// A drop this high takes all of max HP; between the two the share is linear
/// (2.5% of max HP a metre). A tuning knob with `SAFE_DROP`.
pub const LETHAL_DROP: f32 = 42.0;

/// The vertical state a player carries between ticks: standing on the ground
/// (the default) or in the air, with a vertical speed (negative is down) and
/// the height it left the ground at.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Vertical {
    pub airborne: bool,
    pub vy: f32,
    pub from_y: f32,
}

/// A body touching down: the height of the fall (from where it left the
/// ground to where it landed, exact whatever the tick) and the speed that
/// height amounts to in free fall, `sqrt(2 g drop)`. The tick-by-tick `vy`
/// moves the body for the Position fan and finds the landing tick; the cost
/// never reads it, so a fall costs the same however the ticks fell.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Landing {
    pub impact_speed: f32,
    pub drop: f32,
}

/// The steepest floor's gradient: `tan(FLOOR_MAX_ANGLE_DEG)`.
pub fn floor_tan() -> f32 {
    FLOOR_MAX_ANGLE_DEG.to_radians().tan()
}

/// Whether ground with this gradient (dh/dx, dh/dz) is a floor the client's
/// body stands on: no steeper than `FLOOR_MAX_ANGLE_DEG`.
pub fn is_floor(gradient: (f32, f32)) -> bool {
    let (gx, gz) = gradient;
    (gx * gx + gz * gz).sqrt() <= floor_tan()
}

/// How far down the ground may drop under a step of `horizontal_step` metres
/// before the body leaves it: the client's `move_and_slide` keeps its floor
/// down a slope of `FLOOR_MAX_ANGLE_DEG` plus its snap length. The snap is
/// added whole (it errs on staying grounded, the design's ledger, row 8),
/// which is safe only because a non-floor is never followed at all.
pub fn max_drop(horizontal_step: f32) -> f32 {
    horizontal_step * floor_tan() + FLOOR_SNAP_LENGTH
}

/// One tick of falling: the speed and height after `dt` from a body moving
/// down at `vy` at height `y`. The one integrator (`vertical_step` and
/// `next_height` both read it, so the two can never disagree).
fn fallen(vy: f32, y: f32, dt: f32) -> (f32, f32) {
    let vy = vy - GRAVITY * dt;
    (vy, y + vy * dt)
}

/// The body's height at the end of a tick of falling from `y`, before the
/// ground is consulted: what `vertical_step` will test against the ground.
pub fn next_height(v: &Vertical, y: f32, dt: f32) -> f32 {
    fallen(v.vy, y, dt).1
}

/// One tick of vertical motion. `y` is the body's height now, `ground_y` the
/// ground under its new XZ and `floor` whether that ground is a floor
/// (`is_floor` of its gradient), `horizontal_step` how far it moved this
/// tick. Returns the new height and, when it touched down, the landing.
///
/// Grounded, the body follows the ground down by at most `max_drop`, and only
/// onto a floor (a step up is a step whatever the ground, the slope rule
/// having allowed it; standing on a face it cannot stand on, or stepping
/// along one, it leaves the ground); anything else is an edge, and it begins
/// to fall this same tick from its own height. Airborne, it falls at `GRAVITY`; the tick
/// it reaches a floor it lands there, the landing measured from the height it
/// left the ground at, so the cost does not depend on where in a tick the
/// ground was met. Ground it reaches that is no floor it rests against
/// instead, the fall still open: the movement step slides it clear next tick.
pub fn vertical_step(
    v: &mut Vertical,
    y: f32,
    ground_y: f32,
    floor: bool,
    horizontal_step: f32,
    dt: f32,
) -> (f32, Option<Landing>) {
    if !v.airborne {
        let within_reach = ground_y >= y - max_drop(horizontal_step);
        if within_reach && (floor || ground_y > y) {
            return (ground_y, None);
        }
        *v = Vertical {
            airborne: true,
            vy: 0.0,
            from_y: y,
        };
    }
    let (vy, next) = fallen(v.vy, y, dt);
    v.vy = vy;
    if next <= ground_y {
        if !floor {
            v.vy = 0.0;
            return (ground_y, None);
        }
        return (ground_y, Some(touch_down(v, ground_y)));
    }
    (next, None)
}

/// Land a body in the air where it is (a port or teleport about to move it;
/// the design's ledger, row 6): the landing is the whole fall from where it
/// left the ground to the ground under it now, so a port mid-fall cannot
/// erase it. `None` for a body on the ground.
pub fn settle(v: &mut Vertical, ground_y: f32) -> Option<Landing> {
    if !v.airborne {
        return None;
    }
    Some(touch_down(v, ground_y))
}

/// The landing of a body on `ground_y`, and the vertical state reset to
/// standing. A body that somehow ends under the ground fell nothing and
/// lands softly.
fn touch_down(v: &mut Vertical, ground_y: f32) -> Landing {
    let drop = (v.from_y - ground_y).max(0.0);
    *v = Vertical::default();
    Landing {
        impact_speed: (2.0 * GRAVITY * drop).sqrt(),
        drop,
    }
}

/// The landing's damage: nothing up to `SAFE_DROP`, all of `max_hp` from
/// `LETHAL_DROP`, linear between, never less than a point once it hurts. No
/// armor, evasion or shield enters it (the user's call), and no attacker.
pub fn damage(drop: f32, max_hp: f32) -> f32 {
    if !drop.is_finite() {
        return 0.0;
    }
    let share = ((drop - SAFE_DROP) / (LETHAL_DROP - SAFE_DROP)).clamp(0.0, 1.0);
    if share <= 0.0 {
        return 0.0;
    }
    (share * max_hp).round().max(1.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    const DT: f32 = 0.05;
    /// A full-speed tick on the flat: 5 m/s x 50 ms.
    const STEP: f32 = 0.25;

    #[test]
    fn walking_down_the_clients_steepest_floor_stays_grounded() {
        // Down a 55° slope the ground drops STEP x tan(55°) a tick, 0.357 m:
        // under the 0.857 m the body follows.
        let mut v = Vertical::default();
        let drop = STEP * floor_tan();
        let (y, landing) = vertical_step(&mut v, 10.0, 10.0 - drop, true, STEP, DT);
        assert_eq!(landing, None);
        assert!((y - (10.0 - drop)).abs() < 1e-6, "{y}");
        assert!(!v.airborne);
    }

    #[test]
    fn standing_still_follows_half_a_metre_and_no_more() {
        let mut v = Vertical::default();
        assert_eq!(vertical_step(&mut v, 5.0, 4.6, true, 0.0, DT), (4.6, None));
        assert!(!v.airborne);
        let (y, landing) = vertical_step(&mut v, 5.0, 4.4, true, 0.0, DT);
        assert!(v.airborne, "a 0.6 m drop under a still body is an edge");
        assert_eq!(landing, None);
        assert!(y < 5.0 && y > 4.9, "it begins to fall this tick: {y}");
    }

    #[test]
    fn a_step_past_max_drop_goes_airborne_and_the_snap_is_whole() {
        assert!((max_drop(0.0) - FLOOR_SNAP_LENGTH).abs() < 1e-6);
        assert!((max_drop(STEP) - 0.857).abs() < 0.001, "{}", max_drop(STEP));
        let mut v = Vertical::default();
        let (_, landing) = vertical_step(&mut v, 10.0, 0.0, true, STEP, DT);
        assert!(v.airborne && landing.is_none());
        assert!(v.vy < 0.0, "falling: {v:?}");
    }

    /// The review's find (2026-11-04): with the snap added whole, a body was
    /// followed down any face as long as each tick's drop stayed under half a
    /// metre, so a slow enough step descended a vertical cliff unhurt.
    #[test]
    fn a_body_never_follows_the_ground_onto_a_face_it_cannot_stand_on() {
        // Down a 70° face a tiny step drops a few centimetres, well within
        // reach, but the ground is no floor: the body leaves it.
        let mut v = Vertical::default();
        let (_, landing) = vertical_step(&mut v, 10.0, 9.97, false, 0.01, DT);
        assert!(v.airborne && landing.is_none(), "{v:?}");
        assert_eq!(v.from_y, 10.0);
        // Reaching such ground from the air: the body rests against it, no
        // landing, the fall still open from where it began.
        let mut v = Vertical {
            airborne: true,
            vy: -8.0,
            from_y: 30.0,
        };
        assert_eq!(
            vertical_step(&mut v, 12.0, 11.8, false, 0.0, DT),
            (11.8, None)
        );
        assert_eq!(
            v,
            Vertical {
                airborne: true,
                vy: 0.0,
                from_y: 30.0
            }
        );
        // A floor at that height lands it, charged from where it began.
        let mut v = Vertical {
            airborne: true,
            vy: -8.0,
            from_y: 30.0,
        };
        let (y, landing) = vertical_step(&mut v, 12.0, 11.8, true, 0.0, DT);
        assert_eq!(y, 11.8);
        let landing = landing.expect("a floor lands the body");
        assert!((landing.drop - 18.2).abs() < 1e-4, "{landing:?}");
        // A step UP onto ground the slope rule allowed is a step, floor or not.
        let mut v = Vertical::default();
        assert_eq!(
            vertical_step(&mut v, 10.0, 10.3, false, STEP, DT),
            (10.3, None)
        );
        assert!(!v.airborne);
        // Standing on such ground (a load or a port put the body there) is
        // no stance: the body leaves it and the slide takes it down.
        let mut v = Vertical::default();
        let (_, landing) = vertical_step(&mut v, 10.0, 10.0, false, 0.0, DT);
        assert!(v.airborne && landing.is_none(), "{v:?}");
    }

    #[test]
    fn a_ten_metre_fall_lands_at_the_free_fall_speed() {
        let mut v = Vertical::default();
        let mut y = 10.0;
        let mut ticks = 0;
        let landing = loop {
            let (ny, landing) = vertical_step(&mut v, y, 0.0, true, 0.0, DT);
            y = ny;
            ticks += 1;
            if let Some(l) = landing {
                break l;
            }
            assert!(ticks < 100, "never landed");
        };
        // sqrt(2 x 20 x 10) = 20 m/s, in about a second (20 ticks), exactly
        // 10 m of drop whatever tick the ground was met in.
        assert!((landing.impact_speed - 20.0).abs() < 1e-4, "{landing:?}");
        assert!((landing.drop - 10.0).abs() < 1e-5, "{landing:?}");
        assert!((19..=22).contains(&ticks), "{ticks} ticks");
        assert_eq!(y, 0.0);
        assert_eq!(v, Vertical::default());
    }

    /// The ticks lag a true free fall a little (the speed steps before the
    /// height), which is why the landing reads the height left from and not
    /// the speed at the end: a fall costs the same at any tick rate.
    #[test]
    fn the_landing_reads_the_height_not_the_tick() {
        for dt in [0.05, 0.01, 0.2] {
            let mut v = Vertical::default();
            let mut y = 30.0;
            let landing = loop {
                let (ny, landing) = vertical_step(&mut v, y, 4.0, true, 0.0, dt);
                y = ny;
                if let Some(l) = landing {
                    break l;
                }
            };
            assert!((landing.drop - 26.0).abs() < 1e-5, "dt {dt}: {landing:?}");
        }
    }

    #[test]
    fn the_damage_is_a_share_of_max_hp() {
        assert_eq!(damage(0.0, 100.0), 0.0);
        assert_eq!(damage(SAFE_DROP, 100.0), 0.0);
        assert_eq!(damage(SAFE_DROP + 20.0, 100.0), 50.0);
        assert_eq!(damage(LETHAL_DROP, 100.0), 100.0);
        assert_eq!(damage(90.0, 100.0), 100.0);
        // A Warrior 1 (28 HP) off a 3 m drop: 2.5% is 0.7, so one point.
        assert_eq!(damage(3.0, 28.0), 1.0);
        assert_eq!(damage(10.0, 28.0), 6.0);
        assert_eq!(damage(f32::NAN, 100.0), 0.0);
    }

    #[test]
    fn a_floor_is_the_clients_angle_and_a_wall_is_steeper() {
        let t55 = floor_tan();
        assert!(is_floor((0.0, 0.0)));
        assert!(is_floor((t55 * 0.99, 0.0)));
        assert!(!is_floor((0.0, t55 * 1.01)));
        assert!(!is_floor((60.0, 0.0)), "a 0.5 m face 30 m high");
        // The height a tick of falling reaches, from rest and at speed: the
        // same integrator `vertical_step` moves the body with.
        let still = Vertical::default();
        assert!((next_height(&still, 10.0, DT) - (10.0 - GRAVITY * DT * DT)).abs() < 1e-6);
        let mut fast = Vertical {
            airborne: true,
            vy: -20.0,
            from_y: 30.0,
        };
        let predicted = next_height(&fast, 10.0, DT);
        assert!((predicted - (10.0 - 21.0 * DT)).abs() < 1e-5);
        let (stepped, _) = vertical_step(&mut fast, 10.0, 0.0, true, 0.0, DT);
        assert_eq!(stepped, predicted);
    }

    #[test]
    fn a_port_settles_a_fall_where_it_is() {
        // Left the ground at 7.5 m, 2.5 m fallen so far, the floor under it:
        // the whole 7.5 m landing.
        let mut v = Vertical {
            airborne: true,
            vy: -10.0,
            from_y: 7.5,
        };
        let landing = settle(&mut v, 0.0).expect("a body in the air lands");
        assert!(
            (landing.impact_speed - 300f32.sqrt()).abs() < 1e-3,
            "{landing:?}"
        );
        assert!((landing.drop - 7.5).abs() < 1e-3, "{landing:?}");
        assert_eq!(v, Vertical::default());
        // On the ground there is nothing to settle.
        assert_eq!(settle(&mut v, 5.0), None);
        // A body somehow under the ground it lands on fell nothing.
        let mut under = Vertical {
            airborne: true,
            vy: -4.0,
            from_y: 1.8,
        };
        let l = settle(&mut under, 3.0).expect("lands");
        assert_eq!((l.impact_speed, l.drop), (0.0, 0.0));
    }
}
