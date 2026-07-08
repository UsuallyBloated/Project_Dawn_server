//! Server-side quest table — the authoritative source for quest XP rewards.
//!
//! Quests are still *tracked* client-side (objectives, journal), but the
//! reward is server-authored: the client reports only a quest id
//! (`ClientWorldMsg::CompleteQuest`), the server looks it up here, computes
//! the XP itself from its own curve, and records the completion so a quest
//! pays once per character, ever. This closes the PD_W0018-era hole where the
//! client named its own `GrantQuestXp { amount }` (one forged packet was an
//! instant level cap; a relog was an infinite repeat-turn-in faucet).
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

fn table() -> &'static HashMap<String, Quest> {
    static TABLE: OnceLock<HashMap<String, Quest>> = OnceLock::new();
    TABLE.get_or_init(|| {
        let file: QuestsFile =
            toml::from_str(QUESTS_TOML).expect("data/quests.toml must parse");
        file.quests.into_iter().map(|q| (q.id.clone(), q)).collect()
    })
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
}
