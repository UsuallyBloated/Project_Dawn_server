//! Server-side NPC position table — proximity authority for vendor, bank,
//! quest turn-in and soul-bind operations.
//!
//! Found by the 2026-08-24 dead-intents audit: because an `Interact` intent
//! was never built, the server had no NPC model, so nothing checked *where*
//! the player was standing. Combat, spell targeting, resurrection
//! (`RES_CAST_RANGE`) and loot (`LOOT_PICKUP_RANGE`) all range-check; vendor,
//! bank and quest turn-in did not — a modified client could bank its whole
//! inventory from a dungeon one second before dying, voiding the gear-loss
//! half of the death penalty. Not a mint (no unauthorised items/coins/XP),
//! but it nullified a designed consequence.
//!
//! Same `OnceLock` + `include_str!` pattern as `items.rs` / `named.rs`.

use super::connection::Vec3f;
use serde::Deserialize;
use std::sync::OnceLock;

const NPCS_TOML: &str = include_str!("../../data/npcs.toml");

/// Server-side service range, deliberately looser than the client's 6 m UI
/// gate. This is a backstop against *remote* abuse (tens to hundreds of
/// metres away), not a UI: it must never refuse an honest player over
/// position interpolation or a step of drift, so it is sized to the town
/// plaza — the starter spawn sits ~12 m from the banker, and every NPC is
/// within 15 m of it. An exploiter's position is not 12 m out; it is 50+.
pub const NPC_SERVICE_RANGE: f32 = 15.0;

#[derive(Debug, Clone, Deserialize)]
pub struct Npc {
    pub id: String,
    pub name: String,
    pub kinds: Vec<String>,
    pub pos: [f32; 3],
}

impl Npc {
    pub fn position(&self) -> Vec3f {
        Vec3f { x: self.pos[0], y: self.pos[1], z: self.pos[2] }
    }

    pub fn has_kind(&self, kind: &str) -> bool {
        self.kinds.iter().any(|k| k == kind)
    }
}

#[derive(Debug, Deserialize)]
struct NpcsFile {
    npc: Vec<Npc>,
}

fn table() -> &'static Vec<Npc> {
    static NPCS: OnceLock<Vec<Npc>> = OnceLock::new();
    NPCS.get_or_init(|| {
        let parsed: NpcsFile =
            toml::from_str(NPCS_TOML).expect("npcs.toml must parse — fix the embedded file");
        for n in &parsed.npc {
            for k in &n.kinds {
                assert!(
                    matches!(k.as_str(), "vendor" | "banker" | "quest" | "soul_binder"),
                    "npcs.toml: unknown kind '{k}' on '{}'",
                    n.id
                );
            }
        }
        parsed.npc
    })
}

pub fn lookup(id: &str) -> Option<&'static Npc> {
    table().iter().find(|n| n.id == id)
}

/// Is any NPC of `kind` within service range of `pos`? Returns the NPC so the
/// caller can name it in logs.
pub fn any_within_range(kind: &str, pos: Vec3f) -> Option<&'static Npc> {
    table()
        .iter()
        .filter(|n| n.has_kind(kind))
        .find(|n| n.position().distance_to(pos) <= NPC_SERVICE_RANGE)
}

/// Is the specific NPC `id` within service range of `pos`? Used for quest
/// turn-ins, which name their NPC rather than accepting any of a kind.
pub fn id_within_range(id: &str, pos: Vec3f) -> bool {
    lookup(id).is_some_and(|n| n.position().distance_to(pos) <= NPC_SERVICE_RANGE)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embedded_toml_parses_and_kinds_are_valid() {
        assert!(!table().is_empty(), "npcs.toml produced no NPCs");
    }

    #[test]
    fn the_four_town_npcs_exist_with_their_roles() {
        assert!(lookup("brom").expect("brom").has_kind("vendor"));
        assert!(lookup("brom").expect("brom").has_kind("quest"), "Brom also gives quests");
        assert!(lookup("aldric").expect("aldric").has_kind("quest"));
        assert!(lookup("sister_maelis").expect("maelis").has_kind("soul_binder"));
        assert!(lookup("thalia").expect("thalia").has_kind("banker"));
    }

    /// The starter spawn must be inside the service range of every town NPC,
    /// or a fresh character standing at spawn would be refused — and the
    /// integration tests, which buy and sell from spawn, would refuse with
    /// them. This is the constraint that sized NPC_SERVICE_RANGE.
    #[test]
    fn starter_spawn_reaches_every_town_npc() {
        let spawn = Vec3f::ZERO;
        for id in ["brom", "aldric", "sister_maelis", "thalia"] {
            let n = lookup(id).unwrap();
            assert!(
                n.position().distance_to(spawn) <= NPC_SERVICE_RANGE,
                "{id} is out of service range from the starter spawn"
            );
        }
    }

    #[test]
    fn range_gate_accepts_near_and_refuses_far() {
        let at_banker = Vec3f { x: 11.0, y: 0.0, z: 5.0 };
        assert!(any_within_range("banker", at_banker).is_some());

        let dungeon = Vec3f { x: 200.0, y: -10.0, z: 200.0 };
        assert!(any_within_range("banker", dungeon).is_none(), "the exploit case");
        assert!(any_within_range("vendor", dungeon).is_none());
        assert!(!id_within_range("aldric", dungeon));
    }

    #[test]
    fn unknown_ids_are_not_in_range_anywhere() {
        assert!(!id_within_range("no_such_npc", Vec3f::ZERO));
        assert!(lookup("no_such_npc").is_none());
    }
}
