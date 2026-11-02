//! The world loop's state: `WorldState`, everything that lives across ticks, and
//! `Intents`, the queues one tick's message drain fills and its later steps
//! apply (with the intent types they hold). Moved out of `tick::run` as the first
//! step of splitting it into phases; each phase destructures these back into the
//! same local names the code always used.

use super::*;

/// Everything the world loop keeps from one tick to the next.
pub(super) struct WorldState {
    pub(super) cfg: Arc<Config>,
    pub(super) pool: SqlitePool,
    pub(super) server: RenetServer,
    pub(super) connections: HashMap<ClientId, PerConnection>,
    pub(super) aoi: AoiGrid,
    pub(super) last_checkpoint: Instant,
    pub(super) pending_saves: Vec<PendingSave>,
    pub(super) last_pending_retry: Instant,
    pub(super) enemy_pos_sent: u64,
    pub(super) last_fan_report: Instant,
    pub(super) tick_health: crate::world::health::TickHealth,
    pub(super) last_health_report: Instant,
    pub(super) last_ban_sweep: Instant,
    pub(super) last_ledger_sweep: Option<Instant>,
    pub(super) last_time_broadcast: Instant,
    pub(super) ban_tx: tokio::sync::mpsc::UnboundedSender<Vec<i64>>,
    pub(super) ban_rx: tokio::sync::mpsc::UnboundedReceiver<Vec<i64>>,
    pub(super) ban_sweep_in_flight: bool,
    pub(super) spawner: Spawner,
    pub(super) enemies: HashMap<EntityId, Entity>,
    pub(super) loot_bags: HashMap<EntityId, LootBag>,
    pub(super) group_manager: GroupManager,
    pub(super) trade_manager: crate::world::trade::TradeManager,
    pub(super) corpses: HashMap<EntityId, crate::world::corpses::Corpse>,
}

// Track 6 sub-task 5 — group intents buffered for the
// post-dispatch sweep. The sweep needs the full connections
// map (to resolve names to ids + fan rosters to multiple
// members), so we can't process inline in handle_message.
pub(super) struct GroupInviteI { pub(super) inviter: u64, pub(super) target_name: String }

pub(super) struct GroupAcceptI { pub(super) invitee: u64, pub(super) from: u64 }

pub(super) struct GroupLeaveI { pub(super) member: u64 }

pub(super) struct GroupKickI { pub(super) leader: u64, pub(super) target_name: String }

pub(super) struct GroupLootModeI { pub(super) leader: u64, pub(super) mode: u8 }

pub(super) struct GroupPassLeadershipI { pub(super) leader: u64, pub(super) new_leader: u64 }

pub(super) struct AutosplitNoticeI { pub(super) char_id: u64, pub(super) on: bool }

// PD_W0024 — quest accept / abandon lifecycle intents in ONE
// arrival-order queue (not bucketed per-type), so a same-tick
// abandon-then-accept (or accept-then-abandon) on the same quest
// resolves to the client's LAST-stated intent instead of a per-type
// drain order. The drain only mutates in-memory `active_quests` +
// marks `quests_dirty`; the end-of-tick flush (step 6-ter) does the
// single reconciling DB write, so a forged storm can't amplify into an
// awaited write per message.
pub(super) enum QuestLifecycle {
    Accept,
    Abandon,
}

// Track 12 Piece A — pet commands. Buffered to apply after
// message dispatch so we can mutate `enemies` (where pets
// live) without overlapping the handler's mutable
// `connections` borrow.
pub(super) struct PetCommandI { pub(super) owner: u64, pub(super) command: u8, pub(super) target_id: Option<EntityId> }

// Track 13.2 — move-item intents. Buffered like pet commands;
// dispatch runs after the message-drain so we don't overlap
// the handler's mut borrow on `conn`.
pub(super) struct MoveItemI {
    pub(super) owner: u64,
    pub(super) src_location: String,
    pub(super) src_slot: u32,
    pub(super) dst_location: String,
    pub(super) dst_slot: u32,
}

// PD_W0028 — trade intents, buffered like everything else.
pub(super) enum TradeI {
    Request { requester: u64, target: u64 },
    OfferItem { owner: u64, window_slot: u8, from_location: String, from_slot: u32 },
    RetrieveItem { owner: u64, window_slot: u8 },
    OfferCoins { owner: u64, coins: protocol::world::Coins },
    Accept { owner: u64 },
    Cancel { owner: u64 },
}

// Track 13.2.b — split + drop intents.
pub(super) struct SplitStackI {
    pub(super) owner: u64,
    pub(super) src_location: String,
    pub(super) src_slot: u32,
    pub(super) dst_location: String,
    pub(super) dst_slot: u32,
    pub(super) count: u32,
}

