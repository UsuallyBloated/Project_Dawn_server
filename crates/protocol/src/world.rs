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
pub const WORLD_PROTOCOL_ID: u64 = 0x5044_5f57_3030_3134; // "PD_W0014"

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

    // Quests
    AcceptQuest {
        quest_id: String,
        giver_id: EntityId,
    },
    AbandonQuest {
        quest_id: String,
    },
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
    LevelUp {
        new_level: u32,
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
}
