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
/// PD_W0003 covers the Track 4 batch: `ResourceUpdate` (plus the cast/buff/
/// combat/death broadcast variants added in the same session). One bump per
/// track; individual sub-task commits append new variants under this id.
pub const WORLD_PROTOCOL_ID: u64 = 0x5044_5f57_3030_3033; // "PD_W0003"

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
    Attack,
    CastSpell {
        spell_id: u32,
        target_id: Option<EntityId>,
    },
    UseSkill {
        skill_id: u32,
        target_id: Option<EntityId>,
    },
    CancelCast,

    // Inventory
    MoveItem {
        from: SlotRef,
        to: SlotRef,
    },
    EquipItem {
        from: SlotRef,
    },
    UnequipItem {
        slot: EquipSlot,
    },
    DropItem {
        slot: SlotRef,
        count: u32,
    },
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

    // GM
    GmCommand {
        line: String,
    },

    // Track 4: owning-client → server broadcast of current resources. Server
    // fans out to peers as three separate ServerWorldMsg variants
    // (HealthUpdate / ManaUpdate / StaminaUpdate) so the existing typed
    // signals on the client can stay unchanged. Throttled client-side to
    // ~4 Hz under quiet conditions; fires immediately on >5% delta of max.
    ResourceUpdate {
        hp: f32,
        max_hp: f32,
        mp: f32,
        max_mp: f32,
        stamina: f32,
        max_stamina: f32,
    },

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
}
