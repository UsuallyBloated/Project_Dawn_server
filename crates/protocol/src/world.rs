//! World-channel messages (bincode over UDP/renet).
//!
//! These types compile but the world server is not yet wired up. They're
//! the source of truth for the wire format whenever it lands. Adding,
//! removing, or reordering variants is a protocol-version bump.
//!
//! Sessions: world packets carry the same 32-byte token issued by the
//! auth service, but as raw bytes instead of hex (saves 50%).

use serde::{Deserialize, Serialize};

/// renet `protocol_id` — bumped on any wire-format break.
/// Auth-minted ConnectTokens are signed with this; mismatch ⇒ token rejected.
///
/// PD_W0005 covers the Track 6 batch: server-authoritative player stats. The
/// server now owns HP/MP/Stamina (loaded from DB at spawn, mutated by regen
/// + combat + PvP, fanned out as `HealthUpdate` / `ManaUpdate` /
/// `StaminaUpdate`); `ClientWorldMsg::ResourceUpdate` is removed because the
/// authority flips and the client no longer broadcasts resources. One bump
/// per track; individual sub-task commits append new variants under this id.
///
/// PD_W0013: four-tier currency. `CoinsUpdate` now carries a `Coins`
/// (platinum/gold/silver/copper) instead of a single `i64`, a wire break.
///
/// PD_W0014: group loot rights + coin drops (client
/// docs/design/group_loot_and_coin.md). Adds the per-player `SetAutosplit`
/// intent; the rest of the track's wire bits (loot-mode / coin display /
/// roster mode / reject feedback) append variants under this same id as
/// they land. One bump for the whole track — client + server must rebuild
/// together.
///
/// PD_W0015: Banker NPC, slice 1 (coins). Adds the per-character bank wallet
/// over the wire — `BankDepositCoins` / `BankWithdrawCoins` / `BankExchange`
/// client intents and `BankSnapshot` / `BankRejected` server→client messages.
/// New variants append at the END of each enum (bincode encodes by positional
/// discriminant, so appending keeps every existing variant stable).
///
/// PD_W0016: Banker NPC, slice 2 (item storage). Adds the per-character item
/// vault (10 slots) and the account-shared vault (2 slots). New client intents
/// `BankStoreItem` / `BankWithdrawItem` and the server message `BankItemSnapshot`,
/// all appended at the END of their enums.
///
/// PD_W0017: Camp + linkdead, slice B (`/camp`). Adds the voluntary sit-gated
/// logout: client intents `Camp` / `CancelCamp` and the server confirm
/// `CampUpdate { remaining_secs, active }`, all appended at the END of their
/// enums.
///
/// PD_W0018: server-authoritative XP + leveling (corpse / resurrection epic,
/// Slice 0). The inert `LevelUp` is reshaped to carry `xp` / `xp_to_next`
/// alongside `new_level` (it had no encoder, so reshaping in place is safe),
/// `XpGained` now carries real `current` / `to_next`, and the client intent
/// `GrantQuestXp { amount }` (appended at the END of `ClientWorldMsg`) lets
/// client-tracked quests award xp through the server's authoritative path.
///
/// PD_W0019: corpse / resurrection Slice 1. Appends `ServerWorldMsg::CorpseSpawn
/// { corpse_id, owner_id, owner_name, pos }` so the client renders a player
/// corpse (body + "<name>'s corpse" nameplate) on death or when one loads into
/// AOI at boot. Despawn rides the existing `EntityDespawn`; corpse looting is
/// Slice 2.
///
/// PD_W0020: corpse / resurrection Slice 2 (loot your own corpse). Appends
/// `ServerWorldMsg::CorpseContents { corpse_id, items, coins }`, sent privately to
/// the owner only so the client can drive a loot window; taking reuses the
/// existing `LootItem` / `LootAll` intents keyed by corpse_id (no new client
/// intent). No wire change to those intents.
///
/// PD_W0021: monster orb -> corpse. Appends `creature_name: String` to the END of
/// the existing `LootBagSpawn` variant (append-only-safe — bincode encodes a
/// struct variant's fields positionally), so the client renders a dead-body
/// visual with a "<creature>'s corpse" nameplate instead of a golden orb. Empty
/// for a player-dropped public bag (no creature died -> keep the sack look).
///
/// PD_W0022: corpse / resurrection Slice 3 (Cleric + Paladin resurrection).
/// Appends `ClientWorldMsg::ResurrectAccept { corpse_id, accept }`, and
/// `ServerWorldMsg::ResurrectOffer { corpse_id, caster_name, xp_percent }` (sent
/// privately to the corpse owner) + a generic `ServerWorldMsg::Teleport { pos }`
/// that summons the owner to their corpse — all appended at the END of their enums.
///
/// PD_W0023: quests go online + server-authoritative. Appends
/// `ServerWorldMsg::KillCredit { mob_name }` (private quest kill credit to
/// whoever earned a kill — never the EntityDied broadcast, so bystanders get
/// none), `ClientWorldMsg::DevSpawnMob { .. }` (dev-gated Test Panel world-mob
/// spawn), and `ClientWorldMsg::CompleteQuest { quest_id }` (quest turn-in by
/// id; the server computes the reward from its own quest table and pays once
/// per character, ever — replaces `GrantQuestXp` for quests, which is now
/// dev-gated like HealSelf).
///
/// PD_W0024: quest phase 2 — objective STATE moves server-side. The server's
/// quest table gains per-quest objectives, kills are counted server-side in
/// `active_quests` (survives relog/restart), and turn-in requires every
/// objective met (closes the forged-`CompleteQuest`-without-kills hole).
/// Wires up the dormant scaffold variants `ClientWorldMsg::AcceptQuest` /
/// `AbandonQuest` (mid-enum, never sent before, so reuse is positionally
/// safe) and appends `ServerWorldMsg::QuestSnapshot` (journal seed on
/// EnterWorld) / `QuestProgress` (private per-increment) / `QuestRejected`
/// (visible accept/turn-in feedback) / `QuestCompleted` (turn-in success
/// confirm) at the END of that enum. `KillCredit` stays in the enum but is
/// no longer sent: `QuestProgress` replaces it as the journal driver.
///
/// PD_W0025: server-authoritative weapon procs. Appends
/// `ServerWorldMsg::ProcTriggered { attacker, target, proc_name, amount, crit,
/// dmg_type }` at the END of that enum — the server now rolls a weapon's
/// proc_chance on a landed melee swing and applies proc_damage itself (folded
/// into the same swing's death cascade), announcing it via this message so the
/// client renders the named "<proc> for N" hit. Replaces the old client-driven
/// proc (a second Attack the client sent, which double-hit and is now dropped by
/// the swing-rate limit).
///
/// Note: this ID is a wire *marker*, not the connection gate. The client takes
/// its `protocol_id` from the server-minted ConnectToken, so bumping it here does
/// NOT by itself refuse a stale client — an old client still connects and simply
/// ignores the unknown `ProcTriggered` (decode returns `Raw`, no crash). To
/// actually refuse an out-of-date client, raise the auth `min_client_version` at
/// deploy time alongside the new client build.
///
/// PD_W0026: `ConnectOk` gains `is_gm`, so the client learns its GM status from
/// the world handshake instead of caching it from the auth `LoginOk`.
/// PD_W0027: the cursor slot. `LootToCursor` joins the client intents, and
/// `"cursor"` becomes a valid location string on `MoveItem` / `EquipItem` /
/// `DropItem` / `UseConsumable` / `InventoryDelta` (strings, so no shape
/// change — the bump exists because an old server would drop the new intent
/// and an old client would not understand a cursor delta).
pub const WORLD_PROTOCOL_ID: u64 = 0x5044_5f57_3030_3237; // "PD_W0027"

pub type EntityId = u64;

/// A player wallet: four independent coin stacks at 100:1 ratios
/// (100 copper = 1 silver, 100 silver = 1 gold, 100 gold = 1 platinum).
///
/// The stacks are independent on purpose. A player may choose to hold raw
/// copper rather than its reduced form, and that choice carries a real weight
/// cost (encumbrance). Nothing here silently consolidates held coin: `spend`
/// breaks a higher coin into change only when the lower stacks can't cover the
/// cost, and `add_payout` deposits a vendor payout already reduced on *top* of
/// existing stacks. Wholesale re-minting is an explicit moneychanger action,
/// never a side effect of buying or selling.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Coins {
    pub platinum: i64,
    pub gold: i64,
    pub silver: i64,
    pub copper: i64,
}

