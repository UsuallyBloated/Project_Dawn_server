//! Bard songs, the server's rules (the rework of 2026-10-20). A song takes
//! its `cast_time` to sing, costs no mana, and the server pulses the bard's
//! one active song every `SONG_PULSE_SECS`; each pulse's effects are the
//! ordinary buffs lingering `SONG_LINGER_SECS`. The tick loop owns the state
//! (`PerConnection::active_song`); this module holds the pure parts: how a
//! song's strength scales with the bard's level, and the scaled effects.
//!
//! Scaling (user, 2026-10-20): a level 1 character must not be as capable
//! as a level 10 one. Classic EQ scaled each song by level (Selo's ran +20%
//! at 5 and +65% at 50; Hymn of Restoration 2 a tick at 6 and 13 at 60), so
//! each song's listed effect is its level-50 value and runs at
//! `SONG_SCALE_AT_LEARN` of it when first learned.

use super::{spells::Spell, SONG_FULL_AT_LEVEL, SONG_PULSE_SECS, SONG_SCALE_AT_LEARN};

/// The fraction of a song's listed effect a bard of `level` gets, the song
/// first learned at `learn_level`: `SONG_SCALE_AT_LEARN` there, 1.0 from
/// `SONG_FULL_AT_LEVEL` on, linear between.
pub fn song_scale(level: i32, learn_level: i32) -> f32 {
    let full = SONG_FULL_AT_LEVEL.max(learn_level + 1);
    if level >= full {
        return 1.0;
    }
    let t = (level - learn_level).max(0) as f32 / (full - learn_level) as f32;
    SONG_SCALE_AT_LEARN + (1.0 - SONG_SCALE_AT_LEARN) * t
}

/// What one pulse of `spell` puts on a bard of `level` and `class`, scaled.
/// Zero means the song carries no such effect.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PulseEffects {
    /// A heal-over-time in HP a second for the linger: the song's per-pulse
    /// heal spread over the pulse.
    pub hot_hps: f32,
    /// A speed multiplier (1.0 = none); the bonus part scales.
    pub speed_mult: f32,
    pub mp_regen_hps: f32,
    pub haste: f32,
    pub accuracy: f32,
    pub crit: f32,
    pub attack_slow: f32,
}

pub fn pulse_effects(spell: &Spell, level: i32, class: &str) -> PulseEffects {
    let s = song_scale(level, spell.required_level(class));
    PulseEffects {
        hot_hps: if spell.heal_amount > 0.0 { spell.heal_amount * s / SONG_PULSE_SECS } else { 0.0 },
        speed_mult: if spell.move_speed_mult > 1.0 { 1.0 + (spell.move_speed_mult - 1.0) * s } else { 1.0 },
        mp_regen_hps: spell.mp_regen_hps * s,
        haste: spell.haste_amount * s,
        accuracy: spell.accuracy_buff * s,
        crit: spell.crit_buff * s,
        attack_slow: spell.attack_slow_amount * s,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_song_grows_from_its_learn_level_to_full_at_fifty() {
        assert!((song_scale(1, 1) - SONG_SCALE_AT_LEARN).abs() < 1e-6, "at the learn level");
        assert!((song_scale(50, 1) - 1.0).abs() < 1e-6, "full at 50");
        assert!((song_scale(60, 1) - 1.0).abs() < 1e-6, "and past it");
        let mid = song_scale(25, 1);
        assert!(mid > 0.65 && mid < 0.75, "about 0.7 halfway: {mid}");
        assert!(song_scale(10, 10) < song_scale(10, 1), "a song learned later starts lower at the same level");
        assert!((song_scale(4, 10) - SONG_SCALE_AT_LEARN).abs() < 1e-6, "below the learn level counts as the learn level");
    }

    #[test]
    fn the_pulse_scales_each_effect_and_spreads_the_heal_over_the_pulse() {
        super::super::spells::lookup("Smite"); // the table parses
        let selos = super::super::spells::lookup("Selos' Melody").expect("Selos' Melody");
        let at1 = pulse_effects(selos, 1, "Bard");
        let at50 = pulse_effects(selos, 50, "Bard");
        assert!((at50.speed_mult - 1.35).abs() < 1e-6, "the listed value at 50");
        assert!((at1.speed_mult - 1.14).abs() < 1e-6, "0.4 of the bonus at level 1: {}", at1.speed_mult);
        let mending = super::super::spells::lookup("Poet's Mending").expect("Poet's Mending");
        let heal50 = pulse_effects(mending, 50, "Bard");
        assert!((heal50.hot_hps - 8.0 / SONG_PULSE_SECS).abs() < 1e-6, "8 a pulse as HP a second");
        let weave = super::super::spells::lookup("Mana Weave").expect("Mana Weave");
        assert!((pulse_effects(weave, 50, "Bard").mp_regen_hps - 1.0).abs() < 1e-6, "P99's 6 a tick");
        assert!(pulse_effects(weave, 10, "Bard").mp_regen_hps < 0.5, "and less when first learned");
        for s in super::super::spells::all().filter(|s| s.is_song) {
            assert!((s.cast_time - 3.0).abs() < 1e-6 && s.mana_cost == 0.0, "{}: a 3 s sing, free", s.name);
        }
    }
}
