//! Decoded application-message handlers. The tick loop ([`super::tick`])
//! dispatches incoming bytes to these; they mutate the connection state
//! and may queue replies on the renet server for the next packet flush.

use super::{
    connection::{PerConnection, Vec3f},
    entity::Entity,
    loot::LootBag,
    CHANNEL_POSITION, CHANNEL_SYSTEM,
};
use bincode::config::standard as bincode_cfg;
use protocol::world::{ClientWorldMsg, Coins, KickCode, ServerWorldMsg, Vec3};
use renet::{ClientId, RenetServer};
use std::time::Instant;

/// Outcome of dispatching a single decoded `ClientWorldMsg`. The tick loop
/// uses this to decide whether to keep the connection, tear it down, or
/// follow up with multi-client side effects (EntitySpawn fan-out when a
/// client enters the world, resource fan-out on ResourceUpdate, etc.).
pub enum Outcome {
    Continue,
    Disconnect,
    /// `conn.in_world` transitioned false → true this dispatch (i.e. client
    /// sent `EnterWorld` after leaving the lobby). The tick loop owes the
    /// new client EntitySpawns for every existing in_world peer, and every
    /// existing in_world peer an EntitySpawn for the new client. Peers
    /// don't see a body until the owner has left the lobby — fixes the
    /// "A sees B's static capsule while B is at Enter World" artifact.
    JustEnteredWorld,
    /// Track 4 sub-task 2 — owning client started casting. Tick loop fans
    /// out CastStart inline (cast events are infrequent enough that
    /// per-message fan-out beats coalescing).
    CastStartFanOut {
        spell_name: String,
        duration: f32,
    },
    /// Owning client completed a cast. Tick loop fans out CastComplete.
    CastCompleteFanOut {
        spell_name: String,
    },
    /// Owning client's cast was interrupted / cancelled. Tick loop fans out
    /// CastFail.
    CastFailFanOut {
        reason: String,
    },
    /// Track 4 sub-task 3 — owning client broadcast a fresh buff snapshot.
    /// Cache updated on `conn`; tick loop fans out to in_world peers in
    /// a post-dispatch sweep, deduped per sender like resources.
    BuffSnapshotFanOut,
    /// Track 4 sub-task 4 — combat outcome from the attacker's POV. Pure
    /// visual fan-out; payload travels verbatim in the matching server
    /// variant.
    HitFanOut {
        target: u64,
        amount: i32,
        crit: bool,
        dmg_type: protocol::world::DamageType,
    },
    MissFanOut {
        target: u64,
    },
    EvadeFanOut {
        target: u64,
    },
    /// Track 4 sub-task 5 — dying client signaled HP-zero. Tick loop fans
    /// out EntityDied to in_world peers.
    DeathFanOut,
    /// Track 22.H — player retargeted. Tick loop fans an `EntityTarget`
    /// (reused from the enemy-target broadcast) so peers' ToT frames
    /// can resolve what the tracked remote player is attacking.
    PlayerTargetFanOut {
        target: Option<protocol::world::EntityId>,
    },
    /// Track 6 sub-task 2 — player → server attack intent. The handler
    /// has already validated `conn.in_world` and the message decoded
    /// cleanly; the tick loop runs `combat::calc_swing` against the
    /// attacker's PerConnection + weapon path, then validates the
    /// target (alive, in range) and applies the resulting damage.
    AttackIntent {
        attacker: u64,
        target_id: protocol::world::EntityId,
        weapon_path: String,
        is_offhand: bool,
        dmg_type: protocol::world::DamageType,
    },

    /// Track 6 sub-task 3b — player → server spell-cast intent. The
    /// tick loop resolves the spell name in spells.toml, validates
    /// mana / target, applies the authoritative damage or heal, and
    /// fans Hit + HealthUpdate / ManaUpdate.
    ///
    /// Track 10 — `cast_name_at_dispatch` / `cast_set_at_at_dispatch`
    /// snapshot the caster's cast cache at the moment this CastSpell
    /// was decoded. The tick loop's gate uses them to verify that a
    /// matching CastStartBroadcast actually ran for long enough. We
    /// snapshot here (rather than re-reading conn at gate time)
    /// because CastComplete arrives in the same incoming batch as
    /// CastSpell and would clear the cache before the gate fires.
    CastSpellIntent {
        caster: u64,
        spell_name: String,
        target_id: Option<protocol::world::EntityId>,
        cast_name_at_dispatch: String,
        cast_set_at_at_dispatch: Option<std::time::Instant>,
        /// Track 17.2 — caster pos at CastStart time, snapshotted at
        /// dispatch for the movement-during-cast gate. Vec3f::ZERO when
        /// no cast was in flight (instant casts skip the gate anyway).
        cast_start_pos_at_dispatch: super::connection::Vec3f,
    },

    /// Track 6 sub-task 5 — player → server group intents. The tick
    /// loop's post-dispatch sweep resolves them against the
    /// groups::GroupManager singleton.
    GroupInviteIntent {
        inviter: u64,
        target_name: String,
    },
    GroupAcceptIntent {
        invitee: u64,
        from: u64,
    },
    GroupLeaveIntent {
        member: u64,
    },
    GroupKickIntent {
        leader: u64,
        target_name: String,
    },
    /// Track 12 Piece A — player → server pet command. Tick loop
    /// resolves the owner's pet, validates the target if `command ==
    /// ATTACK`, then sets `pet.target` + `pet.command_at` (sticky
    /// override that beats `last_attacked_enemy` inheritance until
    /// it decays).
    PetCommandIntent {
        owner: u64,
        command: u8,
        target_id: Option<protocol::world::EntityId>,
    },

    /// Track 13.2 — player → server inventory move. Tick loop
    /// validates src/dst, mutates `PerConnection.inventory`, and
    /// fans `InventoryDelta` for each affected slot.
    MoveItemIntent {
        owner: u64,
        src_location: String,
        src_slot: u32,
        dst_location: String,
        dst_slot: u32,
    },

    /// Track 13.2.b — split `count` items off src into dst. Both
    /// locations are 'base' for now; bag locations defer to 13.2.c.
    SplitStackIntent {
        owner: u64,
        src_location: String,
        src_slot: u32,
        dst_location: String,
        dst_slot: u32,
        count: u32,
    },

    /// Track 13.2.b — drop `count` items at the player's feet as
    /// a server-owned loot bag. `count == 0` drops the whole stack.
    DropItemIntent {
        owner: u64,
        location: String,
        slot: u32,
        count: u32,
    },

    /// Track 15.1 — destroy `count` items outright (no loot bag, no
    /// recovery). `count == 0` removes the whole stack.
    DestroyItemIntent {
        owner: u64,
        location: String,
        slot: u32,
        count: u32,
    },

    /// Track 15.2 — consume one unit of the entry at
    /// `(location, slot)`. Server validates the item is a consumable
    /// (food / drink / heal-on-use), decrements the stack, fans
    /// `InventoryDelta`, and applies the heal / food-buff / drink-buff
    /// via the existing buff + resource pipeline (fans
    /// `HealthUpdate` / `ManaUpdate` / `BuffSnapshot` as appropriate).
    UseConsumableIntent {
        owner: u64,
        location: String,
        slot: u32,
    },

