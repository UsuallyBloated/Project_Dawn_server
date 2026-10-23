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
    /// The faction whose standing decides whether this NPC serves a player
    /// (`factions.toml`): a vendor or banker refuses at Dubious or below, a
    /// quest giver at Apprehensive or below. None means the NPC serves
    /// everyone (the soul binder, by design).
    #[serde(default)]
    pub faction: Option<String>,
    /// For a vendor: the client's `VendorDefinitions` entry it mirrors.
    #[serde(default)]
    pub vendor_type: Option<String>,
    /// For a vendor: the item names it sells. Nothing else is for sale there.
    #[serde(default)]
    pub stock: Vec<String>,
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
            if let Some(f) = n.faction.as_deref() {
                assert!(
                    super::factions::lookup(f).is_some(),
                    "npcs.toml: unknown faction '{f}' on '{}'",
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

/// An NPC by its id or, failing that, its display name (case-insensitive):
/// the client's NPC nodes carry the name and not the id, so a `Consider`
/// names what the player sees.
pub fn lookup_by_id_or_name(key: &str) -> Option<&'static Npc> {
    lookup(key).or_else(|| table().iter().find(|n| n.name.eq_ignore_ascii_case(key)))
}

/// Is any NPC of `kind` within service range of `pos`? Returns the NPC so the
/// caller can name it in logs.
pub fn any_within_range(kind: &str, pos: Vec3f) -> Option<&'static Npc> {
    table()
        .iter()
        .filter(|n| n.has_kind(kind))
        .find(|n| n.position().distance_to(pos) <= NPC_SERVICE_RANGE)
}

/// Every NPC of `kind` within service range of `pos`, for the faction gate
/// (one vendor may refuse a character another serves).
pub fn all_within_range(kind: &str, pos: Vec3f) -> Vec<&'static Npc> {
    table()
        .iter()
        .filter(|n| n.has_kind(kind))
        .filter(|n| n.position().distance_to(pos) <= NPC_SERVICE_RANGE)
        .collect()
}

/// A vendor within service range of `pos` that `serves` the buyer and stocks
/// `item_name`, if any. The client names no vendor yet (its `vendor_id` is
/// always 0), so any vendor in reach that deals with the buyer and sells the
/// item will do; what matters is that an item nobody stocks, a rare drop with
/// a sell-back price, cannot be bought anywhere, and that a vendor who refuses
/// the buyer on faction cannot be the one who sells.
pub fn vendor_stocking_within_range(
    item_name: &str,
    pos: Vec3f,
    serves: impl Fn(&Npc) -> bool,
) -> Option<&'static Npc> {
    all_within_range("vendor", pos)
        .into_iter()
        .filter(|n| serves(n))
        .find(|n| n.stock.iter().any(|s| s == item_name))
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
        for id in ["brom", "aldric", "sister_maelis", "thalia", "elara", "hadrik"] {
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

    /// Every stocked name must be a real item with a price, or the vendor
    /// lists an item the BuyItem path can never sell.
    #[test]
    fn every_vendor_stocks_only_real_priced_items() {
        for n in table().iter().filter(|n| n.has_kind("vendor")) {
            assert!(!n.stock.is_empty(), "vendor {} has no stock", n.id);
            assert!(n.vendor_type.is_some(), "vendor {} names no vendor_type", n.id);
            for name in &n.stock {
                let item = crate::world::items::lookup_by_name(name)
                    .unwrap_or_else(|| panic!("{}: stocked item '{name}' is not in items.toml", n.id));
                assert!(item.vendor_price > 0, "{}: '{name}' has no vendor_price", n.id);
            }
        }
    }

    #[test]
    fn a_priced_item_nobody_stocks_is_sold_nowhere() {
        let at_brom = Vec3f { x: 8.0, y: 0.0, z: 5.0 };
        assert_eq!(vendor_stocking_within_range("Bread Loaf", at_brom, |_| true).map(|n| n.id.as_str()), Some("brom"));
        // Hadrik (x = -5) is within service range of the spawn, Brom is not
        // the one who sells swords.
        assert!(vendor_stocking_within_range("Iron Short Sword", Vec3f::ZERO, |_| true).is_some());
        // The only vendor who stocks it refuses the buyer (faction): not for sale.
        assert!(vendor_stocking_within_range("Iron Short Sword", Vec3f::ZERO, |n| n.id != "hadrik").is_none());
        // A named drop with a sell-back price.
        assert!(crate::world::items::lookup_by_name("Predator's Collar").is_some_and(|i| i.vendor_price > 0));
        assert!(vendor_stocking_within_range("Predator's Collar", at_brom, |_| true).is_none());
        let dungeon = Vec3f { x: 200.0, y: 0.0, z: 200.0 };
        assert!(vendor_stocking_within_range("Bread Loaf", dungeon, |_| true).is_none());
    }

    #[test]
    fn unknown_ids_are_not_in_range_anywhere() {
        assert!(!id_within_range("no_such_npc", Vec3f::ZERO));
        assert!(lookup("no_such_npc").is_none());
    }
}
