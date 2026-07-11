//! Server-side quest table — the authoritative source for quest XP rewards
//! and (PD_W0024) quest objectives.
//!
//! The reward is server-authored: the client reports only a quest id
//! (`ClientWorldMsg::CompleteQuest`), the server looks it up here, computes
//! the XP itself from its own curve, and records the completion so a quest
//! pays once per character, ever. This closes the PD_W0018-era hole where the
//! client named its own `GrantQuestXp { amount }` (one forged packet was an
//! instant level cap; a relog was an infinite repeat-turn-in faucet).
//!
//! Since PD_W0024 the OBJECTIVES are server-counted too: `active_quests`
//! progress lives on the connection + DB, kills increment it in the tick
//! loop, and a turn-in is rejected until every objective is met — a forged
//! `CompleteQuest` with no gameplay behind it pays nothing.
//!
//! Mirror of the client's `data/quest_definitions.gd` — same lockstep
//! discipline as `spells.toml` / `spell_definitions.gd`: edit both in the
//! same commit. The tier fractions mirror `QuestDefinitions.REWARD_TIERS`.

use serde::Deserialize;
use std::collections::HashMap;
use std::sync::OnceLock;

const QUESTS_TOML: &str = include_str!("../../data/quests.toml");

#[derive(Debug, Deserialize)]
struct QuestsFile {
    #[serde(rename = "quest")]
    quests: Vec<Quest>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Quest {
    pub id: String,
    pub level_req: i32,
    pub reward_tier: String,
    /// Server-counted objectives, in the same order as the client journal's
    /// TRACKED (kill-type) objectives — `QuestProgress.objective_index` and
    /// the `active_quests.progress` vector both index into this list.
    /// Required and non-empty: a quest with no objectives would be a free
    /// turn-in, so the parser rejects it (see `parse`).
    pub objectives: Vec<Objective>,
    /// PD_W0024 slice B — item reward `.tres` paths granted server-side on
    /// turn-in (one each of `count = 1`), keyed into the item registry
    /// (`items.toml`). Optional; the parser rejects a path that isn't a known
    /// item so a typo fails a test instead of silently granting nothing. Kept
    /// in lockstep with the client's `QuestDefinitions.item_rewards`.
    #[serde(default)]
    pub item_rewards: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Objective {
    /// Only "kill" exists today; the parser rejects anything else so a typo'd
    /// kind fails the build's tests instead of silently never counting.
    pub kind: String,
    /// Mob-name pattern, matched by `kill_matches` (bidirectional lowercased
    /// substring — the exact rule the client's `notify_kill` used, kept so
    /// both sides always agree whether "Grey Wolf" ticks "Wolf").
    pub target: String,
    /// Kills required to satisfy this objective.
    pub count: i32,
}

/// Cap on simultaneously-active quests per character. Generous for real play
/// (five quests are authored today) but keeps a forged `AcceptQuest` burst
/// from growing unbounded per-connection state, DB rows, and snapshot size.
pub const MAX_ACTIVE: usize = 20;

/// The client's `QuestManager.notify_kill` matching rule, ported verbatim:
/// lowercased substring containment in EITHER direction. "Wolf" matches
/// "Grey Wolf" (target in mob) and "Grey Wolf the Elder" matches "Wolf"
/// (mob in target is the degenerate case). Anchor-tested below.
pub fn kill_matches(objective_target: &str, mob_name: &str) -> bool {
    let t = objective_target.to_lowercase();
    let m = mob_name.to_lowercase();
    t.contains(&m) || m.contains(&t)
}

/// True when `progress` (one running count per objective, same order as
/// `quest.objectives`) satisfies every objective. A short or oversized
/// progress vector is treated as NOT met — a malformed DB row must never
/// unlock a turn-in.
pub fn objectives_met(quest: &Quest, progress: &[i32]) -> bool {
    progress.len() == quest.objectives.len()
        && quest
            .objectives
            .iter()
            .zip(progress)
            .all(|(o, p)| *p >= o.count)
}

/// Fraction of one level (the cubic band at the quest's `level_req`) each
/// difficulty tier pays. Mirrors GDScript `QuestDefinitions.REWARD_TIERS` —
/// change both in the same commit.
fn tier_fraction(tier: &str) -> Option<f64> {
    match tier {
        "trivial" => Some(0.15),
        "standard" => Some(0.30),
        "hard" => Some(0.50),
        "named" => Some(0.80),
        _ => None,
    }
}

/// Parse + validate a quests.toml string. Panics on bad data: the file is
/// compile-time-embedded, so a data bug should fail the very first test run,
/// never limp into a live server that silently miscounts.
fn parse(toml_str: &str) -> HashMap<String, Quest> {
    let file: QuestsFile = toml::from_str(toml_str).expect("data/quests.toml must parse");
    for q in &file.quests {
        assert!(
            !q.objectives.is_empty(),
            "quest {:?} has no objectives — an objective-free quest is a free turn-in",
            q.id
        );
        for o in &q.objectives {
            assert_eq!(
                o.kind, "kill",
                "quest {:?}: unsupported objective kind {:?} (only \"kill\" is counted)",
                q.id, o.kind
            );
            assert!(
                !o.target.trim().is_empty(),
                "quest {:?}: empty objective target would match every mob",
                q.id
            );
            assert!(
                o.count >= 1,
                "quest {:?}: objective count must be >= 1 (got {})",
                q.id,
                o.count
            );
        }
        // Every reward path must resolve in the item registry, or the turn-in
        // would grant nothing — fail here (a test / first boot) instead.
        for path in &q.item_rewards {
            assert!(
                super::items::lookup(path).is_some(),
                "quest {:?}: item_reward {:?} is not a known item (regenerate items.toml)",
                q.id,
                path
            );
        }
    }
    file.quests.into_iter().map(|q| (q.id.clone(), q)).collect()
}

fn table() -> &'static HashMap<String, Quest> {
    static TABLE: OnceLock<HashMap<String, Quest>> = OnceLock::new();
    TABLE.get_or_init(|| parse(QUESTS_TOML))
}

/// Look up a quest by id (`None` = unknown / forged id).
pub fn lookup(quest_id: &str) -> Option<&'static Quest> {
    table().get(quest_id)
}