    /// Track 15.2 follow-up — server-side GM `/give` so client-only
    /// items stop diverging from server inventory. Pre-parsed in the
    /// dispatch arm: the line `give <item name> [qty]` becomes
    /// `(item_name, qty)`. Apply phase looks up the item by name and
    /// calls `add_item_locating`, then fans `InventoryDelta` per
    /// touched slot. Until the accounts table grows an `is_gm` flag,
    /// any in-world client can issue this.
    GmGiveIntent {
        owner: u64,
        item_name: String,
        qty: u32,
    },

    /// Track 13.3 — equip item from a base slot into a paperdoll
    /// slot (potentially swapping with the previously equipped
    /// item).
    EquipItemIntent {
        owner: u64,
        src_location: String,
        src_slot: u32,
        equip_slot: u8,
    },

    /// Track 13.3 — unequip item from a paperdoll slot into a base
    /// slot (potentially swapping with the previously held item).
    UnequipItemIntent {
        owner: u64,
        equip_slot: u8,
        dst_location: String,
        dst_slot: u32,
    },

    /// Track 14 follow-up — buy `qty` of `item_name` from the
    /// vendor identified by `vendor_id`. Server validates the item
    /// exists in the registry, charges `vendor_price * qty` from
    /// the player's coins, and grants the stack via
    /// `add_item_locating`. The vendor_id is currently informational
    /// (server doesn't yet have NPCs so stock-by-vendor validation
    /// is deferred); registry price + coin balance + inventory cap
    /// are all enforced.
    BuyItemIntent {
        owner: u64,
        vendor_id: protocol::world::EntityId,
        item_name: String,
        qty: u32,
    },

    /// Track 14 follow-up — sell `qty` items from the player's
    /// `slot` (base or bag). Server looks up the item, computes
    /// `vendor_price / 2 * qty`, credits coins, removes from
    /// inventory. Equip-slot sells are rejected (sell from your
    /// paperdoll isn't a thing — unequip first).
    SellItemIntent {
        owner: u64,
        slot: protocol::world::SlotRef,
        qty: u32,
    },

    /// Track 5 sub-task 4 — player → server pickup intent for one slot
    /// of a loot bag. The tick loop validates bag existence + slot
    /// index + pickup range, removes the stack, sends `LootGranted`
    /// privately, and either re-broadcasts the bag (still has items)
    /// or EntityDespawns it (empty now).
    LootItemIntent {
        looter: u64,
        bag_id: protocol::world::EntityId,
        slot: u32,
    },
    /// Player → server "take everything in the bag" intent. Behaves
    /// like a LootItemIntent for every remaining slot, ordered by the
    /// bag's current item list.
    LootAllIntent {
        looter: u64,
        bag_id: protocol::world::EntityId,
    },
    /// Player → server chat. Handler returns this; the tick loop fans
    /// `ChatMessage` to recipients based on `channel`:
    /// - Say  → in-AOI peers (3×3 cell neighbourhood, sender excluded)
    /// - Shout / Ooc → every in-world peer (sender excluded)
    /// - Tell → the one connection whose `name` matches `target_name`
    ///   (case-insensitive). On no-match the tick loop sends a system
    ///   `ChatMessage` back to the sender.
    /// - Other channels (Group, Guild, Raid, Auction, System) are
    ///   intentionally ignored here — Group is RPC-driven by
    ///   GroupManager today; the others are unimplemented.
    ChatFanOut {
        channel: protocol::world::ChatChannel,
        text: String,
        speaker: String,
        target_name: Option<String>,
    },
    /// Player → server inspect request. Tick loop looks up the target's
    /// connection by char_id, reads `inventory.equipment`, and sends
    /// `InspectResult` back to the inspector only (using the dispatch
    /// `client_id` directly, so this variant carries only the target).
    InspectIntent {
        target_char_id: i64,
    },
}

