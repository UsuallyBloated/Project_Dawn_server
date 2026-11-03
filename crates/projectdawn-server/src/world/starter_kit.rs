//! The starting kit (user, 2026-11-03): every new character is born wearing a
//! Ragged Shirt, Ragged Pants and one Trainee's weapon for its class. All of it
//! is NO DROP and worth nothing (`data/items.toml`), so a character made to be
//! stripped and deleted has nothing that can leave it: the kit cannot be sold,
//! traded, dropped or put in the shared vault. `db::create_character` writes
//! the kit into the worn slots and the item ledger in the same transaction as
//! the character itself.

/// Paperdoll indices (`items::is_equippable_in_slot`): main hand, chest, legs.
pub const MAIN_HAND: u8 = 0;
pub const CHEST: u8 = 3;
pub const LEGS: u8 = 4;

pub const RAGGED_SHIRT: &str = "res://data/loot/items/ragged_shirt.tres";
pub const RAGGED_PANTS: &str = "res://data/loot/items/ragged_pants.tres";
pub const TRAINEES_SWORD: &str = "res://data/loot/items/trainees_sword.tres";
pub const TRAINEES_KNIFE: &str = "res://data/loot/items/trainees_knife.tres";
pub const TRAINEES_CLUB: &str = "res://data/loot/items/trainees_club.tres";
pub const TRAINEES_SPEAR: &str = "res://data/loot/items/trainees_spear.tres";
pub const TRAINEES_STAFF: &str = "res://data/loot/items/trainees_staff.tres";

/// The Trainee's weapon each class starts with (the user's table). `None` for
/// a class not on it, which `create_character` has already refused.
pub fn trainee_weapon(class: &str) -> Option<&'static str> {
    Some(match class {
        "Warrior" | "Paladin" | "Shadow Knight" | "Bard" | "Ranger" | "Witch Hunter" => {
            TRAINEES_SWORD
        }
        "Rogue" | "Blood Mage" => TRAINEES_KNIFE,
        "Cleric" | "Monk" | "Druid" => TRAINEES_CLUB,
        "Shaman" | "Beast Master" => TRAINEES_SPEAR,
        "Magician" | "Wizard" | "Sorcerer" | "Necromancer" | "Enchanter" => TRAINEES_STAFF,
        _ => return None,
    })
}

/// The whole kit as (paperdoll slot, item path), or `None` for an unknown class.
pub fn starter_kit(class: &str) -> Option<[(u8, &'static str); 3]> {
    Some([
        (MAIN_HAND, trainee_weapon(class)?),
        (CHEST, RAGGED_SHIRT),
        (LEGS, RAGGED_PANTS),
    ])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::world::{items, skills};

    /// Every playable class gets a kit it may actually wear: each piece is in
    /// the registry, NO DROP, worth nothing, fits its slot, and passes the
    /// class equip rule (a staff for a class with no 2H Blunt would be
    /// refused at the equip, so the kit would be born worn but unusable).
    #[test]
    fn every_class_can_wear_its_kit() {
        for class in crate::char_data::PLAYABLE_CLASSES {
            let kit = starter_kit(class).unwrap_or_else(|| panic!("{class} has no kit"));
            for (slot, path) in kit {
                let item =
                    items::lookup(path).unwrap_or_else(|| panic!("{path} not in items.toml"));
                assert!(item.no_drop, "{path} must be NO DROP");
                assert_eq!(item.vendor_price, 0, "{path} must be worth nothing");
                assert!(
                    items::is_equippable_in_slot(path, slot),
                    "{path} does not fit slot {slot}"
                );
                assert_eq!(
                    skills::equip_refusal(class, 1, path, slot, ""),
                    None,
                    "{class} cannot wear {path}"
                );
            }
        }
    }

    /// No race and class is born encumbered: the whole kit weighs no more
    /// than the weakest starting character can carry (the Fae, STR -15 on a
    /// caster, carries 5.0, under the old 5.5 cloth kit; docs/design/
    /// item_weight_proposal.md).
    #[test]
    fn no_one_is_born_encumbered() {
        for race in crate::char_data::CREATION_RACES {
            for class in crate::char_data::PLAYABLE_CLASSES {
                let kit = starter_kit(class).expect("kit");
                let weight: f32 = kit
                    .iter()
                    .map(|(_, p)| items::lookup(p).map_or(0.0, |i| i.weight))
                    .sum();
                let strength = crate::char_data::compute(race, class, 1).stats.strength;
                let cap = crate::world::encumbrance::capacity(strength);
                assert!(
                    weight <= cap,
                    "a {race} {class} carries {cap} and is born with {weight}"
                );
            }
        }
    }

    #[test]
    fn the_users_table() {
        assert_eq!(trainee_weapon("Warrior"), Some(TRAINEES_SWORD));
        assert_eq!(trainee_weapon("Rogue"), Some(TRAINEES_KNIFE));
        assert_eq!(trainee_weapon("Blood Mage"), Some(TRAINEES_KNIFE));
        assert_eq!(trainee_weapon("Monk"), Some(TRAINEES_CLUB));
        assert_eq!(trainee_weapon("Beast Master"), Some(TRAINEES_SPEAR));
        assert_eq!(trainee_weapon("Enchanter"), Some(TRAINEES_STAFF));
        assert_eq!(trainee_weapon("Revenant"), None);
    }
}
