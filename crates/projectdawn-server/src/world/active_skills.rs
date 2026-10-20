//! Active skills, the server's model (client-authority audit item 6,
//! 2026-10-19). Backstab, Shield Bash, Hide, Feign Death and the rest used to
//! run on the client: it spent its own stamina, started its own cooldown and
//! sent a plain `Attack`, so a skill's multiplier was worth nothing, the
//! stamina never moved on the server, and every SELF effect was either a
//! decoration or a silent error online. Now `UseSkill` names the skill and
//! the server owns all of it: the class gate, the cooldown, the stamina, the
//! swing (the ordinary melee roll times the skill's multiplier) and the
//! effect. A refused skill costs nothing and answers with `SkillFail`.
//!
//! The table is `data/active_skills.toml`, a mirror of the client's
//! `data/skill_definitions.gd`, kept in step by
//! `tools/check_active_skill_lockstep.gd`.

use serde::Deserialize;
use std::collections::HashMap;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

const ACTIVE_SKILLS_TOML: &str = include_str!("../../data/active_skills.toml");

/// A skill used this much before its cooldown has run is still accepted: the
/// client times its own cooldown, and a packet can arrive a little ahead of
/// the server's clock. A modified client gains at most this over an honest
/// one (the global cooldown's quarter second, the same reasoning).
pub const COOLDOWN_GRACE: Duration = Duration::from_millis(250);

/// The chance a Feign Death takes (the client's number). Nothing scales it
/// yet; EQ's grew with the skill.
pub const FEIGN_DEATH_SUCCESS: f32 = 0.80;

/// How long a feigned player stays unnoticed by idle mobs if they never
/// move: EQ's mobs forget a feigned player after about two minutes. Any
/// movement, swing, skill or hostile cast ends it sooner.
pub const FEIGN_DEATH_HOLD_SECS: f32 = 120.0;

#[derive(Debug, Deserialize)]
struct SkillsFile {
    #[serde(rename = "skill")]
    skills: Vec<ActiveSkill>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ActiveSkill {
    pub name: String,
    pub cooldown: f32,
    pub stamina_cost: f32,
    /// 0 for a SELF skill (no swing); otherwise the multiplier on an
    /// ordinary main-hand swing.
    pub damage_multiplier: f32,
    pub classes: Vec<String>,
    #[serde(default)]
    pub effect_type: String,
    #[serde(default)]
    pub effect_duration: f32,
    #[serde(default)]
    pub absorb_amount: f32,
    #[serde(default)]
    pub stun_duration: f32,
    #[serde(default)]
    pub stealth_duration: f32,
}

/// What a skill does besides (or instead of) its swing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Effect {
    None,
    /// Every melee swing at the user is deflected for `effect_duration`.
    EvadeBoost,
    /// An absorb pool of `absorb_amount` on the user.
    AbsorbShield,
    /// The swing, then the user's pet is sent at the target.
    WarderFury,
    /// The target cannot move, swing or cast for `stun_duration`.
    Stun,
    /// A roll; on success every mob forgets the user.
    FeignDeath,
    /// Idle mobs do not notice the user for `stealth_duration`, or until
    /// they move, swing, use a damage skill or cast at a mob.
    Stealth,
    /// The swing never misses.
    Truesight,
}

impl ActiveSkill {
    pub fn effect(&self) -> Effect {
        match self.effect_type.as_str() {
            "EVADE_BOOST" => Effect::EvadeBoost,
            "ABSORB_SHIELD" => Effect::AbsorbShield,
            "WARDER_FURY" => Effect::WarderFury,
            "STUN" => Effect::Stun,
            "FEIGN_DEATH" => Effect::FeignDeath,
            "STEALTH" => Effect::Stealth,
            "TRUESIGHT" => Effect::Truesight,
            _ => Effect::None,
        }
    }

    /// A skill that swings at a target (the client's rule: a multiplier
    /// above zero means an ENEMY target).
    pub fn is_damage(&self) -> bool {
        self.damage_multiplier > 0.0
    }

    pub fn usable_by(&self, class: &str) -> bool {
        self.classes.iter().any(|c| c == class)
    }

    pub fn cooldown_duration(&self) -> Duration {
        Duration::from_secs_f32(self.cooldown.max(0.0))
    }
}

fn skills() -> &'static HashMap<String, ActiveSkill> {
    static SKILLS: OnceLock<HashMap<String, ActiveSkill>> = OnceLock::new();
    SKILLS.get_or_init(|| {
        let parsed: SkillsFile = toml::from_str(ACTIVE_SKILLS_TOML)
            .expect("active_skills.toml must parse; fix the embedded file");
        parsed.skills.into_iter().map(|s| (s.name.clone(), s)).collect()
    })
}

/// Look up a skill by its name (`SkillData.skill_name` on the client).
pub fn lookup(name: &str) -> Option<&'static ActiveSkill> {
    skills().get(name)
}

/// Whether `ready_at` (the instant a skill's cooldown ends) still refuses a
/// use at `now`, allowing `COOLDOWN_GRACE`.
pub fn on_cooldown(ready_at: Option<Instant>, now: Instant) -> bool {
    match ready_at {
        Some(t) => now + COOLDOWN_GRACE < t,
        None => false,
    }
}

/// The feign death decision for one roll in [0, 1).
pub fn feign_death_succeeds(roll: f32) -> bool {
    roll < FEIGN_DEATH_SUCCESS
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_table_parses_and_every_effect_is_known() {
        let n = skills().len();
        assert!(n >= 32, "the client's 32 skills are all here: {n}");
        for s in skills().values() {
            assert!(!s.classes.is_empty(), "{} names a class", s.name);
            assert!(s.cooldown > 0.0 && s.stamina_cost > 0.0, "{} costs something", s.name);
            if !s.effect_type.is_empty() {
                assert_ne!(s.effect(), Effect::None, "{} has an unknown effect {}", s.name, s.effect_type);
            }
            match s.effect() {
                Effect::EvadeBoost => assert!(s.effect_duration > 0.0, "{} lasts", s.name),
                Effect::AbsorbShield => assert!(s.absorb_amount > 0.0, "{} absorbs", s.name),
                Effect::Stun => assert!(s.stun_duration > 0.0 && s.is_damage(), "{} stuns with a hit", s.name),
                Effect::Stealth => assert!(s.stealth_duration > 0.0, "{} hides for a while", s.name),
                Effect::WarderFury | Effect::Truesight => assert!(s.is_damage(), "{} swings", s.name),
                Effect::FeignDeath | Effect::None => {}
            }
        }
        let backstab = lookup("Backstab").expect("Backstab");
        assert!((backstab.damage_multiplier - 3.0).abs() < 1e-6);
        assert!(backstab.usable_by("Rogue") && !backstab.usable_by("Cleric"));
        assert_eq!(lookup("Shield Bash").unwrap().effect(), Effect::Stun);
        assert!(lookup("No Such Skill").is_none());
    }

    #[test]
    fn the_cooldown_allows_its_grace_and_no_more() {
        let now = Instant::now();
        assert!(!on_cooldown(None, now));
        assert!(on_cooldown(Some(now + Duration::from_secs(3)), now));
        assert!(!on_cooldown(Some(now + Duration::from_millis(200)), now), "inside the grace");
        assert!(on_cooldown(Some(now + Duration::from_millis(400)), now), "past it");
    }

    #[test]
    fn feign_death_takes_four_rolls_in_five() {
        assert!(feign_death_succeeds(0.0));
        assert!(feign_death_succeeds(0.79));
        assert!(!feign_death_succeeds(0.80));
        assert!(!feign_death_succeeds(0.99));
    }
}