pub fn handle_message(
    server: &mut RenetServer,
    conn: &mut PerConnection,
    client_id: ClientId,
    msg: ClientWorldMsg,
    now: Instant,
) -> Outcome {
    conn.touch(now);
    match msg {
        ClientWorldMsg::Connect {
            session_token: _,
            char_id,
            client_version: _,
        } => {
            // The renet ConnectToken's `client_id` already proved this
            // connection owns `char_id` (auth signed it). Defense-in-depth:
            // if the app-layer message disagrees, hang up.
            if char_id != conn.char_id as u64 {
                tracing::warn!(
                    expected = conn.char_id,
                    got = char_id,
                    %client_id,
                    "Connect char_id mismatch — kicking"
                );
                send_kick(server, client_id, KickCode::Unknown, "char_id mismatch");
                return Outcome::Disconnect;
            }
            if conn.ready {
                // Duplicate Connect — ignore, don't tear down.
                return Outcome::Continue;
            }
            conn.ready = true;
            send_connect_ok(server, client_id, conn);
            // Initial position so the client has something to render against
            // before the first broadcast tick.
            send_position(server, client_id, conn);
            // Fan-out is deferred until the client signals it has left the
            // lobby (ClientWorldMsg::EnterWorld). Until then peers don't
            // know about this client.
            Outcome::Continue
        }

        ClientWorldMsg::EnterWorld => {
            if !conn.ready {
                // Out of order — EnterWorld without a completed Connect.
                // Drop silently; a misbehaving client doesn't deserve a
                // kick for this, and a correct client never sends it.
                return Outcome::Continue;
            }
            if conn.in_world {
                // Duplicate (player went to lobby and back? not supported
                // yet, but defensive). Ignore.
                return Outcome::Continue;
            }
            conn.in_world = true;
            Outcome::JustEnteredWorld
        }

        ClientWorldMsg::Disconnect => {
            tracing::info!(char_id = conn.char_id, "client requested disconnect");
            Outcome::Disconnect
        }

        ClientWorldMsg::Heartbeat => Outcome::Continue,

        ClientWorldMsg::Move {
            sequence,
            direction,
            jumping: _,
        } => {
            if !conn.ready {
                // Client started sending moves before completing the
                // handshake — drop silently.
                return Outcome::Continue;
            }
            // Drop out-of-order Move packets (unreliable channel can reorder).
            // Also handles wraparound: u32 holds ~2.4 years at 20 Hz.
            if sequence <= conn.last_move_seq && conn.last_move_seq != 0 {
                return Outcome::Continue;
            }
            conn.last_move_seq = sequence;

            // Track 6 sub-task 4d — CC gating. Rooted or mezzed players
            // can't move; store a zero direction so the tick loop holds
            // their position. The sequence number still increments so
            // out-of-order detection stays correct.
            let dir_v = if super::buffs::is_mezzed(&conn.active_buffs)
                || super::buffs::is_rooted(&conn.active_buffs)
            {
                Vec3f::ZERO
            } else {
                // Y is zeroed — server does not simulate gravity or jumping.
                // Only XZ is authoritative; vertical position stays at spawn height.
                let v = Vec3f { x: direction.x, y: 0.0, z: direction.z };
                v.clamp_length(1.0)
            };
            // Store the latest intent for the tick loop to integrate exactly
            // once per tick. Integrating here would advance pos N times when
            // N Moves arrive between ticks — at typical client send rates
            // that's a ~3× speedup. Server-authoritative speed cap is
            // enforced by clamping the direction to unit length; the tick
            // loop multiplies by MAX_MOVE_SPEED × TICK_DT.
            conn.latest_direction = dir_v;
            conn.last_move_received = Some(now);
            Outcome::Continue
        }

        ClientWorldMsg::CastStartBroadcast { spell_name, duration } => {
            if !conn.ready {
                return Outcome::Continue;
            }
            conn.cast_spell_name = spell_name.clone();
            conn.cast_total_duration = duration;
            conn.cast_set_at = Some(now);
            // Track 17.2 — snapshot the caster's position so the
            // CastSpell gate can reject movement-during-cast forgeries.
            conn.cast_start_pos = conn.pos;
            Outcome::CastStartFanOut { spell_name, duration }
        }

        ClientWorldMsg::CastCompleteBroadcast { spell_name } => {
            if !conn.ready {
                return Outcome::Continue;
            }
            conn.cast_spell_name.clear();
            conn.cast_total_duration = 0.0;
            conn.cast_set_at = None;
            Outcome::CastCompleteFanOut { spell_name }
        }

        ClientWorldMsg::HitBroadcast {
            target,
            amount,
            crit,
            dmg_type,
        } => {
            if !conn.ready {
                return Outcome::Continue;
            }
            Outcome::HitFanOut {
                target,
                amount,
                crit,
                dmg_type,
            }
        }

        ClientWorldMsg::MissBroadcast { target } => {
            if !conn.ready {
                return Outcome::Continue;
            }
            Outcome::MissFanOut { target }
        }

        ClientWorldMsg::EvadeBroadcast { target } => {
            if !conn.ready {
                return Outcome::Continue;
            }
            Outcome::EvadeFanOut { target }
        }

        ClientWorldMsg::SetTarget { target_id } => {
            // Track 22.H — peer target broadcast. The local client
            // fires this whenever the player retargets so peers can
            // render the target-of-target frame for tracked remote
            // players. Server reuses the existing EntityTarget
            // ServerWorldMsg variant (already used by enemy AI).
            if !conn.ready {
                return Outcome::Continue;
            }
            conn.current_target = target_id;
            Outcome::PlayerTargetFanOut { target: target_id }
        }

        ClientWorldMsg::DeathBroadcast => {
            if !conn.ready {
                return Outcome::Continue;
            }
            // Cast cache cleared on death — a corpse isn't mid-cast.
            // Track 6 also zeroes conn.hp + flags the broadcast baseline
            // dirty so the next regen tick fans HealthUpdate(0). Peer
            // RemotePlayer.apply_health_update needs that drop-to-zero
            // before the matching Respawn HealthUpdate(>0) flips
            // _apply_respawn(). Sub-task 3 will lift death detection
            // fully server-side; for now the dying client is still the
            // source of truth for "I died."
            conn.cast_spell_name.clear();
            conn.cast_total_duration = 0.0;
            conn.cast_set_at = None;
            conn.hp = 0.0;
            // Round-7 playtest fix — food / drink and other timed buffs
            // shouldn't survive a death. The client's BuffManager
            // already calls clear_all on PlayerDeath.player_died, but
            // those buffs are server-authoritative in launcher mode —
            // without clearing here too the next BuffSnapshot fan-out
            // restores them on the corpse.
            conn.active_buffs.clear();
            super::regen::mark_dirty(conn);
            Outcome::DeathFanOut
        }

        ClientWorldMsg::EquipUpdate { armor } => {
            // Track 14.2 — deprecated. The server now derives
            // `equipped_armor` (and stat / max-HP / max-MP bonuses)
            // from the registry via `inventory::recompute_equipped_stats`
            // whenever the equipment map changes. Old clients still
            // send this; we log + ignore. A later track removes it
            // from the wire entirely.
            if !conn.ready {
                return Outcome::Continue;
            }
            tracing::debug!(
                char_id = conn.char_id,
                client_armor_claim = armor,
                server_armor = conn.equipped_armor,
                "EquipUpdate received — ignored (server-derived since Track 14.2)"
            );
            Outcome::Continue
        }

        ClientWorldMsg::PvpToggle { on } => {
            if !conn.ready {
                return Outcome::Continue;
            }
            conn.pvp_override_on = on;
            tracing::info!(
                char_id = conn.char_id,
                on,
                "dev PvP override toggled"
            );
            Outcome::Continue
        }

        ClientWorldMsg::SetAutosplit { on } => {
            if !conn.ready {
                return Outcome::Continue;
            }
            conn.autosplit = on;
            tracing::info!(char_id = conn.char_id, on, "autosplit toggled");
            Outcome::Continue
        }

        ClientWorldMsg::DamageSelf { amount } => {
            if !conn.in_world || conn.hp <= 0.0 || !conn.is_dev {
                return Outcome::Continue;
            }
            let delta = amount.max(0) as f32;
            conn.hp = (conn.hp - delta).max(0.0);
            super::regen::mark_dirty(conn);
            tracing::info!(
                char_id = conn.char_id,
                amount = delta,
                new_hp = conn.hp,
                "dev damage self"
            );
            Outcome::Continue
        }

        ClientWorldMsg::HealSelf { amount } => {
            if !conn.in_world || !conn.is_dev {
                return Outcome::Continue;
            }
            let delta = amount.max(0) as f32;
            conn.hp = (conn.hp + delta).min(conn.max_hp);
            // The Test Panel "Full Heal" dev button backs this message;
            // restore MP + stamina too. These are server-authoritative, so a
            // client-only set leaves the server's mp drained and casts keep
            // rejecting — this is the only path that actually tops mana off.
            // mark_dirty fans HealthUpdate / ManaUpdate / StaminaUpdate.
            conn.mp = conn.max_mp;
            conn.stamina = conn.max_stamina;
            super::regen::mark_dirty(conn);
            tracing::info!(
                char_id = conn.char_id,
                amount = delta,
                new_hp = conn.hp,
                "dev full restore (hp/mp/stamina)"
            );
            Outcome::Continue
        }

        ClientWorldMsg::GiveCoins { platinum, gold, silver, copper } => {
            if !conn.in_world || !conn.is_dev {
                return Outcome::Continue;
            }
            // Exact per-tier credit, no reduction — a raw-copper grant must
            // stay raw copper (that's what makes it useful for encumbrance
            // testing). Negative grants allowed (dev drain), floored at 0.
            conn.coins.platinum = conn.coins.platinum.saturating_add(platinum).max(0);
            conn.coins.gold = conn.coins.gold.saturating_add(gold).max(0);
            conn.coins.silver = conn.coins.silver.saturating_add(silver).max(0);
            conn.coins.copper = conn.coins.copper.saturating_add(copper).max(0);
            conn.coins_dirty = true;
            send_coins_update(server, client_id, conn.coins);
            tracing::info!(
                char_id = conn.char_id,
                platinum, gold, silver, copper,
                total_copper = conn.coins.total_copper(),
                "dev coin grant"
            );
            Outcome::Continue
        }

        ClientWorldMsg::Respawn => {
            if !conn.ready {
                return Outcome::Continue;
            }
            // The dying client's local respawn timer elapsed. Reset
            // resources to the weakened post-respawn levels (matching
            // autoloads/player_death.gd's 25% / 25% / 50% multipliers
            // so the client's local set_hp doesn't immediately diverge
            // from the server's view). Sub-task 3 lifts the timer +
            // multipliers fully server-side. mark_dirty forces a
            // fan-out on the next regen tick so peer RemotePlayer
            // bars stand back up.
            conn.hp = conn.max_hp * 0.25;
            conn.mp = conn.max_mp * 0.25;
            conn.stamina = conn.max_stamina * 0.50;
            conn.regen_hp_acc = 0.0;
            conn.regen_mp_acc = 0.0;
            conn.regen_stamina_acc = 0.0;
            super::regen::mark_dirty(conn);
            Outcome::Continue
        }

        ClientWorldMsg::BuffSnapshotBroadcast { buffs: _ } => {
            // Track 6 sub-task 4a: server is now the source of truth
            // for buff state. The client's snapshot is ignored — the
            // server-originated fan-out from tick.rs step 5a is the
            // canonical signal. Variant kept for transitional builds;
            // sub-task 4b removes the wire variant entirely.
            Outcome::Continue
        }

        ClientWorldMsg::CastFailBroadcast { reason } => {
            if !conn.ready {
                return Outcome::Continue;
            }
            conn.cast_spell_name.clear();
            conn.cast_total_duration = 0.0;
            conn.cast_set_at = None;
            Outcome::CastFailFanOut { reason }
        }

        ClientWorldMsg::Sit => {
            // Track 6: regen rate scales while seated. Movement (any Move
            // with non-zero direction) auto-stands the client via the
            // GDScript player; the server falls back to flipping the flag
            // off on movement integration too, in case the Stand intent
            // dropped on the wire.
            if !conn.ready {
                return Outcome::Continue;
            }
            conn.is_sitting = true;
            Outcome::Continue
        }

        ClientWorldMsg::Stand => {
            if !conn.ready {
                return Outcome::Continue;
            }
            conn.is_sitting = false;
            Outcome::Continue
        }

        ClientWorldMsg::Attack {
            target_id,
            weapon_path,
            is_offhand,
            dmg_type,
        } => {
            if !conn.in_world {
                // Pre-EnterWorld clients can't engage in combat; drop
                // silently rather than kick (lobby could race with
                // legitimate UI interactions).
                return Outcome::Continue;
            }
            // Track 6 sub-task 4d — Mez gates Attack.
            if super::buffs::is_mezzed(&conn.active_buffs) {
                return Outcome::Continue;
            }
            Outcome::AttackIntent {
                attacker: conn.char_id as u64,
                target_id,
                weapon_path,
                is_offhand,
                dmg_type,
            }
        }

        ClientWorldMsg::GroupInvite { name } => {
            if !conn.in_world {
                return Outcome::Continue;
            }
            Outcome::GroupInviteIntent {
                inviter: conn.char_id as u64,
                target_name: name,
            }
        }

        ClientWorldMsg::GroupAcceptInvite { from } => {
            if !conn.in_world {
                return Outcome::Continue;
            }
            Outcome::GroupAcceptIntent {
                invitee: conn.char_id as u64,
                from,
            }
        }

        ClientWorldMsg::GroupLeave => {
            if !conn.in_world {
                return Outcome::Continue;
            }
            Outcome::GroupLeaveIntent { member: conn.char_id as u64 }
        }

        ClientWorldMsg::GroupKick { name } => {
            if !conn.in_world {
                return Outcome::Continue;
            }
            Outcome::GroupKickIntent {
                leader: conn.char_id as u64,
                target_name: name,
            }
        }

        ClientWorldMsg::CastSpell { spell_name, target_id } => {
            if !conn.in_world {
                return Outcome::Continue;
            }
            // Track 6 sub-task 4d — Silence and Mez gate CastSpell. Fan
            // a CastFail back so the caster's client can log "Silenced!"
            // and the local Spells cooldown / mana doesn't sit stuck.
            if super::buffs::is_silenced(&conn.active_buffs) {
                tracing::info!(
                    caster = conn.char_id,
                    spell = %spell_name,
                    "cast rejected — silenced"
                );
                return Outcome::CastFailFanOut { reason: "Silenced.".to_string() };
            }
            if super::buffs::is_mezzed(&conn.active_buffs) {
                tracing::info!(
                    caster = conn.char_id,
                    spell = %spell_name,
                    "cast rejected — mezzed"
                );
                return Outcome::CastFailFanOut { reason: "Mesmerized.".to_string() };
            }
            // Track 10 — snapshot the cast cache state *now*, before
            // any CastCompleteBroadcast in the same batch wipes it.
            // Track 17.2 — also snapshot cast_start_pos for the
            // movement gate.
            let cast_name_at_dispatch = conn.cast_spell_name.clone();
            let cast_set_at_at_dispatch = conn.cast_set_at;
            let cast_start_pos_at_dispatch = conn.cast_start_pos;
            Outcome::CastSpellIntent {
                caster: conn.char_id as u64,
                spell_name,
                target_id,
                cast_name_at_dispatch,
                cast_set_at_at_dispatch,
                cast_start_pos_at_dispatch,
            }
        }

        ClientWorldMsg::LootItem { bag_id, slot } => {
            if !conn.in_world {
                return Outcome::Continue;
            }
            Outcome::LootItemIntent {
                looter: conn.char_id as u64,
                bag_id,
                slot,
            }
        }

        ClientWorldMsg::LootAll { bag_id } => {
            if !conn.in_world {
                return Outcome::Continue;
            }
            Outcome::LootAllIntent {
                looter: conn.char_id as u64,
                bag_id,
            }
        }

        ClientWorldMsg::PetCommand { command, target_id } => {
            if !conn.in_world {
                return Outcome::Continue;
            }
            Outcome::PetCommandIntent {
                owner: conn.char_id as u64,
                command,
                target_id,
            }
        }

        ClientWorldMsg::MoveItem {
            src_location,
            src_slot,
            dst_location,
            dst_slot,
        } => {
            if !conn.in_world {
                return Outcome::Continue;
            }
            Outcome::MoveItemIntent {
                owner: conn.char_id as u64,
                src_location,
                src_slot,
                dst_location,
                dst_slot,
            }
        }

        ClientWorldMsg::SplitStack {
            src_location,
            src_slot,
            dst_location,
            dst_slot,
            count,
        } => {
            if !conn.in_world {
                return Outcome::Continue;
            }
            Outcome::SplitStackIntent {
                owner: conn.char_id as u64,
                src_location,
                src_slot,
                dst_location,
                dst_slot,
                count,
            }
        }

        ClientWorldMsg::DropItem {
            location,
            slot,
            count,
        } => {
            if !conn.in_world {
                return Outcome::Continue;
            }
            Outcome::DropItemIntent {
                owner: conn.char_id as u64,
                location,
                slot,
                count,
            }
        }

        ClientWorldMsg::DestroyItem {
            location,
            slot,
            count,
        } => {
            if !conn.in_world {
                return Outcome::Continue;
            }
            Outcome::DestroyItemIntent {
                owner: conn.char_id as u64,
                location,
                slot,
                count,
            }
        }

        ClientWorldMsg::UseConsumable { slot } => {
            if !conn.in_world {
                return Outcome::Continue;
            }
            // SlotRef → wire `(location, slot)` tuple the apply phase
            // already understands. Equip-slot consumables aren't a
            // thing (no consumable is equippable) — reject early.
            let (location, slot_idx): (String, u32) = match slot {
                protocol::world::SlotRef::BaseSlot { idx } => ("base".to_string(), idx as u32),
                protocol::world::SlotRef::BagSlot { base, slot } => {
                    (format!("bag_{base}"), slot as u32)
                }
                protocol::world::SlotRef::EquipSlot(_) => {
                    return Outcome::Continue;
                }
            };
            Outcome::UseConsumableIntent {
                owner: conn.char_id as u64,
                location,
                slot: slot_idx,
            }
        }

        ClientWorldMsg::EquipItem {
            src_location,
            src_slot,
            equip_slot,
        } => {
            if !conn.in_world {
                return Outcome::Continue;
            }
            Outcome::EquipItemIntent {
                owner: conn.char_id as u64,
                src_location,
                src_slot,
                equip_slot,
            }
        }

        ClientWorldMsg::UnequipItem {
            equip_slot,
            dst_location,
            dst_slot,
        } => {
            if !conn.in_world {
                return Outcome::Continue;
            }
            Outcome::UnequipItemIntent {
                owner: conn.char_id as u64,
                equip_slot,
                dst_location,
                dst_slot,
            }
        }

        ClientWorldMsg::BuyItem {
            vendor_id,
            item_name,
            qty,
        } => {
            if !conn.in_world || qty == 0 {
                return Outcome::Continue;
            }
            Outcome::BuyItemIntent {
                owner: conn.char_id as u64,
                vendor_id,
                item_name,
                qty,
            }
        }

        ClientWorldMsg::SellItem { slot, qty } => {
            if !conn.in_world || qty == 0 {
                return Outcome::Continue;
            }
            Outcome::SellItemIntent {
                owner: conn.char_id as u64,
                slot,
                qty,
            }
        }

        ClientWorldMsg::GmCommand { line } => {
            if !conn.in_world {
                return Outcome::Continue;
            }
            // Parse `give <item name> [qty]`. Trailing integer = stack
            // count; multi-word names with spaces remain intact. Matches
            // the GDScript `/give` parser in hud.gd.
            let trimmed = line.trim();
            if let Some(rest) = trimmed.strip_prefix("give ") {
                let mut tokens: Vec<&str> = rest.split_whitespace().collect();
                if tokens.is_empty() {
                    return Outcome::Continue;
                }
                let qty: u32 = if tokens.len() > 1 {
                    if let Ok(n) = tokens[tokens.len() - 1].parse::<u32>() {
                        tokens.pop();
                        n
                    } else {
                        1
                    }
                } else {
                    1
                };
                if qty == 0 {
                    return Outcome::Continue;
                }
                let item_name = tokens.join(" ");
                if item_name.is_empty() {
                    return Outcome::Continue;
                }
                return Outcome::GmGiveIntent {
                    owner: conn.char_id as u64,
                    item_name,
                    qty,
                };
            }
            // Unknown gm sub-command — log and drop.
            tracing::info!(char_id = conn.char_id, %line, "unknown GmCommand");
            Outcome::Continue
        }

        // The other ~30 ClientWorldMsg variants land in later tracks.
        // Unknown-but-decoded messages: ignore, don't kick. Unknown-and-
        // failed-to-decode messages don't reach here (decode error is
        // logged in tick.rs).
        ClientWorldMsg::InspectPlayer { target_char_id } => {
            if !conn.in_world {
                return Outcome::Continue;
            }
            Outcome::InspectIntent { target_char_id }
        }

        ClientWorldMsg::Chat { channel, text, target_name } => {
            if !conn.in_world {
                return Outcome::Continue;
            }
            // Hard cap to keep payloads sane. Clients should validate but
            // a hostile or buggy client could still try to flood.
            const MAX_CHAT_LEN: usize = 512;
            let trimmed = if text.len() > MAX_CHAT_LEN {
                text[..MAX_CHAT_LEN].to_string()
            } else {
                text
            };
            Outcome::ChatFanOut {
                channel,
                text: trimmed,
                speaker: conn.name.clone(),
                target_name,
            }
        }

        _ => Outcome::Continue,
    }
}