/// The server-computed XP for completing `quest_id`: tier% of the cubic band
/// at the quest's own `level_req` (fixed per quest, NOT scaled to the
/// turn-in-er's level — a low quest stays "gray" to a high-level character).
/// `None` for an unknown quest id or an unknown tier — the caller rejects.
pub fn xp_reward_for(quest_id: &str) -> Option<i32> {
    let q = lookup(quest_id)?;
    let frac = tier_fraction(&q.reward_tier)?;
    let band = crate::char_data::xp_to_next_for(q.level_req);
    Some((frac * band as f64).round() as i32)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quest_table_parses_and_pays_the_authored_tiers() {
        // Anchors kept in lockstep with the client's QuestDefinitions:
        // standard L1 = 30% of band(1)=1000 -> 300; trivial L1 -> 150;
        // standard L3 = 30% of band(3)=19000 -> 5700;
        // named L5 = 80% of band(5)=61000 -> 48800.
        assert_eq!(xp_reward_for("wolf_threat"), Some(300));
        assert_eq!(xp_reward_for("rat_infestation"), Some(150));
        assert_eq!(xp_reward_for("gnoll_raiders"), Some(5_700));
        assert_eq!(xp_reward_for("rotfang_hunt"), Some(48_800));
        assert_eq!(xp_reward_for("test_q1"), Some(300));
    }

    #[test]
    fn unknown_quest_id_pays_nothing() {
        assert_eq!(xp_reward_for("forged_quest_id"), None);
        assert_eq!(xp_reward_for(""), None);
    }

    #[test]
    fn every_authored_quest_has_valid_objectives() {
        // table() runs the validating parse over the embedded toml; reaching
        // the asserts below means id/kind/target/count all passed.
        for (id, q) in table() {
            assert!(!q.objectives.is_empty(), "quest {id} lost its objectives");
        }
        // Authored anchors, kept in lockstep with quest_definitions.gd.
        let wolf = lookup("wolf_threat").unwrap();
        assert_eq!(wolf.objectives.len(), 1);
        assert_eq!(wolf.objectives[0].target, "Wolf");
        assert_eq!(wolf.objectives[0].count, 5);
        assert_eq!(lookup("rat_infestation").unwrap().objectives[0].count, 8);
        assert_eq!(lookup("gnoll_raiders").unwrap().objectives[0].target, "Gnoll");
        assert_eq!(lookup("rotfang_hunt").unwrap().objectives[0].count, 1);
        assert_eq!(lookup("test_q1").unwrap().objectives[0].target, "Wolf");
    }

    #[test]
    fn authored_item_rewards_resolve_in_the_registry() {
        // table() ran the validating parse, which asserts every item_reward
        // path resolves in items.toml — reaching here means they do. Anchor the
        // wiring (lockstep with client QuestDefinitions.item_rewards).
        assert_eq!(
            lookup("wolf_threat").unwrap().item_rewards,
            vec!["res://data/loot/items/tarnished_silver_ring.tres".to_string()]
        );
        assert_eq!(
            lookup("gnoll_raiders").unwrap().item_rewards,
            vec!["res://data/loot/items/scouts_leather_boots.tres".to_string()]
        );
        assert_eq!(
            lookup("rotfang_hunt").unwrap().item_rewards,
            vec!["res://data/loot/items/hunters_medal.tres".to_string()]
        );
        assert!(lookup("rat_infestation").unwrap().item_rewards.is_empty());
        assert!(lookup("test_q1").unwrap().item_rewards.is_empty());
        // The registry actually knows the reward item.
        assert!(
            crate::world::items::lookup("res://data/loot/items/hunters_medal.tres").is_some()
        );
    }

    #[test]
    fn kill_match_mirrors_the_client_rule() {
        // The client rule (quest_manager.gd notify_kill): lowercased substring
        // containment in either direction. These anchors are the contract —
        // if either side changes, change both in the same commit.
        assert!(kill_matches("Wolf", "Grey Wolf")); // target in mob
        assert!(kill_matches("Grey Wolf", "Wolf")); // mob in target
        assert!(kill_matches("wolf", "WOLF")); // case-insensitive
        assert!(kill_matches("Rotfang", "Rotfang"));
        assert!(!kill_matches("Rat", "Grey Wolf"));
        assert!(!kill_matches("Gnoll", "Rat"));
        // "Rat" is inside "Rotfang"? No — but it IS inside "Giant Rat" and,
        // by the substring rule, also inside "Ratling Warren-Rat". Document
        // the known quirk: substring matching over-matches on compound names.
        assert!(kill_matches("Rat", "Giant Rat"));
        assert!(!kill_matches("Rat", "Rotfang"));
    }

    #[test]
    fn objectives_met_requires_exact_shape_and_full_counts() {
        let q = lookup("wolf_threat").unwrap(); // kill Wolf x5
        assert!(!objectives_met(q, &[])); // short vector = not met
        assert!(!objectives_met(q, &[4]));
        assert!(objectives_met(q, &[5]));
        assert!(objectives_met(q, &[9])); // overshoot is fine
        assert!(!objectives_met(q, &[5, 5])); // oversized vector = malformed, not met
    }

    #[test]
    fn parse_rejects_exploitable_quest_data() {
        use std::panic::catch_unwind;
        // No objectives = free turn-in.
        assert!(catch_unwind(|| parse(
            "[[quest]]\nid = \"q\"\nlevel_req = 1\nreward_tier = \"trivial\"\nobjectives = []\n"
        ))
        .is_err());
        // Missing objectives field entirely (serde requires it).
        assert!(catch_unwind(|| parse(
            "[[quest]]\nid = \"q\"\nlevel_req = 1\nreward_tier = \"trivial\"\n"
        ))
        .is_err());
        // Empty target matches every mob in the zone.
        assert!(catch_unwind(|| parse(
            "[[quest]]\nid = \"q\"\nlevel_req = 1\nreward_tier = \"trivial\"\nobjectives = [ { kind = \"kill\", target = \" \", count = 1 } ]\n"
        ))
        .is_err());
        // Zero count is met before the first kill.
        assert!(catch_unwind(|| parse(
            "[[quest]]\nid = \"q\"\nlevel_req = 1\nreward_tier = \"trivial\"\nobjectives = [ { kind = \"kill\", target = \"Wolf\", count = 0 } ]\n"
        ))
        .is_err());
        // Unknown kind would silently never count.
        assert!(catch_unwind(|| parse(
            "[[quest]]\nid = \"q\"\nlevel_req = 1\nreward_tier = \"trivial\"\nobjectives = [ { kind = \"collect\", target = \"Pelt\", count = 1 } ]\n"
        ))
        .is_err());
        // An item_reward path not in the registry would grant nothing at turn-in.
        assert!(catch_unwind(|| parse(
            "[[quest]]\nid = \"q\"\nlevel_req = 1\nreward_tier = \"trivial\"\nobjectives = [ { kind = \"kill\", target = \"Wolf\", count = 1 } ]\nitem_rewards = [ \"res://data/loot/items/does_not_exist.tres\" ]\n"
        ))
        .is_err());
    }
}