impl Coins {
    /// Copper-equivalent value of one coin of each tier, indexed copper→platinum.
    const TIER_VALUE: [i64; 4] = [1, 100, 10_000, 1_000_000];

    pub const ZERO: Coins = Coins { platinum: 0, gold: 0, silver: 0, copper: 0 };

    /// Total wallet value expressed in copper.
    pub fn total_copper(self) -> i64 {
        self.copper
            .saturating_add(self.silver.saturating_mul(Self::TIER_VALUE[1]))
            .saturating_add(self.gold.saturating_mul(Self::TIER_VALUE[2]))
            .saturating_add(self.platinum.saturating_mul(Self::TIER_VALUE[3]))
    }

    /// The fully-reduced (minimal-coin) representation of a copper amount.
    /// Used for fresh payouts and as a correctness fallback — never to
    /// silently rewrite a player's existing holdings.
    pub fn from_copper(mut amount: i64) -> Coins {
        if amount < 0 {
            amount = 0;
        }
        let platinum = amount / Self::TIER_VALUE[3];
        amount %= Self::TIER_VALUE[3];
        let gold = amount / Self::TIER_VALUE[2];
        amount %= Self::TIER_VALUE[2];
        let silver = amount / Self::TIER_VALUE[1];
        amount %= Self::TIER_VALUE[1];
        Coins { platinum, gold, silver, copper: amount }
    }

    pub fn can_afford(self, cost_copper: i64) -> bool {
        cost_copper <= self.total_copper()
    }

    /// Copper value of one coin of `tier` (0 = copper … 3 = platinum).
    pub const fn tier_value(tier: u8) -> i64 {
        Self::TIER_VALUE[tier as usize]
    }

    /// Count held in `tier` (0 = copper … 3 = platinum).
    pub fn tier_count(self, tier: u8) -> i64 {
        match tier {
            0 => self.copper,
            1 => self.silver,
            2 => self.gold,
            _ => self.platinum,
        }
    }

    /// True if any tier is negative. Used to reject malformed client amounts
    /// (a deposit/withdraw of a negative stack would otherwise mint coin).
    pub fn has_negative(self) -> bool {
        self.platinum < 0 || self.gold < 0 || self.silver < 0 || self.copper < 0
    }

    /// Per-tier "holds at least `other` in every tier" — the affordability
    /// check before a deposit/withdraw of specific tier amounts.
    pub fn has_at_least(self, other: Coins) -> bool {
        self.platinum >= other.platinum
            && self.gold >= other.gold
            && self.silver >= other.silver
            && self.copper >= other.copper
    }

    /// Per-tier sum — credit specific tier amounts on top of existing stacks
    /// without re-reducing (unlike `add_payout`). For bank deposit/withdraw.
    /// Saturating to match the overflow-safe discipline of `total_copper` /
    /// `add_payout` (a tier never wraps to a negative balance).
    /// Pay the VALUE of `want` out of this purse, whatever tiers that takes.
    /// Returns false and leaves the purse untouched if it cannot be afforded.
    ///
    /// The four tiers are independent stacks, so a plain per-tier check refuses
    /// to pay 6 silver from a purse holding 5 silver and 10 gold, and refuses to
    /// pay 1 gold from a purse of 100 silver. Both read to a player as a bug
    /// rather than as a rule. Money is money: what matters is whether the purse
    /// is worth enough, and `spend` already works out the coins, breaking a
    /// larger one and scattering the change back down when it has to. 10 gold
    /// paying 5 silver leaves 9 gold 95 silver.
    ///
    /// The distinction that does matter is CONSENT, not direction. Converting
    /// coin the player asked to move is just carrying out the request. Silently
    /// re-denominating coin they are merely holding is not, which is why corpse
    /// loot returns its tiers exactly as they were left.
    pub fn pay_value_of(&mut self, want: Coins) -> bool {
        if want.has_negative() {
            return false;
        }
        self.spend(want.total_copper())
    }

    pub fn add_each(self, other: Coins) -> Coins {
        Coins {
            platinum: self.platinum.saturating_add(other.platinum),
            gold: self.gold.saturating_add(other.gold),
            silver: self.silver.saturating_add(other.silver),
            copper: self.copper.saturating_add(other.copper),
        }
    }

    /// Per-tier difference; caller must ensure `has_at_least(other)` first.
    /// Saturating for the same overflow-safety reason as `add_each`.
    pub fn sub_each(self, other: Coins) -> Coins {
        Coins {
            platinum: self.platinum.saturating_sub(other.platinum),
            gold: self.gold.saturating_sub(other.gold),
            silver: self.silver.saturating_sub(other.silver),
            copper: self.copper.saturating_sub(other.copper),
        }
    }