/// Encode a Position broadcast for `conn`. Returned bytes can be cloned and
/// sent to multiple recipients per tick; the tick loop calls this once per
/// sender and reuses the bytes across the fan-out.
pub fn build_position_msg(conn: &PerConnection) -> Option<Vec<u8>> {
    let msg = ServerWorldMsg::Position {
        id: conn.char_id as u64,
        pos: Vec3 { x: conn.pos.x, y: conn.pos.y, z: conn.pos.z },
        vel: Vec3 { x: 0.0, y: 0.0, z: 0.0 },
        yaw: conn.yaw,
        sequence: conn.last_move_seq,
    };
    encode(&msg)
}

fn send_position(server: &mut RenetServer, client_id: ClientId, conn: &PerConnection) {
    if let Some(bytes) = build_position_msg(conn) {
        server.send_message(client_id, CHANNEL_POSITION, bytes);
    }
}

/// Send an EntitySpawn for `conn` to a single recipient. Used both for the
/// "new client → existing peers" and "existing peers → new client" sides of
/// the connection handshake fan-out, plus any future per-recipient AOI
/// transitions.
pub fn send_entity_spawn(
    server: &mut RenetServer,
    recipient_id: ClientId,
    conn: &PerConnection,
) {
    let msg = ServerWorldMsg::EntitySpawn {
        id: conn.char_id as u64,
        name: conn.name.clone(),
        race: conn.race.clone(),
        class: conn.class.clone(),
        level: conn.level.max(0) as u32,
        pos: Vec3 { x: conn.pos.x, y: conn.pos.y, z: conn.pos.z },
        yaw: conn.yaw,
    };
    if let Some(bytes) = encode(&msg) {
        server.send_message(recipient_id, CHANNEL_SYSTEM, bytes);
    }
}