pub(super) struct DropItemI {
    pub(super) owner: u64,
    pub(super) location: String,
    pub(super) slot: u32,
    pub(super) count: u32,
}

// Track 15.1 — destroy intents (no loot bag).
pub(super) struct DestroyItemI {
    pub(super) owner: u64,
    pub(super) location: String,
    pub(super) slot: u32,
    pub(super) count: u32,
}

// Track 15.2 — use-consumable intents.
pub(super) struct UseConsumableI {
    pub(super) owner: u64,
    pub(super) location: String,
    pub(super) slot: u32,
}

// Track 15.2 follow-up — GM /give intents (server-side spawn).
pub(super) struct GmGiveI {
    pub(super) owner: u64,
    pub(super) item_name: String,
    pub(super) qty: u32,
}

// Track 13.3 — equip / unequip intents.
pub(super) struct EquipItemI {
    pub(super) owner: u64,
    pub(super) src_location: String,
    pub(super) src_slot: u32,
    pub(super) equip_slot: u8,
}

pub(super) struct UnequipItemI {
    pub(super) owner: u64,
    pub(super) equip_slot: u8,
    pub(super) dst_location: String,
    pub(super) dst_slot: u32,
}

pub(super) struct BuyItemI {
    pub(super) owner: u64,
    #[allow(dead_code)] // vendor stock validation lands once server NPCs do
    pub(super) vendor_id: EntityId,
    pub(super) item_name: String,
    pub(super) qty: u32,
}

pub(super) struct SellItemI {
    pub(super) owner: u64,
    pub(super) slot: protocol::world::SlotRef,
    pub(super) qty: u32,
}

// PD_W0015 — Banker, slice 1 (coins): deposit / withdraw / exchange.
pub(super) struct BankDepositI { pub(super) owner: u64, pub(super) coins: protocol::world::Coins }

pub(super) struct BankWithdrawI { pub(super) owner: u64, pub(super) coins: protocol::world::Coins }

pub(super) struct BankExchangeI { pub(super) owner: u64, pub(super) from_tier: u8, pub(super) to_tier: u8, pub(super) qty: u32 }

// PD_W0016 — Banker, slice 2 (item vaults): store / withdraw whole stacks.
pub(super) struct BankStoreItemI { pub(super) owner: u64, pub(super) src_location: String, pub(super) src_slot: u32, pub(super) shared: bool }

pub(super) struct BankWithdrawItemI { pub(super) owner: u64, pub(super) shared: bool, pub(super) vault_slot: u32 }

