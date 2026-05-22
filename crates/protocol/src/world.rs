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
pub const WORLD_PROTOCOL_ID: u64 = 0x5044_5f57_3030_3130; // "PD_W0010"

pub type EntityId = u64;
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
        coins: i64,
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