/// Send an EntityDespawn for `entity_id` to a single recipient. Reliable
/// system channel so the despawn is guaranteed to arrive even if the
/// preceding Position broadcasts are dropped on the unreliable channel.
pub fn send_entity_despawn(
    server: &mut RenetServer,
    recipient_id: ClientId,
    entity_id: u64,
) {
    let msg = ServerWorldMsg::EntityDespawn { id: entity_id };
    if let Some(bytes) = encode(&msg) {
        server.send_message(recipient_id, CHANNEL_SYSTEM, bytes);
    }
}

/// Private `LootGranted` to a single recipient — the looter whose
/// `LootItem` / `LootAll` intent landed. The client adds the stack
/// to local inventory; the bag's wire-side state goes out separately
/// to every in_world peer as a fresh `LootBagSpawn` snapshot.
pub fn send_loot_granted(
    server: &mut RenetServer,
    recipient_id: ClientId,
    item_path: String,
    count: u32,
) {
    let msg = ServerWorldMsg::LootGranted { item_path, count };
    if let Some(bytes) = encode(&msg) {
        server.send_message(recipient_id, CHANNEL_SYSTEM, bytes);
    }
}

/// Track 6 sub-task 5 — forward a pending group invite to the
/// invitee. Sent on the reliable system channel; client shows an
/// accept/reject UI.
pub fn send_group_invited(
    server: &mut RenetServer,
    invitee_id: ClientId,
    from_id: u64,
    from_name: String,
) {
    let msg = ServerWorldMsg::GroupInvited { from_id, from_name };
    if let Some(bytes) = encode(&msg) {
        server.send_message(invitee_id, CHANNEL_SYSTEM, bytes);
    }
}