/// One tick's queues, filled by the message drain (step 3) and emptied by the
/// steps that apply them.
#[derive(Default)]
pub(super) struct Intents {
    pub(super) to_disconnect: Vec<ClientId>,
    pub(super) newly_in_world: Vec<ClientId>,
    // Cast lifecycle events from this tick. Stored verbatim and fanned
    // out in order — coalescing CastStart + CastComplete from the same
    // sender would silently drop a fast-cast complete.
    pub(super) cast_fanouts: Vec<(ClientId, CastEvent)>,
    // Jumps begun this tick (the rising edge of a Move's `jumping`),
    // relayed below to the players who can see each jumper.
    pub(super) jump_fanouts: Vec<ClientId>,
    // Track 4 sub-task 3 — like resources, dedup per sender so a burst
    // of buff changes inside one tick produces a single fan-out.
    pub(super) buff_fanouts: Vec<ClientId>,
    // Track 4 sub-task 4 combat events. Verbatim queue (Hit/Miss/Evade
    // are one-shot visuals, ordered).
    pub(super) combat_fanouts: Vec<(ClientId, CombatEvent)>,
    // Track 4 sub-task 5 — dying clients to fan out as EntityDied.
    // Dedup-on-insert in case the dying client somehow sends Death
    // twice in one tick.
    pub(super) death_fanouts: Vec<ClientId>,
    // Track 22.H — player target broadcasts queued for the
    // post-dispatch fan-out (which needs the in_world recipients
    // list, not available inside the per-message dispatch).
    pub(super) player_target_fanouts: Vec<(ClientId, Option<EntityId>)>,
    // Chat fan-out queue. Each entry carries the sender's client id
    // so the drain step can exclude the sender from broadcast
    // recipients and resolve the sender's connection for Tell
    // bounce-backs when the target name doesn't match.
    pub(super) chat_fanouts: Vec<(ClientId, protocol::world::ChatChannel, String, String, Option<String>)>,
    // Player-inspect requests. `(inspector_client_id, target_char_id)`.
    pub(super) inspect_intents: Vec<(ClientId, i64)>,
    // Track 5 sub-task 3 — player → server attack intents queued for
    // the apply phase after dispatch. Verbatim queue (each swing is
    // a distinct event; coalescing would silently drop multi-hit
    // combos).
    pub(super) attack_intents: Vec<AttackIntent>,
    pub(super) use_skill_intents: Vec<UseSkillIntent>,
    pub(super) cast_spell_intents: Vec<CastSpellIntent>,
    // Casters already told this tick that an extra CastSpell was dropped.
    pub(super) cast_overflow_told: Vec<ClientId>,
    pub(super) group_invite_intents: Vec<GroupInviteI>,
    pub(super) group_accept_intents: Vec<GroupAcceptI>,
    pub(super) group_leave_intents: Vec<GroupLeaveI>,
    pub(super) group_kick_intents: Vec<GroupKickI>,
    pub(super) group_loot_mode_intents: Vec<GroupLootModeI>,
    pub(super) group_pass_leadership_intents: Vec<GroupPassLeadershipI>,
    pub(super) autosplit_notice_intents: Vec<AutosplitNoticeI>,
    // Track 5 sub-task 4 — player → server loot pickup intents.
    // Verbatim queue; sub-task 4 is FFA loot so order matters for
    // contested bags (first arrival wins the slot).
    pub(super) loot_intents: Vec<LootIntent>,
    // PD_W0027 — ground pickups to the cursor slot: (looter, bag_id).
    pub(super) loot_to_cursor_intents: Vec<(u64, u64)>,
    // Corpse / resurrection Slice 3 — (responder, corpse_id, accept) responses
    // to a res offer, applied after dispatch where the corpses map is in scope.
    pub(super) resurrect_accept_intents: Vec<(u64, protocol::world::EntityId, bool)>,
    // (char_id, zone, pos) — bind writes, persisted immediately (see db::set_bind_point).
    pub(super) bind_intents: Vec<(i64, Option<String>, (f32, f32, f32))>,
    // PD_W0023 — dev-gated Test Panel spawns, applied alongside the natural
    // spawner pass where the enemies map + AOI grid are in scope.
    pub(super) dev_spawn_intents: Vec<(crate::world::connection::Vec3f, crate::world::zones::MobTemplate)>,
    // The Test Panel's "Despawn All Enemies": the GM and their position,
    // applied with the dev spawns.
    pub(super) dev_despawn_intents: Vec<(ClientId, Vec3f)>,
    // Spell bar writes (a memorize, a forget, the one-time seed), applied
    // after dispatch where the pool is.
    pub(super) spell_gem_saves: Vec<(i64, Vec<(i32, String)>, bool)>,
    // PD_W0023 — quest turn-ins (responder, quest_id), applied after dispatch
    // where the DB pool is in scope (the completion persists before the award).
    pub(super) complete_quest_intents: Vec<(u64, String)>,
    pub(super) quest_lifecycle_intents: Vec<(u64, QuestLifecycle, String)>,
    pub(super) consider_intents: Vec<(ClientId, EntityId)>,
    pub(super) pet_command_intents: Vec<PetCommandI>,
    pub(super) move_item_intents: Vec<MoveItemI>,
    pub(super) trade_intents: Vec<TradeI>,
    pub(super) split_stack_intents: Vec<SplitStackI>,
    pub(super) drop_item_intents: Vec<DropItemI>,
    pub(super) destroy_item_intents: Vec<DestroyItemI>,
    pub(super) use_consumable_intents: Vec<UseConsumableI>,
    pub(super) gm_give_intents: Vec<GmGiveI>,
    pub(super) equip_item_intents: Vec<EquipItemI>,
    pub(super) unequip_item_intents: Vec<UnequipItemI>,
    pub(super) buy_item_intents: Vec<BuyItemI>,
    pub(super) sell_item_intents: Vec<SellItemI>,
    pub(super) bank_deposit_intents: Vec<BankDepositI>,
    pub(super) bank_withdraw_intents: Vec<BankWithdrawI>,
    pub(super) bank_exchange_intents: Vec<BankExchangeI>,
    pub(super) bank_store_item_intents: Vec<BankStoreItemI>,
    pub(super) bank_withdraw_item_intents: Vec<BankWithdrawItemI>,
    // EQ's eating emote for the people near anyone who eats or drinks this tick
    // (by hand in the consumable step, on their own at the regen step), fanned
    // once both steps have run.
    pub(super) eat_emotes: Vec<(ClientId, String)>,
}