    /// Banker tier exchange (slice 1): convert up to `qty` coins of `from_tier`
    /// into `to_tier` in place, value-preserving (0% fee for MVP). Up-conversions
    /// convert the whole-multiple part and leave any remainder in `from_tier`
    /// (150 copper → 1 silver + 50 copper); down-conversions always divide
    /// evenly. Touches only the two tiers involved — no silent consolidation of
    /// the rest of the wallet. Returns Err (wallet untouched) on same/invalid
    /// tier, zero qty, insufficient `from_tier`, or too little to make even one
    /// target coin.
    pub fn exchange(&mut self, from_tier: u8, to_tier: u8, qty: u32) -> Result<(), &'static str> {
        if from_tier == to_tier {
            return Err("pick two different coin tiers");
        }
        if from_tier > 3 || to_tier > 3 {
            return Err("invalid coin tier");
        }
        if qty == 0 {
            return Err("nothing to convert");
        }
        let qty = qty as i64;
        if self.tier_count(from_tier) < qty {
            return Err("not enough of that coin to convert");
        }
        let from_v = Self::TIER_VALUE[from_tier as usize];
        let to_v = Self::TIER_VALUE[to_tier as usize];
        let value = qty.saturating_mul(from_v);
        let to_count = value / to_v;
        if to_count == 0 {
            // Less than one target coin's worth (e.g. 50 copper → silver).
            return Err("not enough of that coin to make even one of the target");
        }
        // Convert the whole-multiple part; the remainder (always a whole number
        // of `from_tier` coins) stays put. Value is preserved exactly (0% fee
        // for MVP — a future fee would deduct from `to_count` here per
        // currency.md's bands).
        let remainder_in_from = (value % to_v) / from_v;
        let consumed = qty - remainder_in_from;
        self.adjust_tier(from_tier, -consumed);
        self.adjust_tier(to_tier, to_count);
        Ok(())
    }

    fn adjust_tier(&mut self, tier: u8, delta: i64) {
        match tier {
            0 => self.copper = self.copper.saturating_add(delta),
            1 => self.silver = self.silver.saturating_add(delta),
            2 => self.gold = self.gold.saturating_add(delta),
            _ => self.platinum = self.platinum.saturating_add(delta),
        }
    }

    /// Spend `cost` copper-equivalents, disturbing the wallet as little as
    /// possible: spend low coins first (shedding heavy copper), breaking a
    /// single higher coin into change only when the lower stacks fall short.
    /// A deliberate copper hoard is therefore left intact by unrelated
    /// purchases. Returns false and leaves the wallet untouched if the player
    /// can't afford it.
    pub fn spend(&mut self, cost: i64) -> bool {
        let total = self.total_copper();
        if cost < 0 || total < cost {
            return false;
        }
        // Mutable working copy indexed copper→platinum; commit only on success.
        let mut counts = [self.copper, self.silver, self.gold, self.platinum];
        let mut remaining = cost;
        for i in 0..4 {
            if remaining == 0 {
                break;
            }
            let val = Self::TIER_VALUE[i];
            let whole = (remaining / val).min(counts[i]);
            counts[i] -= whole;
            remaining -= whole * val;
            // Sub-`val` remainder: break one coin of this tier and scatter the
            // change back down into the lower tiers.
            if remaining > 0 && remaining < val && counts[i] > 0 {
                counts[i] -= 1;
                let mut change = val - remaining;
                remaining = 0;
                for j in (0..i).rev() {
                    counts[j] += change / Self::TIER_VALUE[j];
                    change %= Self::TIER_VALUE[j];
                }
            }
        }
        if remaining > 0 {
            // Greedy couldn't settle (shouldn't happen once affordable);
            // guarantee correctness by reducing the remainder.
            *self = Coins::from_copper(total - cost);
            return true;
        }
        self.copper = counts[0];
        self.silver = counts[1];
        self.gold = counts[2];
        self.platinum = counts[3];
        true
    }

    /// Deposit a vendor/quest payout — reduced to minimal coins — on top of the
    /// existing stacks. Existing stacks are not re-reduced.
    pub fn add_payout(&mut self, amount_copper: i64) {
        let p = Coins::from_copper(amount_copper.max(0));
        self.platinum = self.platinum.saturating_add(p.platinum);
        self.gold = self.gold.saturating_add(p.gold);
        self.silver = self.silver.saturating_add(p.silver);
        self.copper = self.copper.saturating_add(p.copper);
    }
}
pub type Sequence = u32;

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Vec3 {
    pub x: f32,
    pub y: f32,
    pub z: f32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[repr(u8)]
pub enum DamageType {
    Physical = 0,
    Fire,
    Ice,
    Lightning,
    Arcane,
    Holy,
    Nature,
    Spirit,
    Shadow,
    Poison,
}

/// Track 18.1 — passive skill kind. Server fans
/// `SkillProgressUpdate` per advance and `SkillProgressSnapshot` once
/// on enter-world to seed the client's display cache. Mirrors the
/// three GDScript autoloads (WeaponSkills, ArmorSkills, CastingSkills).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[repr(u8)]
pub enum SkillKind {
    Weapon = 0,
    Armor = 1,
    Casting = 2,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ChatChannel {
    Say,
    Ooc,
    Group,
    Tell,
    Guild,
    Raid,
    Auction,
    System,
    // Added in PD_W0011. Kept at the end so existing variants' bincode
    // discriminants stay stable.
    Shout,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum EquipSlot {
    Weapon,
    Offhand,
    Head,
    Chest,
    Legs,
    Feet,
    Hands,
    Ring,
    Neck,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SlotRef {
    BaseSlot { idx: u8 },
    BagSlot { base: u8, slot: u8 },
    EquipSlot(EquipSlot),
}

// ─── Client → Server ─────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ClientWorldMsg {
    // Connection
    Connect {
        session_token: [u8; 32],
        char_id: u64,
        client_version: String,
    },
    Disconnect,
    Heartbeat,

    // Movement
    Move {
        sequence: Sequence,
        direction: Vec3,
        jumping: bool,
    },

    // Combat (intent only — server resolves outcomes)
    SetTarget {
        target_id: Option<EntityId>,
    },
    /// Track 6 sub-task 2 — player → server attack intent. The client
    /// no longer computes the damage roll; it picks the target and
    /// reports which weapon is equipped (so the server can read
    /// damage_min/max + skill from its items table) and whether the
    /// swing came from the main hand or offhand. The server runs the
    /// full formula (STR/DEX bonus + weapon range + crit + offhand
    /// multiplier) and fans `Hit` with the authoritative amount. An
    /// empty `weapon_path` or unknown path falls back to bare-handed
    /// damage (1-4 + STR bonus).
    Attack {
        target_id: EntityId,
        weapon_path: String,
        is_offhand: bool,
        dmg_type: DamageType,
    },
    /// Track 6 sub-task 3b — server-authoritative spell cast. Carries
    /// `spell_name` (lookup key for the server's spells.toml table)
    /// and the chosen target id. SELF target ignores target_id; ENEMY /
    /// AOE require a valid target. Server validates mana cost +
    /// target type + range + target liveness, applies the
    /// authoritative damage / heal, and fans Hit + HealthUpdate /
    /// HealthUpdate(self) + ManaUpdate. `spell_id` is reserved for
    /// future numeric-id resolution; today the server keys by name.
    CastSpell {
        spell_name: String,
        target_id: Option<EntityId>,
    },
    UseSkill {
        skill_id: u32,
        target_id: Option<EntityId>,
    },
    CancelCast,

    // Track 13.2 inventory ops live further down. The scaffolded
    // SlotRef-based MoveItem / EquipItem / UnequipItem / DropItem
    // variants were never wired (the GDScript side has no bincode
    // encoder for tagged enums); Track 13.2 uses string-based
    // locations instead.
    UseConsumable {
        slot: SlotRef,
    },
    StackAll,

    // World interaction
    Interact {
        entity_id: EntityId,
    },
    DialogueResponse {
        node_id: String,
        choice_idx: u32,
    },
    BuyItem {
        vendor_id: EntityId,
        item_name: String,
        qty: u32,
    },
    SellItem {
        slot: SlotRef,
        qty: u32,
    },
    LootItem {
        bag_id: EntityId,
        slot: u32,
    },
    LootAll {
        bag_id: EntityId,
    },
    /// PD_W0027 — ground pickup: take the ONLY item stack in a loot bag
    /// onto the cursor slot (the item rides the mouse until placed).
    /// The server refuses unless the bag holds exactly one stack and
    /// zero coin, the looter's cursor is empty, and the same range /
    /// loot-rights / round-robin gates as `LootItem` pass.
    LootToCursor {
        bag_id: EntityId,
    },

    // Quests
    /// PD_W0024 — accept a quest so the server starts counting its objectives
    /// (a dormant scaffold variant until then; reusing it is positionally
    /// safe because it was never sent). Sent when the player takes a quest
    /// from NPC dialogue. The server validates (known id, level_req met, not
    /// already active, not already completed, active-quest cap) and on
    /// failure answers `QuestRejected`; success is silent (the client already
    /// added the quest optimistically). `giver_id` is unused for now — NPCs
    /// aren't server entities yet; send 0 (giver/proximity validation comes
    /// with the faction system).
    AcceptQuest {
        quest_id: String,
        giver_id: EntityId,
    },
    /// PD_W0024 — drop an active quest: the server forgets the quest and its
    /// objective progress (re-accepting later starts from zero; the permanent
    /// completion record is untouched, so no repeat payout opens up). Also a
    /// reused dormant scaffold variant.
    AbandonQuest {
        quest_id: String,
    },
    /// Dormant scaffold — never sent. The live turn-in is `CompleteQuest`
    /// (PD_W0023, tail of this enum). Kept only for positional stability.
    TurnInQuest {
        quest_id: String,
        npc_id: EntityId,
    },

    // Crafting & gathering
    StartCombine {
        recipe_id: String,
        station_id: EntityId,
    },
    StartMining {
        node_id: EntityId,
    },
    StartSkinning {
        corpse_id: EntityId,
    },

    // Social
    Chat {
        channel: ChatChannel,
        text: String,
        // Only populated for `ChatChannel::Tell`. Server looks the
        // target up by name and fans `ChatMessage` to that one
        // connection. Other channels ignore this field.
        target_name: Option<String>,
    },
    /// Request the equipment snapshot of another in-world player. Server
    /// validates source is in-world, looks up the target by char_id,
    /// and replies privately with `InspectResult`. Bag contents are not
    /// included — only the paperdoll slots are public.
    InspectPlayer {
        target_char_id: i64,
    },
    Sit,
    Stand,
    BindAtCurrentLocation,

    // Group
    GroupInvite {
        name: String,
    },
    GroupAcceptInvite {
        from: u64,
    },
    GroupLeave,
    GroupKick {
        name: String,
    },

    /// Track 12 Piece A — player issues a command to their pet.
    /// `command` is one of the values in `pet_command` (Attack=2,
    /// Back=3 are the MVP set; Follow/Guard/Sit reserved for future
    /// behaviours). `target_id` is required for Attack (enemy id);
    /// ignored for the others.
    PetCommand {
        command: u8,
        target_id: Option<EntityId>,
    },

    /// Track 13.2 — player requests to move an inventory entry from
    /// `src` to `dst`. The wire shape uses string `location` to
    /// stay future-proof against bag / equip locations added in
    /// later sub-tasks; for 13.2 only `'base'` is honoured server-
    /// side. The server validates source occupancy + destination
    /// empty / same-stack semantics, mutates, and fans
    /// `InventoryDelta` for each affected slot.
    MoveItem {
        src_location: String,
        src_slot: u32,
        dst_location: String,
        dst_slot: u32,
    },

    /// Track 13.2.b — split `count` items off the src stack into
    /// dst. Dst must be empty or hold the same item_path; on merge
    /// the dst count saturates rather than overflowing.
    SplitStack {
        src_location: String,
        src_slot: u32,
        dst_location: String,
        dst_slot: u32,
        count: u32,
    },

    /// Track 13.2.b — drop `count` of the entry at `(location, slot)`
    /// at the player's feet. `count == 0` drops the whole stack.
    /// Server creates a single-stack `LootBag` at the caster's pos
    /// and fans `LootBagSpawn` via the existing FFA loot pipeline.
    DropItem {
        location: String,
        slot: u32,
        count: u32,
    },

    /// Track 15.1 — destroy `count` of the entry at `(location, slot)`
    /// outright. `count == 0` destroys the whole stack. Distinct from
    /// `DropItem`: no loot bag is spawned, the item is gone. Used by
    /// the trash cell / Destroy button UI to remove items the player
    /// doesn't want without polluting the ground with despawning bags.
    DestroyItem {
        location: String,
        slot: u32,
        count: u32,
    },

    /// Track 13.3 — equip the item at `(src_location, src_slot)`
    /// into paperdoll slot `equip_slot`. `equip_slot` matches the
    /// existing `protocol::world::EquipSlot` enum order (weapon=0,
    /// offhand=1, head=2, chest=3, legs=4, feet=5, hands=6, ring=7,
    /// neck=8). If the paperdoll slot is occupied, the existing
    /// item is moved into `src_slot` (swap).
    EquipItem {
        src_location: String,
        src_slot: u32,
        equip_slot: u8,
    },

    /// Track 13.3 — unequip the item in paperdoll slot `equip_slot`
    /// into `(dst_location, dst_slot)`. If dst is occupied, the
    /// swap moves the dst item into the paperdoll slot (subject to
    /// the same byte-range validation — item-vs-slot validation is
    /// deferred until the server-side item registry lands).
    UnequipItem {
        equip_slot: u8,
        dst_location: String,
        dst_slot: u32,
    },

    // GM
    GmCommand {
        line: String,
    },

    // (Track 6 removed `ResourceUpdate`. The server now owns HP/MP/Stamina:
    // resources load from DB at character spawn, regen ticks on the server,
    // damage / heals route through Attack / CastSpell / UseSkill intents.
    // Server fans out `HealthUpdate` / `ManaUpdate` / `StaminaUpdate` on
    // every threshold-crossing change. Clients render only — the typed
    // signals on the client are unchanged.)

    // Sent by the client when the player clicks "Enter World" in the lobby
    // (i.e. the game scene actually loads). Server gates EntitySpawn fan-out
    // on this — peers don't see a player's body until they've left the
    // lobby. App-Connect alone is no longer enough to render to peers, but
    // it's still enough to receive ConnectOk and prepare PlayerStats.
    EnterWorld,

    // Track 4 sub-task 2: owning-client → server broadcasts of the local
    // player's casting state. Server relays as CastStart / CastComplete /
    // CastFail on the reliable system channel. Targeted peers render a
    // cast bar in their HUD target frame for the duration. CastFail covers
    // both cancel-by-interrupt and cancel-by-movement; "reason" is
    // human-readable but not currently displayed.
    CastStartBroadcast {
        spell_name: String,
        duration: f32,
    },
    CastCompleteBroadcast {
        spell_name: String,
    },
    CastFailBroadcast {
        reason: String,
    },

    // Track 4 sub-task 3: full buff list snapshot. Fired on every
    // BuffManager.buffs_changed. Snapshot rather than add/remove deltas
    // — simpler logic, can't desync if a message ever drops. ~10 buffs ×
    // ~30 B each = ~300 B per change on the reliable channel; trivial.
    BuffSnapshotBroadcast {
        buffs: Vec<(String, f32)>,
    },

    // Track 4 sub-task 4: attacker broadcasts a hit/miss/evade result so
    // peers can render floating damage / "MISS" / "EVADE" text over the
    // target. Combat math stays client-local (the attacker computes the
    // outcome); these messages are pure visualization fan-out. PvP damage
    // application is Track 6 — for now the target's HP doesn't change as
    // a result of receiving these.
    HitBroadcast {
        target: EntityId,
        amount: i32,
        crit: bool,
        dmg_type: DamageType,
    },
    MissBroadcast {
        target: EntityId,
    },
    EvadeBroadcast {
        target: EntityId,
    },

    // Track 4 sub-task 5 / Track 6: the dying client notifies the server
    // that its HP hit zero. Server zeroes conn.hp (fans HealthUpdate(0)
    // so peer HUDs / RemotePlayer bars drop) and fans EntityDied to
    // in_world peers. The respawning client follows up with `Respawn`
    // once its local respawn timer elapses (Track 6 split the variants
    // because the resource-fanout-on-respawn used to ride ResourceUpdate
    // and that's removed now).
    DeathBroadcast,
    /// Track 6: the dying client's local respawn timer has elapsed and
    /// it's alive again. Server resets conn.hp/mp/stamina to the
    /// authoritative max values from the DB and fans HealthUpdate /
    /// ManaUpdate / StaminaUpdate so peer RemotePlayer bars stand back
    /// up. No payload — the server owns the max values and the timing.
    /// Sub-task 3 will lift death/respawn detection fully server-side
    /// (PvP death + timer-driven revive), at which point this becomes
    /// either an ACK or goes away.
    Respawn,

    /// Track 6 sub-task 3 — client tells the server its current total
    /// armor class (AGI/4 + sum of equipped armor with skill bonuses)
    /// whenever equipment changes. Server caches per-PerConnection and
    /// applies AC/(AC+100) reduction to incoming damage in the same
    /// shape `autoloads/combat.gd::receive_player_damage` uses. Cheaty
    /// (client can claim 9999 armor) but matches Track 6's transitional
    /// trust model; full server-side equipment + skill tracking lands
    /// with inventory authority.
    EquipUpdate {
        armor: i32,
    },

    /// Track 6 sub-task 3 — dev /pvp toggle. Flips the
    /// `pvp_override_on` flag on the sender's PerConnection. Two
    /// players with the flag on can damage each other via the
    /// `combat::can_attack` chokepoint; otherwise PvP attacks fan Miss.
    /// Future duel-accept / PvP-zone / PvP-server rules will layer over
    /// the same flag.
    PvpToggle {
        on: bool,
    },

    /// Track 6 sub-task 3 dev intent — client requests the server to
    /// subtract `amount` from its own HP. Routes through the same
    /// damage path PvP and (future) spell-damage use so the regen
    /// fan-out + death detection are exercised end-to-end. Cheaty by
    /// definition; existing only behind the /damage chat command.
    DamageSelf {
        amount: i32,
    },

    /// Track 6 sub-task 3 dev intent — client requests the server to
    /// add `amount` to its own HP (capped at max_hp). Mirror of
    /// `DamageSelf` for verifying that server-applied heals stick. Will
    /// be replaced by proper `CastSpell` handling once the spell table
    /// is server-side.
    HealSelf {
        amount: i32,
    },

    /// Dev intent — credit exact per-tier coin stacks (no reduction; a
    /// 1,000-copper grant arrives as 1,000 raw copper, which is the point
    /// for encumbrance testing). Backs the Test Panel money buttons; gated
    /// on `is_dev` like HealSelf / DamageSelf. Server replies with the
    /// authoritative CoinsUpdate.
    GiveCoins {
        platinum: i64,
        gold: i64,
        silver: i64,
        copper: i64,
    },

    /// PD_W0014 — per-player `/autosplit` toggle. Sets the sender's
    /// `autosplit` flag: on (default) splits coin they loot among the
    /// nearby group in Round Robin; off keeps it all for the looter.
    /// See client docs/design/group_loot_and_coin.md.
    SetAutosplit {
        on: bool,
    },

    /// PD_W0014 — leader sets the group's loot distribution mode
    /// (`groups::LootMode`: 0 = Round Robin, 1 = Free-for-all). Ignored
    /// from non-leaders. The server re-fans `GroupRoster` with the new
    /// mode on success.
    SetGroupLootMode {
        mode: u8,
    },

    /// PD_W0014 — current leader hands leadership to `new_leader` (a
    /// member char_id). Ignored from non-leaders or for a non-member
    /// target; the server re-fans `GroupRoster` on success. (Fixes the
    /// pre-existing launcher-mode gap where leadership pass was local-only.)
    PassLeadership {
        new_leader: u64,
    },

    /// PD_W0015 — Banker, slice 1. Move `coins` (per-tier amounts) from the
    /// sender's carried wallet into their bank. Server validates non-negative
    /// + affordable, then fans `CoinsUpdate` (wallet) + `BankSnapshot` (bank).
    BankDepositCoins {
        coins: Coins,
    },
    /// PD_W0015 — Banker, slice 1. Move `coins` from the bank back to the
    /// carried wallet. Server validates the bank holds them.
    BankWithdrawCoins {
        coins: Coins,
    },
    /// PD_W0015 — Banker, slice 1. Convert `qty` coins of `from_tier` into
    /// `to_tier` on the carried wallet (tiers 0 = copper … 3 = platinum).
    /// Up-conversion must be a whole multiple of the target tier; 0% fee MVP.
    BankExchange {
        from_tier: u8,
        to_tier: u8,
        qty: u32,
    },

    /// PD_W0016 — Banker, slice 2. Quick-transfer (deposit) the WHOLE stack at
    /// `(src_location, src_slot)` in the player's inventory into a bank item
    /// vault: `shared = false` is the 10-slot per-character vault, `true` is the
    /// 2-slot account-shared vault. The server merges into same-item stacks then
    /// claims free vault slots; any remainder stays in inventory.
    BankStoreItem {
        src_location: String,
        src_slot: u32,
        shared: bool,
    },
    /// PD_W0016 — Banker, slice 2. Quick-transfer (withdraw) the whole stack at
    /// `vault_slot` back into the player's inventory (`shared` selects which
    /// vault). The server merges into inventory; a remainder with no room stays
    /// in the vault.
    BankWithdrawItem {
        shared: bool,
        vault_slot: u32,
    },

    /// PD_W0017 — Camp, slice B. Begin a voluntary `/camp` logout. The server
    /// gates it on the player being seated (`is_sitting`) and runs a ~30 s
    /// countdown (`CAMP_SECS`), cancelled if the player stands/moves or takes
    /// damage; on completion the server logs the player out cleanly. No payload.
    Camp,
    /// PD_W0017 — Camp, slice B. Abort an in-progress `/camp` countdown. No-op
    /// if the player is not currently camping. No payload.
    CancelCamp,

    /// PD_W0018 — server-authoritative XP, Slice 0. Quests are still tracked
    /// client-side, so the client reports a completed quest's xp reward here
    /// and the server applies it through its authoritative leveling path
    /// (`world::progression::award_xp`). Kept to a primitive so the GDScript
    /// client can encode it. As trusted as today's client-only quest xp; quest
    /// turn-ins move server-side in a later track.
    GrantQuestXp {
        amount: i32,
    },

    /// PD_W0022 — corpse / resurrection Slice 3. The dead player's response to a
    /// `ResurrectOffer` on their corpse. `accept = false` declines. The server
    /// re-validates (corpse still exists, owner in range / in-world, not already
    /// resurrected) before summoning + refunding xp.
    ResurrectAccept {
        corpse_id: EntityId,
        accept: bool,
    },

    /// PD_W0023 — dev-only (requires the server's PD_DEV_CMDS gate, like
    /// HealSelf): ask the server to spawn a REAL world mob near the requester.
    /// Backs the Test Panel spawn buttons in launcher mode so dev-spawned
    /// monsters are true world creatures (server combat, XP, loot, corpse,
    /// quest kill credit) instead of client-local puppets the server can't see.
    DevSpawnMob {
        name: String,
        level: u32,
        hp: f32,
        dmg: i32,
        speed: f32,
        aggro: f32,
    },

    /// PD_W0023 — quest turn-in by id. Replaces the raw `GrantQuestXp{amount}`
    /// for quests (which let a client name its own reward — one forged packet
    /// was an instant level cap). The server looks the id up in its own quest
    /// table, computes the XP itself (tier% x band(level_req)), and records the
    /// completion so a quest pays once per character, ever (also kills the
    /// relog + re-turn-in farm). `GrantQuestXp` survives dev-gated for the
    /// Test Panel leveling buttons.
    CompleteQuest {
        quest_id: String,
    },
}

// ─── Server → Client ─────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum KickCode {
    SessionExpired,
    Restart,
    BannedNow,
    DuplicateLogin,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum QuestStatus {
    Active,
    Completed,
    Failed,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ServerWorldMsg {
    // Connection
    /// Sent once after the app-layer `Connect` handshake completes. Carries
    /// the local player's identity (name/race/class/level) so the game can
    /// initialize PlayerStats before changing to the world scene — without
    /// this, launcher-mode characters spawn classless and can't cast or use
    /// skills. The server is authoritative on these fields (loaded from DB
    /// in `CharacterSpawn`); the launcher doesn't need to relay them.
    ConnectOk {
        player_id: EntityId,
        name: String,
        race: String,
        class: String,
        level: u32,
        /// PD_W0026 — whether this account holds GM. The client previously
        /// cached this from the AUTH LoginOk, which meant any path into the
        /// world that skipped the login screen (the retired standalone launcher,
        /// or re-entering the login scene with a live session) left the client
        /// believing it was not a GM while the server knew otherwise. Sending it
        /// on the world handshake makes it authoritative and path-independent.
        /// Display-gating only: every dev command is gated server-side on the
        /// is_gm bit in the signed token, never on this.
        is_gm: bool,
    },
    Kick {
        reason: String,
        code: KickCode,
        reconnect_after_secs: Option<u32>,
    },
    Heartbeat,

    // Entity replication
    EntityDespawn {
        id: EntityId,
    },
    /// Identity payload for a player (or in future, any entity) appearing in
    /// the recipient's AOI. Sent on the reliable system channel; ongoing
    /// `Position` broadcasts stay lean.
    EntitySpawn {
        id: EntityId,
        name: String,
        race: String,
        class: String,
        level: u32,
        pos: Vec3,
        yaw: f32,
    },
    Position {
        id: EntityId,
        pos: Vec3,
        vel: Vec3,
        yaw: f32,
        sequence: Sequence,
    },

    // Stats / resources
    HealthUpdate {
        id: EntityId,
        hp: f32,
        max_hp: f32,
    },
    ManaUpdate {
        id: EntityId,
        mp: f32,
        max_mp: f32,
    },
    StaminaUpdate {
        id: EntityId,
        stamina: f32,
        max: f32,
    },
    CoinsUpdate {
        coins: Coins,
    },
    XpGained {
        amount: i32,
        current: i32,
        to_next: i32,
    },
    /// PD_W0018 — server-authoritative leveling. Sent privately to a player
    /// whose level changed (up on xp gain, or DOWN on a death penalty). Carries
    /// the authoritative new level plus xp into it + that level's band; the
    /// client sets the level, mirrors the bar, and applies the matching
    /// intrinsic stat deltas locally. New max pools arrive via the resource fan.
    LevelUp {
        new_level: u32,
        xp: i32,
        xp_to_next: i32,
    },
    AlignmentChanged {
        score: i32,
        tier: String,
    },

    // Combat events
    Hit {
        attacker: EntityId,
        target: EntityId,
        amount: i32,
        crit: bool,
        dmg_type: DamageType,
    },
    Miss {
        attacker: EntityId,
        target: EntityId,
    },
    Evade {
        attacker: EntityId,
        target: EntityId,
    },
    EntityDied {
        id: EntityId,
    },

    // Buffs
    BuffApplied {
        target: EntityId,
        buff_id: String,
        duration: f32,
    },
    BuffRemoved {
        target: EntityId,
        buff_id: String,
    },
    HotTick {
        target: EntityId,
        amount: i32,
        source: String,
    },
    DotTick {
        target: EntityId,
        amount: i32,
        source: String,
    },
    /// Track 4 sub-task 3: full buff list for `target`. Replaces any
    /// previous BuffApplied/Removed-style tracking the client had for
    /// this entity. Empty Vec means "no buffs". Same wire shape as
    /// ClientWorldMsg::BuffSnapshotBroadcast but with an explicit target
    /// so the receiver knows which peer to render.
    BuffSnapshot {
        target: EntityId,
        buffs: Vec<(String, f32)>,
    },

    // Casting. `spell_name` instead of a numeric id — we don't have a stable
    // spell-id table on either side yet, and the reliable system channel
    // makes the string cost negligible. A future track can add a SpellDefinitions
    // numeric id table and tighten the wire if it ever matters.
    CastStart {
        caster: EntityId,
        spell_name: String,
        duration: f32,
    },
    CastComplete {
        caster: EntityId,
        spell_name: String,
    },
    CastFail {
        caster: EntityId,
        reason: String,
    },
    Cooldown {
        spell_or_skill_id: u32,
        remaining: f32,
        total: f32,
    },

    // World
    TimeOfDay {
        hour: f32,
    },

    // Chat
    ChatMessage {
        speaker: String,
        channel: ChatChannel,
        text: String,
        lang: String,
    },

    /// Reply to `ClientWorldMsg::InspectPlayer`. `slots` is a list of
    /// `(equip_slot_discriminant, item_path)` pairs for each occupied
    /// paperdoll slot on the inspected player. The unequipped slots
    /// are omitted; client renders those as "—".
    InspectResult {
        target_char_id: i64,
        target_name: String,
        slots: Vec<(u8, String)>,
    },

    // System
    Error {
        code: String,
        msg: String,
    },
    BroadcastMessage {
        msg: String,
    },

    // Track 5: server-authoritative enemies.
    //
    // Enemy entities live entirely server-side; the client renders broadcasts
    // and never invents enemy state. Entity-id namespace is partitioned so
    // enemy ids never collide with player char_ids: enemies start at
    // `protocol::world::ENEMY_ID_BASE` (1_000_000_000).
    //
    // `EnemySpawn` carries the mob's identity + initial state in one shot,
    // analogous to `EntitySpawn` for players. Ongoing position/HP updates
    // reuse the generic `Position` / `HealthUpdate` variants. Death goes
    // out as `EntityDied` then a delayed `EntityDespawn` after the corpse
    // linger.
    EnemySpawn {
        id: EntityId,
        mob_name: String,
        level: u32,
        max_hp: f32,
        hp: f32,
        pos: Vec3,
        yaw: f32,
    },
    /// Server-authoritative aggro replication. Broadcast on target switch
    /// only (not per tick) — enough for "the mob turned on the healer!"
    /// awareness without exposing the full aggro table on the wire.
    /// `target = None` means de-aggro (returning to spawn / leash).
    EntityTarget {
        id: EntityId,
        target: Option<EntityId>,
    },

    // Track 5 sub-task 4 — loot bag fan-out. Server owns the loot table
    // and rolls on enemy death; broadcasts the bag's identity + items +
    // pos. Re-broadcast on every state change (item removed) so the
    // wire shape stays snapshot-style (matches BuffSnapshot). Bag ids
    // live in their own partition above the enemy range so the client
    // can route by id alone. EntityDespawn handles the despawn side.
    LootBagSpawn {
        bag_id: EntityId,
        pos: Vec3,
        items: Vec<(String, u32)>,
        /// PD_W0014 — coin sitting on the corpse, shown in the loot
        /// window. Credited (split or whole) to the looter on the first
        /// loot action and then zeroed; a re-snapshot carries the update.
        coins: Coins,
        /// PD_W0021 — the dead creature's display name. The client renders a
        /// body with a "<name>'s corpse" nameplate instead of a golden orb.
        /// EMPTY for a player-dropped public bag (no creature died), which
        /// keeps the old sack visual. Appended last to stay wire-compatible.
        creature_name: String,
    },
    /// Private message — sent only to the client whose `LootItem` /
    /// `LootAll` intent landed. Carries one stack the looter just claimed
    /// so the client adds it to local inventory. Bag-wide state changes
    /// go out as `LootBagSpawn` (full snapshot) to every in_world peer
    /// simultaneously.
    LootGranted {
        item_path: String,
        count: u32,
    },
    /// PD_W0014 — private notice that a loot attempt was refused: the
    /// looter isn't in the owning group, or it isn't their turn in Round
    /// Robin. The client shows `reason` in the combat log.
    LootRejected {
        reason: String,
    },

    /// Track 6 sub-task 5 — server forwards a pending group invite
    /// to the invitee. Client shows an accept/reject dialog. The
    /// invitee responds with `ClientWorldMsg::GroupAcceptInvite
    /// { from: from_id }` to join, or just lets the invite expire.
    GroupInvited {
        from_id: EntityId,
        from_name: String,
    },
    /// Track 6 sub-task 5 — full group roster update. Fanned to every
    /// online group member whenever membership changes (invite
    /// accepted, member left, member kicked, leader changed). Empty
    /// `members` means the group dissolved (last remaining member
    /// receives this as their cleanup signal).
    GroupRoster {
        group_id: u64,
        leader_id: EntityId,
        members: Vec<(EntityId, String)>,
        /// PD_W0014 — the group's loot distribution mode
        /// (`groups::LootMode` as u8: 0 = Round Robin, 1 = Free-for-all).
        /// Carried on every roster so members always see the current mode.
        loot_mode: u8,
    },
    /// Track 6 — damage-shield reflect notification. Fanned when a
    /// player's DamageShield buff (Thorns / Spellshield) reflects
    /// damage back at an attacker. The defender's client uses this
    /// to log "X took N damage from your <shield_name>"; the
    /// attacker's client renders the floating number as well.
    DamageShieldTrigger {
        defender: EntityId,
        attacker: EntityId,
        amount: i32,
        shield_name: String,
    },

    /// Track 11 — server announces a player-owned pet at `pos`.
    /// `id` is in the reserved pet partition (`>= PET_ID_BASE`) so
    /// the client can route by id alone. `owner` is the summoner's
    /// char_id (always `< ENEMY_ID_BASE`). Ongoing position / HP /
    /// death broadcasts reuse the generic `Position` /
    /// `HealthUpdate` / `EntityDied` / `EntityDespawn` variants.
    PetSpawn {
        id: EntityId,
        owner: EntityId,
        pet_name: String,
        level: u32,
        max_hp: f32,
        hp: f32,
        pos: Vec3,
        yaw: f32,
    },

    /// Track 13.2 — full inventory snapshot. Fanned privately to the
    /// owning client on EnterWorld so they see their persisted items
    /// from `character_items` rendered into their UI immediately.
    /// `entries` is parallel arrays of (location, slot, item_path,
    /// count); the client projects into its `Inventory.base_slots` /
    /// `bag_contents` / `Equipment.equipped` shapes as appropriate.
    InventorySnapshot {
        entries: Vec<(String, u32, String, u32)>,
    },

    /// Track 13.2 — incremental inventory mutation. Fanned privately
    /// to the owning client when the server adds, removes, or moves
    /// an entry (loot grant, MoveItem, drop, equip). `item_path =
    /// None` means the slot is now empty; otherwise the slot now
    /// holds `(item_path, count)`. Slot-by-slot framing keeps the
    /// wire shape stable across single-slot operations and bulk ones
    /// (a swap fans two Deltas, a drop fans one).
    InventoryDelta {
        location: String,
        slot: u32,
        item_path: Option<String>,
        count: u32,
    },

    /// Track 18.1 — single-skill advance event. Fanned privately when
    /// a `skills::try_advance` roll lands. `new_score` is the post-
    /// increment value; the client computes the cap locally from
    /// class + level.
    SkillProgressUpdate {
        kind: SkillKind,
        key: String,
        new_score: u32,
    },

    /// Track 18.1 — full skill score snapshot. Fanned privately once
    /// on enter-world to seed the client's three skill autoload caches
    /// (the GDScript `_skills` dicts). Each list is parallel arrays of
    /// (key, score) for the corresponding `SkillKind`.
    SkillProgressSnapshot {
        weapon: Vec<(String, u32)>,
        armor: Vec<(String, u32)>,
        casting: Vec<(String, u32)>,
    },

    /// PD_W0014 — private one-line group notice shown in the recipient's
    /// combat log. Currently fanned when a group member toggles
    /// `/autosplit` (transparency: it affects whether the group shares
    /// coin from that member's loots). Fanned to the toggler's group-mates
    /// only; the toggler gets their own echo locally.
    ///
    /// Appended at the end of the enum: bincode encodes the variant by
    /// positional index, so new server→client variants go last to keep
    /// every existing variant's discriminant stable (same rule the
    /// PD_W0011 `Shout` / `ChatChannel` additions followed).
    GroupNotice {
        text: String,
    },

    /// PD_W0015 — Banker, slice 1. The recipient's current bank balance
    /// (four-tier wallet). Fanned privately when the player opens the bank and
    /// after each deposit/withdraw. The carried wallet uses `CoinsUpdate`.
    BankSnapshot {
        coins: Coins,
    },
    /// PD_W0015 — Banker, slice 1. A bank action was refused (e.g. you don't
    /// hold that coin, or a non-whole conversion). The client logs `reason`.
    BankRejected {
        reason: String,
    },

    /// PD_W0016 — Banker, slice 2. Full contents of one item vault (`shared`
    /// selects per-character vs account-shared). `entries` is `(slot, item_path,
    /// count)` for filled slots only. The vaults are tiny (10 / 2 slots), so a
    /// full snapshot is re-fanned after each store/withdraw (and on enter-world)
    /// rather than per-slot deltas — same approach as the coin `BankSnapshot`.
    BankItemSnapshot {
        shared: bool,
        entries: Vec<(u32, String, u32)>,
    },

    /// PD_W0017 — Camp, slice B. Server confirm for the `/camp` countdown so the
    /// client display stays authoritative. Sent when a camp starts
    /// (`active = true`, `remaining_secs = CAMP_SECS`) and when it ends
    /// (`active = false`, on cancel by stand/move/damage). On *completion* the
    /// server logs the player out (a clean disconnect), so the client never sees
    /// an `active = false` for the success case — the disconnect is the signal.
    CampUpdate {
        remaining_secs: u32,
        active: bool,
    },

    /// PD_W0019 — corpse / resurrection Slice 1. A player corpse spawned in AOI
    /// (on death or boot-loaded). `owner_id` is the dead player's char_id (so the
    /// client can tell if it's its own corpse); `owner_name` labels the
    /// "<name>'s corpse" nameplate. `corpse_id` is minted from the loot-bag id
    /// partition; despawn rides the shared `EntityDespawn`. No items on the wire
    /// yet (render-only) — Slice 2 adds looting.
    CorpseSpawn {
        corpse_id: EntityId,
        owner_id: EntityId,
        owner_name: String,
        pos: Vec3,
    },

    /// PD_W0020 — corpse / resurrection Slice 2. A corpse's contents, sent
    /// PRIVATELY to the OWNER only (peers see the body via `CorpseSpawn` but never
    /// the gear list). Snapshot-style: re-sent in full after each partial loot so
    /// the owner's open loot window refreshes. The client drives a loot window off
    /// this; taking items reuses the existing `LootItem` / `LootAll` intents keyed
    /// by `corpse_id` (same id partition as loot bags).
    CorpseContents {
        corpse_id: EntityId,
        items: Vec<(String, u32)>,
        coins: Coins,
    },

    /// PD_W0022 — corpse / resurrection Slice 3. A Cleric/Paladin cast a
    /// resurrection on this corpse; offer it to the corpse's owner (sent PRIVATELY
    /// to them). `xp_percent` is for the prompt text only — the server computes the
    /// real refund from the corpse's stored lost xp on accept.
    ResurrectOffer {
        corpse_id: EntityId,
        caster_name: String,
        xp_percent: u32,
    },

    /// PD_W0022 — server-authoritative forced reposition (used by a resurrection to
    /// summon the living player to their corpse). The client snaps its local player
    /// to `pos`; peers see the move via the normal position fan.
    Teleport {
        pos: Vec3,
    },
    /// PD_W0023 — quest kill credit, sent PRIVATELY to whoever earned XP for a
    /// kill (the solo killer, a pet's owner, or each online group member on the
    /// XP split), never broadcast. The client feeds `mob_name` to
    /// `QuestManager.notify_kill` so "kill N X" objectives advance online. Kept
    /// separate from the public `EntityDied` broadcast precisely so a bystander
    /// who merely witnessed the death does NOT get quest credit.
    KillCredit {
        mob_name: String,
    },

    /// PD_W0024 — full quest-journal seed, sent PRIVATELY once on EnterWorld.
    /// `active` is `(quest_id, per-objective progress counts)` for every quest
    /// the character has accepted but not completed; `completed` is every
    /// quest id that has ever paid out (so the client can grey out re-offers
    /// instead of letting the player redo a quest for zero XP). This is what
    /// makes the journal survive relog and server restart.
    QuestSnapshot {
        active: Vec<(String, Vec<i32>)>,
        completed: Vec<String>,
    },

    /// PD_W0024 — one objective counter moved, sent PRIVATELY to the quest
    /// holder (each group member tracks their own progress). `count` is the
    /// new absolute value, not a delta, so a dropped packet self-heals on the
    /// next increment. Replaces `KillCredit` as the online journal driver —
    /// the server counts kills now, the client just renders.
    QuestProgress {
        quest_id: String,
        objective_index: u32,
        count: i32,
    },

    /// PD_W0024 — an Accept/Abandon/CompleteQuest was refused (unknown id,
    /// level too low, already completed, objectives incomplete, ...). The
    /// client prints `reason` to the combat log — a redo attempt now gets a
    /// visible line instead of silently paying nothing. `rollback` is true
    /// ONLY for accept-phase rejections: it tells the client to undo the
    /// optimistic journal add (the server never started tracking this quest).
    /// A turn-in rejection sends `rollback = false` so the client keeps the
    /// still-tracked active entry (the server did NOT string-match reasons —
    /// it knows which action it refused).
    QuestRejected {
        quest_id: String,
        reason: String,
        rollback: bool,
    },

    /// PD_W0024 — a turn-in succeeded and paid out (the XP itself rides the
    /// usual `XpGained` / `LevelUp`). Private to the turn-in-er. The client
    /// flips its journal entry to COMPLETED off this, never optimistically —
    /// the server may have rejected the turn-in instead.
    QuestCompleted {
        quest_id: String,
    },

    /// PD_W0025 — a weapon proc fired on a landed melee swing. The server rolls
    /// proc_chance + applies proc_damage itself (folded into the swing's own
    /// damage/death), then sends this so the client renders the named proc hit
    /// ("<proc_name> for <amount>", elemental flash by `dmg_type`). Private to
    /// the attacker (their outgoing damage), like the `You hit X` lines.
    ProcTriggered {
        attacker: EntityId,
        target: EntityId,
        proc_name: String,
        amount: i32,
        crit: bool,
        dmg_type: DamageType,
    },
}

/// First entity id reserved for server-spawned enemies. Player char_ids are
/// minted by the auth service well below this range (current schema uses
/// i64 row ids starting at 1), so this gives ample headroom for
/// disambiguating players vs enemies by id alone on the client.
pub const ENEMY_ID_BASE: EntityId = 1_000_000_000;
/// First entity id reserved for server-spawned loot bags. Above the enemy
/// partition so the client can route a Position / EntityDespawn by id
/// alone: < ENEMY_ID_BASE → player, < LOOT_BAG_ID_BASE → enemy,
/// < PET_ID_BASE → bag, else pet.
pub const LOOT_BAG_ID_BASE: EntityId = 2_000_000_000;
/// First entity id reserved for server-spawned player-owned pets
/// (Track 11). Sits above the bag partition; the four ranges
/// (player < 1B, enemy < 2B, bag < 3B, pet ≥ 3B) cover the id
/// space the client routes on.
pub const PET_ID_BASE: EntityId = 3_000_000_000;

/// Track 12 Piece A — pet command codes. Wire format: a single u8
/// on `ClientWorldMsg::PetCommand.command`. Values reserved for
/// future commands (Follow/Guard/Sit) are present so the server can
/// add behaviour without a protocol bump; for now they're no-ops.
pub mod pet_command {
    pub const FOLLOW: u8 = 0;
    pub const GUARD: u8 = 1;
    pub const ATTACK: u8 = 2;
    pub const BACK: u8 = 3;
    pub const SIT: u8 = 4;
}

#[cfg(test)]
mod coins_tests {
    use super::Coins;

    #[test]
    fn total_and_from_copper_roundtrip() {
        let c = Coins::from_copper(1_234_567);
        assert_eq!(c, Coins { platinum: 1, gold: 23, silver: 45, copper: 67 });
        assert_eq!(c.total_copper(), 1_234_567);
        assert_eq!(Coins::from_copper(-5), Coins::ZERO);
    }

    #[test]
    fn spend_exact_from_copper() {
        let mut c = Coins { copper: 100, ..Coins::ZERO };
        assert!(c.spend(36));
        assert_eq!(c, Coins { copper: 64, ..Coins::ZERO });
    }

    #[test]
    fn spend_leaves_a_copper_hoard_intact() {
        // The whole point of independent stacks: buying a 1c candle out of a
        // 5000-copper hoard must NOT silently consolidate it into silver.
        let mut c = Coins { copper: 5000, ..Coins::ZERO };
        assert!(c.spend(1));
        assert_eq!(c, Coins { copper: 4999, ..Coins::ZERO });
    }

    #[test]
    fn spend_breaks_a_higher_coin_into_change() {
        // Pay 1 copper out of a lone platinum → 0p 99g 99s 99c (= 999_999c).
        let mut c = Coins { platinum: 1, ..Coins::ZERO };
        assert!(c.spend(1));
        assert_eq!(c, Coins { platinum: 0, gold: 99, silver: 99, copper: 99 });
        assert_eq!(c.total_copper(), 999_999);
    }

    #[test]
    fn spend_rejects_when_unaffordable_and_leaves_wallet_untouched() {
        let mut c = Coins { silver: 1, ..Coins::ZERO };
        assert!(!c.spend(150));
        assert_eq!(c, Coins { silver: 1, ..Coins::ZERO });
    }

    #[test]
    fn add_payout_reduces_payout_but_not_existing_stacks() {
        // Existing 99 raw copper stays raw; the 5000c payout arrives as 50s.
        let mut c = Coins { copper: 99, ..Coins::ZERO };
        c.add_payout(5000);
        assert_eq!(c, Coins { platinum: 0, gold: 0, silver: 50, copper: 99 });
        assert_eq!(c.total_copper(), 5099);
    }

    // ── Banker slice 1: tier exchange + deposit/withdraw math ──

    #[test]
    fn exchange_up_converts_whole_multiples_and_preserves_value() {
        // 200 copper → 2 silver; the rest of the wallet is untouched.
        let mut c = Coins { copper: 250, gold: 1, ..Coins::ZERO };
        let before = c.total_copper();
        assert!(c.exchange(0, 1, 200).is_ok());
        assert_eq!(c, Coins { copper: 50, silver: 2, gold: 1, ..Coins::ZERO });
        assert_eq!(c.total_copper(), before, "0% fee preserves value");
    }

    #[test]
    fn exchange_up_partial_converts_and_keeps_remainder() {
        // 150 copper -> 1 silver, with the unconvertible 50 copper left in place.
        let mut c = Coins { copper: 150, ..Coins::ZERO };
        let before = c.total_copper();
        assert!(c.exchange(0, 1, 150).is_ok());
        assert_eq!(c, Coins { copper: 50, silver: 1, ..Coins::ZERO });
        assert_eq!(c.total_copper(), before, "0% fee preserves value");
    }

    #[test]
    fn exchange_up_rejects_below_one_target_coin() {
        // 50 copper can't make even one silver — nothing converts.
        let mut c = Coins { copper: 50, ..Coins::ZERO };
        assert!(c.exchange(0, 1, 50).is_err());
        assert_eq!(c, Coins { copper: 50, ..Coins::ZERO });
    }

    #[test]
    fn exchange_down_always_divides_evenly() {
        let mut c = Coins { silver: 1, ..Coins::ZERO };
        assert!(c.exchange(1, 0, 1).is_ok());
        assert_eq!(c, Coins { copper: 100, ..Coins::ZERO });
    }

    #[test]
    fn exchange_rejects_insufficient_same_tier_and_zero() {
        let mut c = Coins { copper: 100, ..Coins::ZERO };
        assert!(c.exchange(0, 1, 200).is_err(), "not enough copper");
        assert!(c.exchange(1, 1, 1).is_err(), "same tier");
        assert!(c.exchange(0, 1, 0).is_err(), "zero qty");
        assert_eq!(c, Coins { copper: 100, ..Coins::ZERO }, "no partial mutation");
    }

    #[test]
    fn deposit_helpers_are_per_tier() {
        let wallet = Coins { silver: 5, copper: 30, ..Coins::ZERO };
        let amt = Coins { silver: 2, copper: 30, ..Coins::ZERO };
        assert!(wallet.has_at_least(amt));
        assert!(!wallet.has_at_least(Coins { silver: 6, ..Coins::ZERO }));
        assert_eq!(wallet.sub_each(amt), Coins { silver: 3, ..Coins::ZERO });
        assert_eq!(Coins::ZERO.add_each(amt), amt);
    }

    #[test]
    fn has_negative_flags_malformed_amounts() {
        assert!(Coins { copper: -1, ..Coins::ZERO }.has_negative());
        assert!(!Coins { copper: 1, ..Coins::ZERO }.has_negative());
    }
}

#[cfg(test)]
mod coin_breaking_tests {
    use super::Coins;

    fn c(p: i64, g: i64, s: i64, cu: i64) -> Coins {
        Coins { platinum: p, gold: g, silver: s, copper: cu }
    }

    /// The case the tester raised: 10 gold, pay 5 silver, expect 9g 95s.
    #[test]
    fn paying_silver_breaks_one_gold() {
        let mut purse = c(0, 10, 0, 0);
        assert!(purse.pay_value_of(c(0, 0, 5, 0)));
        assert_eq!(purse, c(0, 9, 95, 0));
    }

    /// Breaking cascades a tier at a time, so change comes back as coins of the
    /// next tier rather than a heap of copper.
    #[test]
    fn paying_copper_from_gold_cascades_one_tier_at_a_time() {
        let mut purse = c(0, 10, 0, 0);
        assert!(purse.pay_value_of(c(0, 0, 0, 5)));
        assert_eq!(purse, c(0, 9, 99, 95));
    }

    /// Nothing is invented: the total is identical before and after.
    #[test]
    fn breaking_conserves_total_value() {
        let mut purse = c(1, 5, 5, 75);
        let before = purse.total_copper();
        let want = c(0, 0, 6, 0);
        assert!(purse.pay_value_of(want));
        assert_eq!(purse.total_copper() + want.total_copper(), before);
    }

    /// A purse of silver CAN pay a gold. Money is money: what matters is
    /// whether the purse is worth enough, not which coins it happens to hold.
    /// Refusing this was the original complaint.
    #[test]
    fn silver_can_pay_a_gold() {
        let mut purse = c(0, 0, 100, 0);
        assert!(purse.pay_value_of(c(0, 1, 0, 0)));
        assert_eq!(purse, Coins::ZERO, "100 silver is exactly one gold");
    }

    /// Paying upward takes only what is needed and leaves the rest alone.
    #[test]
    fn paying_a_gold_from_mixed_silver_leaves_the_remainder() {
        let mut purse = c(0, 0, 150, 0);
        assert!(purse.pay_value_of(c(0, 1, 0, 0)));
        assert_eq!(purse, c(0, 0, 50, 0));
    }

    /// A purse that simply does not hold enough is still refused, untouched.
    #[test]
    fn insufficient_total_is_refused_without_mutating() {
        let mut purse = c(0, 0, 3, 0);
        assert!(!purse.pay_value_of(c(0, 0, 5, 0)));
        assert_eq!(purse, c(0, 0, 3, 0));
    }

    /// An exact per-tier payment behaves exactly as sub_each did, so the common
    /// case is unchanged.
    #[test]
    fn exact_payment_needs_no_breaking() {
        let mut purse = c(1, 2, 3, 4);
        assert!(purse.pay_value_of(c(1, 2, 3, 4)));
        assert_eq!(purse, Coins::ZERO);
    }

    /// Platinum breaks down through gold when silver is short.
    #[test]
    fn breaking_reaches_across_multiple_tiers() {
        let mut purse = c(1, 0, 0, 0);
        assert!(purse.pay_value_of(c(0, 0, 1, 0)));
        assert_eq!(purse, c(0, 99, 99, 0));
        assert_eq!(purse.total_copper(), 1_000_000 - 100);
    }
}