/// Track 6 sub-task 5 — fan the current group roster to every online
/// member. Empty `members` Vec signals the group dissolved (sent to
/// the last remaining member so their client clears its display).
pub fn fan_group_roster(
    server: &mut RenetServer,
    recipients: &[ClientId],
    group_id: u64,
    leader_id: u64,
    members: Vec<(u64, String)>,
) {
    if recipients.is_empty() {
        return;
    }
    let msg = ServerWorldMsg::GroupRoster {
        group_id,
        leader_id,
        members,
    };
    let Some(bytes) = encode(&msg) else { return };
    for &recipient in recipients {
        server.send_message(recipient, CHANNEL_SYSTEM, bytes.clone());
    }
}

/// Private XP grant to the kill-credit recipient. Mirrors
/// `send_loot_granted`'s single-recipient shape on the reliable system
/// channel. `current` / `to_next` are placeholders — the server doesn't
/// track player XP state in Track 5 (still client-authoritative); Track 6
/// will populate them.
pub fn send_xp_gained(server: &mut RenetServer, recipient_id: ClientId, amount: i32) {
    let msg = ServerWorldMsg::XpGained {
        amount,
        current: 0,
        to_next: 0,
    };
    if let Some(bytes) = encode(&msg) {
        server.send_message(recipient_id, CHANNEL_SYSTEM, bytes);
    }
}

/// Fan out a `LootBagSpawn` for `bag` to every recipient. Used for the
/// initial spawn fan-out on enemy death and (sub-task 4B) re-snapshots
/// when items are removed. Bag despawn rides the generic
/// `EntityDespawn` since the id partition lets the client route by id.
pub fn fan_out_loot_bag_spawn(
    server: &mut RenetServer,
    recipients: &[ClientId],
    bag: &LootBag,
) {
    if recipients.is_empty() {
        return;
    }
    let msg = ServerWorldMsg::LootBagSpawn {
        bag_id: bag.id,
        pos: Vec3 { x: bag.pos.x, y: bag.pos.y, z: bag.pos.z },
        items: bag.snapshot(),
    };
    let Some(bytes) = encode(&msg) else {
        return;
    };
    for &recipient_id in recipients {
        server.send_message(recipient_id, CHANNEL_SYSTEM, bytes.clone());
    }
}

/// Fan out a HealthUpdate for any entity id (player char_id or enemy
/// id). Encoded once and cloned per recipient.
pub fn fan_out_health_update(
    server: &mut RenetServer,
    recipients: &[ClientId],
    id: protocol::world::EntityId,
    hp: f32,
    max_hp: f32,
) {
    if recipients.is_empty() {
        return;
    }
    let msg = ServerWorldMsg::HealthUpdate { id, hp, max_hp };
    let Some(bytes) = encode(&msg) else {
        return;
    };
    for &recipient_id in recipients {
        server.send_message(recipient_id, CHANNEL_SYSTEM, bytes.clone());
    }
}

/// Track 6 sub-task 3b — fan out a ManaUpdate. Used by the CastSpell
/// handler after it deducts the spell's mana cost from the caster.
pub fn fan_out_mana_update(
    server: &mut RenetServer,
    recipients: &[ClientId],
    id: protocol::world::EntityId,
    mp: f32,
    max_mp: f32,
) {
    if recipients.is_empty() {
        return;
    }
    let msg = ServerWorldMsg::ManaUpdate { id, mp, max_mp };
    let Some(bytes) = encode(&msg) else {
        return;
    };
    for &recipient_id in recipients {
        server.send_message(recipient_id, CHANNEL_SYSTEM, bytes.clone());
    }
}

/// Build a Position message for `entity`. Returns encoded bytes for
/// the caller to clone-and-send per recipient — same pattern as
/// `build_position_msg` for players.
pub fn build_enemy_position_msg(entity: &Entity) -> Option<Vec<u8>> {
    let msg = ServerWorldMsg::Position {
        id: entity.id,
        pos: Vec3 { x: entity.pos.x, y: entity.pos.y, z: entity.pos.z },
        vel: Vec3 { x: 0.0, y: 0.0, z: 0.0 },
        yaw: entity.yaw,
        sequence: entity.seq,
    };
    encode(&msg)
}

/// Fan out an `EntityTarget` to every recipient. Used on enemy aggro-
/// target switch. Encoded once, cloned per recipient.
pub fn fan_out_entity_target(
    server: &mut RenetServer,
    recipients: &[ClientId],
    id: protocol::world::EntityId,
    target: Option<protocol::world::EntityId>,
) {
    if recipients.is_empty() {
        return;
    }
    let msg = ServerWorldMsg::EntityTarget { id, target };
    let Some(bytes) = encode(&msg) else {
        return;
    };
    for &recipient_id in recipients {
        server.send_message(recipient_id, CHANNEL_SYSTEM, bytes.clone());
    }
}

pub fn fan_out_chat_message(
    server: &mut RenetServer,
    recipients: &[ClientId],
    speaker: &str,
    channel: protocol::world::ChatChannel,
    text: &str,
) {
    if recipients.is_empty() {
        return;
    }
    let msg = ServerWorldMsg::ChatMessage {
        speaker: speaker.to_string(),
        channel,
        text: text.to_string(),
        // Language gating isn't wired up server-side yet; empty string
        // tells the client "no specific language", which falls back to
        // the receiver's native rendering.
        lang: String::new(),
    };
    let Some(bytes) = encode(&msg) else {
        return;
    };
    for &recipient_id in recipients {
        server.send_message(recipient_id, CHANNEL_SYSTEM, bytes.clone());
    }
}

pub fn send_inspect_result(
    server: &mut RenetServer,
    recipient: ClientId,
    target_char_id: i64,
    target_name: String,
    slots: Vec<(u8, String)>,
) {
    let msg = ServerWorldMsg::InspectResult {
        target_char_id,
        target_name,
        slots,
    };
    if let Some(bytes) = encode(&msg) {
        server.send_message(recipient, CHANNEL_SYSTEM, bytes);
    }
}

/// Fan out `EnemySpawn` for `entity` to every recipient. Encoded once and
/// cloned per recipient. Used for live spawns (new mob appears → broadcast
/// to all in_world clients) and late-joiner seed (new client enters world
/// → server sends every living enemy).
pub fn fan_out_enemy_spawn(
    server: &mut RenetServer,
    recipients: &[ClientId],
    entity: &Entity,
) {
    if recipients.is_empty() {
        return;
    }
    let msg = ServerWorldMsg::EnemySpawn {
        id: entity.id,
        mob_name: entity.mob.name.clone(),
        level: entity.mob.level,
        max_hp: entity.max_hp,
        hp: entity.hp,
        pos: Vec3 { x: entity.pos.x, y: entity.pos.y, z: entity.pos.z },
        yaw: entity.yaw,
    };
    let Some(bytes) = encode(&msg) else {
        return;
    };
    for &recipient_id in recipients {
        server.send_message(recipient_id, CHANNEL_SYSTEM, bytes.clone());
    }
}

/// Track 13.2 — fan out a full `InventorySnapshot` privately to one
/// recipient (the owning client). Used to seed the client with its
/// persisted inventory on EnterWorld so the local UI renders from
/// authoritative state rather than the legacy local Inventory
/// autoload.
pub fn send_inventory_snapshot(
    server: &mut RenetServer,
    recipient: ClientId,
    entries: Vec<(String, u32, String, u32)>,
) {
    let msg = ServerWorldMsg::InventorySnapshot { entries };
    let Some(bytes) = encode(&msg) else { return };
    server.send_message(recipient, CHANNEL_SYSTEM, bytes);
}

/// Track 13.2 — fan an `InventoryDelta` privately to one recipient
/// when a single slot mutates (loot grant, MoveItem source / dest,
/// drop, equip). `item_path = None` clears the slot.
pub fn send_inventory_delta(
    server: &mut RenetServer,
    recipient: ClientId,
    location: String,
    slot: u32,
    item_path: Option<String>,
    count: u32,
) {
    let msg = ServerWorldMsg::InventoryDelta {
        location,
        slot,
        item_path,
        count,
    };
    let Some(bytes) = encode(&msg) else { return };
    server.send_message(recipient, CHANNEL_SYSTEM, bytes);
}

/// Track 11 — fan out `PetSpawn` for a freshly summoned pet. Same
/// pattern as `fan_out_enemy_spawn`; the variant carries the owner's
/// char_id so the client can route the pet under the right player.
/// Caller is responsible for `entity.owner.is_some()`.
pub fn fan_out_pet_spawn(
    server: &mut RenetServer,
    recipients: &[ClientId],
    entity: &Entity,
) {
    if recipients.is_empty() {
        return;
    }
    let owner = match entity.owner {
        Some(o) => o,
        None => return,
    };
    let msg = ServerWorldMsg::PetSpawn {
        id: entity.id,
        owner,
        pet_name: entity.mob.name.clone(),
        level: entity.mob.level,
        max_hp: entity.max_hp,
        hp: entity.hp,
        pos: Vec3 { x: entity.pos.x, y: entity.pos.y, z: entity.pos.z },
        yaw: entity.yaw,
    };
    let Some(bytes) = encode(&msg) else {
        return;
    };
    for &recipient_id in recipients {
        server.send_message(recipient_id, CHANNEL_SYSTEM, bytes.clone());
    }
}

/// Fan out `conn`'s cached resources to every recipient as three separate
/// ServerWorldMsg variants (HealthUpdate / ManaUpdate / StaminaUpdate).
/// Each variant is encoded once and the bytes cloned per recipient —
/// matches the Position fan-out pattern.
///
/// No-op if the connection has never broadcast a `ResourceUpdate` (we
/// have nothing meaningful to send and don't want to broadcast zeros).
/// Fan out a CastStart for `caster` to every recipient. Encoded once and
/// cloned per recipient. Used both for live broadcast (sender → other
/// in_world peers) and late-joiner seed (server → newcomer with remaining
/// duration).
pub fn fan_out_cast_start(
    server: &mut RenetServer,
    recipients: &[ClientId],
    caster: u64,
    spell_name: String,
    duration: f32,
) {
    if recipients.is_empty() {
        return;
    }
    let msg = ServerWorldMsg::CastStart {
        caster,
        spell_name,
        duration,
    };
    let Some(bytes) = encode(&msg) else { return };
    for recipient in recipients {
        server.send_message(*recipient, CHANNEL_SYSTEM, bytes.clone());
    }
}

pub fn fan_out_cast_complete(
    server: &mut RenetServer,
    recipients: &[ClientId],
    caster: u64,
    spell_name: String,
) {
    if recipients.is_empty() {
        return;
    }
    let msg = ServerWorldMsg::CastComplete {
        caster,
        spell_name,
    };
    let Some(bytes) = encode(&msg) else { return };
    for recipient in recipients {
        server.send_message(*recipient, CHANNEL_SYSTEM, bytes.clone());
    }
}

pub fn fan_out_cast_fail(
    server: &mut RenetServer,
    recipients: &[ClientId],
    caster: u64,
    reason: String,
) {
    if recipients.is_empty() {
        return;
    }
    let msg = ServerWorldMsg::CastFail { caster, reason };
    let Some(bytes) = encode(&msg) else { return };
    for recipient in recipients {
        server.send_message(*recipient, CHANNEL_SYSTEM, bytes.clone());
    }
}

/// Fan out a combat hit. The attacker's `caster` id, the target's id,
/// and the damage payload travel verbatim. Encoded once, cloned per
/// recipient — same pattern as the other fan-outs.
pub fn fan_out_hit(
    server: &mut RenetServer,
    recipients: &[ClientId],
    attacker: u64,
    target: u64,
    amount: i32,
    crit: bool,
    dmg_type: protocol::world::DamageType,
) {
    if recipients.is_empty() {
        return;
    }
    let msg = ServerWorldMsg::Hit {
        attacker,
        target,
        amount,
        crit,
        dmg_type,
    };
    let Some(bytes) = encode(&msg) else { return };
    for recipient in recipients {
        server.send_message(*recipient, CHANNEL_SYSTEM, bytes.clone());
    }
}

/// Track 6 — broadcast a DamageShieldTrigger so both attacker + defender
/// clients can log the reflect and render floating damage on the attacker.
pub fn fan_out_damage_shield_trigger(
    server: &mut RenetServer,
    recipients: &[ClientId],
    defender: u64,
    attacker: u64,
    amount: i32,
    shield_name: String,
) {
    if recipients.is_empty() {
        return;
    }
    let msg = ServerWorldMsg::DamageShieldTrigger {
        defender,
        attacker,
        amount,
        shield_name,
    };
    let Some(bytes) = encode(&msg) else { return };
    for recipient in recipients {
        server.send_message(*recipient, CHANNEL_SYSTEM, bytes.clone());
    }
}

pub fn fan_out_miss(
    server: &mut RenetServer,
    recipients: &[ClientId],
    attacker: u64,
    target: u64,
) {
    if recipients.is_empty() {
        return;
    }
    let msg = ServerWorldMsg::Miss { attacker, target };
    let Some(bytes) = encode(&msg) else { return };
    for recipient in recipients {
        server.send_message(*recipient, CHANNEL_SYSTEM, bytes.clone());
    }
}

pub fn fan_out_evade(
    server: &mut RenetServer,
    recipients: &[ClientId],
    attacker: u64,
    target: u64,
) {
    if recipients.is_empty() {
        return;
    }
    let msg = ServerWorldMsg::Evade { attacker, target };
    let Some(bytes) = encode(&msg) else { return };
    for recipient in recipients {
        server.send_message(*recipient, CHANNEL_SYSTEM, bytes.clone());
    }
}

pub fn fan_out_entity_died(
    server: &mut RenetServer,
    recipients: &[ClientId],
    entity_id: u64,
) {
    if recipients.is_empty() {
        return;
    }
    let msg = ServerWorldMsg::EntityDied { id: entity_id };
    let Some(bytes) = encode(&msg) else { return };
    for recipient in recipients {
        server.send_message(*recipient, CHANNEL_SYSTEM, bytes.clone());
    }
}

/// Fan out the buff snapshot for `conn` to every recipient. Track 6
/// sub-task 4a: derived from server-authoritative `active_buffs`
/// instead of the deprecated client-broadcast cache. Sends an empty
/// snapshot (representing "no buffs") if active_buffs is empty —
/// new joiners need to know peer state regardless.
pub fn fan_out_buff_snapshot(
    server: &mut RenetServer,
    recipients: &[ClientId],
    conn: &PerConnection,
) {
    if recipients.is_empty() {
        return;
    }
    let payload = super::buffs::snapshot_pairs(&conn.active_buffs);
    let msg = ServerWorldMsg::BuffSnapshot {
        target: conn.char_id as u64,
        buffs: payload,
    };
    let Some(bytes) = encode(&msg) else { return };
    for recipient in recipients {
        server.send_message(*recipient, CHANNEL_SYSTEM, bytes.clone());
    }
}

/// Track 14 follow-up — fan a `CoinsUpdate` privately to one
/// client. Vendor BuyItem / SellItem use this after mutating
/// `conn.coins`. Carries the full four-tier `Coins` wallet
/// (`ServerWorldMsg::CoinsUpdate`); gdext-net re-emits it as four ints.
pub fn send_coins_update(server: &mut RenetServer, client_id: ClientId, coins: Coins) {
    let msg = ServerWorldMsg::CoinsUpdate { coins };
    if let Some(bytes) = encode(&msg) {
        server.send_message(client_id, CHANNEL_SYSTEM, bytes);
    }
}

/// Track 18.1 — fan a per-skill advance event privately to the
/// owning client. Called from the attack / cast / armor-hit paths
/// when `skills::try_advance` returns Some.
pub fn send_skill_progress_update(
    server: &mut RenetServer,
    client_id: ClientId,
    kind: protocol::world::SkillKind,
    key: String,
    new_score: i32,
) {
    let msg = ServerWorldMsg::SkillProgressUpdate {
        kind,
        key,
        new_score: new_score.max(0) as u32,
    };
    if let Some(bytes) = encode(&msg) {
        server.send_message(client_id, CHANNEL_SYSTEM, bytes);
    }
}

/// Track 18.1 — seed the connecting client with all three skill
/// score maps. Sent on enter-world right after the inventory + coin
/// seeds so the GDScript autoloads have authoritative starting state
/// before any try_advance roll fires.
pub fn send_skill_progress_snapshot(
    server: &mut RenetServer,
    client_id: ClientId,
    conn: &PerConnection,
) {
    let (weapon, armor, casting) = super::skills::snapshot(conn);
    let msg = ServerWorldMsg::SkillProgressSnapshot {
        weapon,
        armor,
        casting,
    };
    if let Some(bytes) = encode(&msg) {
        server.send_message(client_id, CHANNEL_SYSTEM, bytes);
    }
}

/// Fan out the connection's current resources as three separate
/// ServerWorldMsg variants. Track 6 made these server-authoritative —
/// the values come straight from `conn.hp` / `conn.mp` / `conn.stamina`
/// (the DB-loaded snapshot at spawn, mutated by regen + combat).
pub fn fan_out_resources(
    server: &mut RenetServer,
    recipients: &[ClientId],
    conn: &PerConnection,
) {
    if recipients.is_empty() {
        return;
    }
    let id = conn.char_id as u64;
    let h = encode(&ServerWorldMsg::HealthUpdate {
        id,
        hp: conn.hp,
        max_hp: conn.max_hp,
    });
    let m = encode(&ServerWorldMsg::ManaUpdate {
        id,
        mp: conn.mp,
        max_mp: conn.max_mp,
    });
    let s = encode(&ServerWorldMsg::StaminaUpdate {
        id,
        stamina: conn.stamina,
        max: conn.max_stamina,
    });
    for recipient in recipients {
        if let Some(b) = &h {
            server.send_message(*recipient, CHANNEL_SYSTEM, b.clone());
        }
        if let Some(b) = &m {
            server.send_message(*recipient, CHANNEL_SYSTEM, b.clone());
        }
        if let Some(b) = &s {
            server.send_message(*recipient, CHANNEL_SYSTEM, b.clone());
        }
    }
}

fn send_connect_ok(server: &mut RenetServer, client_id: ClientId, conn: &PerConnection) {
    let msg = ServerWorldMsg::ConnectOk {
        player_id: conn.char_id as u64,
        name: conn.name.clone(),
        race: conn.race.clone(),
        class: conn.class.clone(),
        level: conn.level.max(0) as u32,
    };
    if let Some(bytes) = encode(&msg) {
        server.send_message(client_id, CHANNEL_SYSTEM, bytes);
    }
}

pub fn send_kick(server: &mut RenetServer, client_id: ClientId, code: KickCode, reason: &str) {
    let msg = ServerWorldMsg::Kick {
        reason: reason.to_string(),
        code,
        reconnect_after_secs: None,
    };
    if let Some(bytes) = encode(&msg) {
        server.send_message(client_id, CHANNEL_SYSTEM, bytes);
    }
}

fn encode(msg: &ServerWorldMsg) -> Option<Vec<u8>> {
    match bincode::serde::encode_to_vec(msg, bincode_cfg()) {
        Ok(b) => Some(b),
        Err(e) => {
            tracing::error!(error = %e, ?msg, "failed to bincode-encode ServerWorldMsg");
            None
        }
    }
}

/// Decode a single client message; returns None on protocol error.
pub fn decode_client(bytes: &[u8]) -> Option<ClientWorldMsg> {
    match bincode::serde::decode_from_slice::<ClientWorldMsg, _>(bytes, bincode_cfg()) {
        Ok((msg, _)) => Some(msg),
        Err(e) => {
            tracing::warn!(error = %e, "failed to bincode-decode ClientWorldMsg");
            None
        }
    }
}
