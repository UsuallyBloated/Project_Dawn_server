//! Rust GDExtension exposing a renet 2.0 client to GDScript.
//!
//! Wraps `RenetClient` + Secure-mode `NetcodeClientTransport` and bincode
//! ser/de against `protocol::world::*`. The wire format mirrors the server's
//! `crates/projectdawn-server` exactly via the shared `protocol` crate; any
//! drift would surface at compile time, not at runtime.
//!
//! Handled message types are decoded into typed signals: `ConnectOk`,
//! `Heartbeat`, `Kick`, `Position`, `EntitySpawn`, `EntityDespawn`,
//! `HealthUpdate`, `ManaUpdate`, `StaminaUpdate`, `CastStart`,
//! `CastComplete`, `CastFail`, `BuffSnapshot`, `Hit`, `Miss`, `Evade`,
//! `EntityDied`, `EnemySpawn`, `EntityTarget`, `LootBagSpawn`,
//! `LootGranted`, `XpGained`. Other variants get bubbled up via
//! `unhandled_server_message(channel, bytes)` for forward-compat — when
//! their handlers land, add a typed `match` arm in `classify` and a
//! matching emit in `fire`.

// EntitySpawn signal carries 7 identity fields by design; godot-rust's
// `#[godot_api]` proc-macro expands declarations into 8-arg fns (self + args),
// tripping clippy's default 7-arg threshold. The lint fires on the macro
// invocation line, where `#[allow]` on the impl block doesn't reach. Suppress
// crate-wide.
#![allow(clippy::too_many_arguments)]

use bincode::config::standard as bincode_cfg;
use godot::classes::{INode, Node};
use godot::prelude::*;
use protocol::world::{ClientWorldMsg, ServerWorldMsg, Vec3 as WireVec3};
use renet::{ChannelConfig, ConnectionConfig, RenetClient, SendType};
use renet_netcode::{ClientAuthentication, ConnectToken, NetcodeClientTransport};
use std::io::Cursor;
use std::net::UdpSocket;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Channel ids — must mirror the server's `crates/projectdawn-server/src/world/mod.rs`.
const CHANNEL_SYSTEM: u8 = 0;
const CHANNEL_POSITION: u8 = 1;

struct GdextNetExtension;

#[gdextension]
unsafe impl ExtensionLibrary for GdextNetExtension {}

/// `Node`-derived class so it can sit at the root of the `Net` autoload tree.
/// State is owned by the renet client + transport; `was_connected` tracks
/// the connected/disconnected edge so we only emit transport_* signals once
/// per state change.
#[derive(GodotClass)]
#[class(base=Node)]
pub struct NetClient {
    base: Base<Node>,
    client: Option<RenetClient>,
    transport: Option<NetcodeClientTransport>,
    was_connected: bool,
}

#[godot_api]
impl INode for NetClient {
    fn init(base: Base<Node>) -> Self {
        Self {
            base,
            client: None,
            transport: None,
            was_connected: false,
        }
    }
}

#[godot_api]
impl NetClient {
    /// Renet handshake completed; safe to send the app-layer `Connect` message.
    #[signal]
    fn transport_connected();

    /// Renet handshake or session torn down. `reason` is best-effort human text.
    #[signal]
    fn transport_disconnected(reason: GString);

    /// Server accepted the app-layer `Connect`. Carries the local player's
    /// identity so the game can initialize PlayerStats before world entry.
    /// Source of truth is the server's CharacterSpawn (DB-loaded); launcher
    /// doesn't need to relay these.
    #[signal]
    fn connect_ok(
        player_id: i64,
        name: GString,
        race: GString,
        class: GString,
        level: i64,
    );

    /// Server sent a `Kick`. `code` is the `KickCode` variant name.
    #[signal]
    fn kicked(reason: GString, code: GString);

    /// Server position broadcast. `sequence` echoes the last accepted Move seq.
    #[signal]
    fn position(id: i64, pos: Vector3, vel: Vector3, yaw: f32, sequence: i64);

    /// Server announces a new entity in the recipient's AOI (slice 3: same
    /// zone, no spatial filter). Carries identity fields the client needs on
    /// first sight; ongoing Positions stay lean. 7 args — clippy threshold
    /// is 7, godot-rust prepends self → 8; allow at impl-block level.
    #[signal]
    fn entity_spawn(
        id: i64,
        name: GString,
        race: GString,
        class: GString,
        level: i64,
        pos: Vector3,
        yaw: f32,
    );

    /// Server announces an entity left the recipient's AOI (disconnect for
    /// player entities; future: out-of-range, despawn timer, etc.).
    #[signal]
    fn entity_despawn(id: i64);

    /// Track 4 resource bars — relayed from the owning client's broadcast.
    /// The owning client is the authority on the value; the server is a
    /// fan-out relay until Track 6 lifts authority server-side.
    #[signal]
    fn health_update(id: i64, hp: f32, max_hp: f32);

    #[signal]
    fn mana_update(id: i64, mp: f32, max_mp: f32);

    #[signal]
    fn stamina_update(id: i64, stamina: f32, max: f32);

    /// Track 4 sub-task 2 cast bar — relayed from the owning client's
    /// broadcast (no server validation in Track 4). `duration` is the
    /// remaining seconds the receiver should run the bar for; for the
    /// initial broadcast it equals the full cast time, for a late-joiner
    /// seed it equals `total - elapsed`.
    #[signal]
    fn cast_start(caster: i64, spell_name: GString, duration: f32);

    #[signal]
    fn cast_complete(caster: i64, spell_name: GString);

    #[signal]
    fn cast_fail(caster: i64, reason: GString);

    /// Track 4 sub-task 4 combat events. `dmg_type` is the u8
    /// discriminant of `protocol::world::DamageType` (mirror of
    /// NetProtocol.DamageType on the GDScript side).
    #[signal]
    fn hit(attacker: i64, target: i64, amount: i64, crit: bool, dmg_type: i64);

    #[signal]
    fn miss(attacker: i64, target: i64);

    #[signal]
    fn evade(attacker: i64, target: i64);

    /// Track 4 sub-task 5 — fired when a peer dies. Receivers play a
    /// fall-over animation; respawn is implied by the next HealthUpdate
    /// with hp > 0 (no separate Respawn variant by design).
    #[signal]
    fn entity_died(id: i64);

    /// Track 5 sub-task 2 — server announces a server-spawned enemy at
    /// `pos`. `id` is in the reserved enemy-id partition
    /// (`>= ENEMY_ID_BASE`) so the client can disambiguate from player
    /// EntitySpawns by id alone.
    #[signal]
    fn enemy_spawn(
        id: i64,
        mob_name: GString,
        level: i64,
        max_hp: f32,
        hp: f32,
        pos: Vector3,
        yaw: f32,
    );

    /// Track 5 sub-task 2 — server-driven aggro replication. Fired when
    /// an enemy switches target (acquires / drops). `target_id == 0`
    /// encodes `None` (drop / no target); a non-zero value is the
    /// targeted entity's id (player char_id or another enemy id).
    /// Zero is safe because id minting starts at 1 on the server-side
    /// for both partitions.
    #[signal]
    fn entity_target(id: i64, target_id: i64);

    /// Track 5 sub-task 4 — server-owned loot bag landed in the AOI.
    /// `items` is parallel arrays of (path, count) so the FFI stays
    /// flat (PackedStringArray + PackedInt32Array for the count
    /// column). `bag_id` is in the LOOT_BAG_ID_BASE partition so the
    /// client routes ongoing EntityDespawn by id alone.
    #[signal]
    fn loot_bag_spawn(
        bag_id: i64,
        pos: Vector3,
        item_paths: PackedStringArray,
        item_counts: PackedInt32Array,
    );

    /// Track 5 sub-task 4 — private confirmation that the local
    /// player's LootItem / LootAll intent landed and the server has
    /// transferred `count` of `item_path` into our inventory. The
    /// GDScript handler loads the path → ItemData and calls
    /// Inventory.add_item.
    #[signal]
    fn loot_granted(item_path: GString, count: i64);

    /// Track 5 sub-task 5 — private kill-credit XP grant. `current`
    /// and `to_next` are placeholders from the server (Track 5 keeps
    /// player XP state client-authoritative); the GDScript handler
    /// calls PlayerStats.gain_xp(amount) and ignores them.
    #[signal]
    fn xp_gained(amount: i64, current: i64, to_next: i64);

    /// Track 6 sub-task 5 — server forwarded a group invite. Client
    /// shows an accept/reject UI; on accept the GDScript handler
    /// fires `send_group_accept_invite(from_id)`.
    #[signal]
    fn group_invited(from_id: i64, from_name: GString);

    /// Track 6 sub-task 5 — server-authoritative group roster update.
    /// `member_ids` and `member_names` are parallel arrays. Empty
    /// arrays signal the group dissolved (last-member or kicked-self
    /// notification).
    #[signal]
    fn group_roster(
        group_id: i64,
        leader_id: i64,
        member_ids: PackedInt64Array,
        member_names: PackedStringArray,
    );

    /// Track 4 sub-task 3 buff snapshot. `names` and `durations` are
    /// parallel arrays — entry i is one buff. Empty arrays mean "no
    /// active buffs". Receiver should replace any previously-tracked
    /// buff list for `target` with these values.
    #[signal]
    fn buff_snapshot(
        target: i64,
        names: PackedStringArray,
        durations: PackedFloat32Array,
    );

    /// Server-initiated app-layer Heartbeat (informational).
    #[signal]
    fn heartbeat();

    /// Catch-all for ServerWorldMsg variants slice 1 doesn't decode into a
    /// typed signal yet. GDScript can ignore until a future track wires them.
    #[signal]
    fn unhandled_server_message(channel: i64, bytes: PackedByteArray);

    /// Build a renet client from a serialized `ConnectToken` and start the
    /// transport handshake. `world_endpoint` is informational — the actual
    /// server address is signed inside the token, so this param exists for
    /// caller-side logging only and is otherwise unused.
    ///
    /// Returns false on bad token / socket / clock errors. Caller should
    /// listen for `transport_connected` (success) or `transport_disconnected`
    /// (failure) rather than treating `true` as "connected now".
    #[func]
    fn connect_to_server(
        &mut self,
        token_bytes: PackedByteArray,
        world_endpoint: GString,
    ) -> bool {
        let _ = world_endpoint;
        let bytes_vec: Vec<u8> = token_bytes.to_vec();
        let token = match ConnectToken::read(&mut Cursor::new(&bytes_vec[..])) {
            Ok(t) => t,
            Err(e) => {
                godot_error!("[gdext_net] ConnectToken::read failed: {e}");
                return false;
            }
        };
        let socket = match UdpSocket::bind("0.0.0.0:0") {
            Ok(s) => s,
            Err(e) => {
                godot_error!("[gdext_net] UDP bind failed: {e}");
                return false;
            }
        };
        if let Err(e) = socket.set_nonblocking(true) {
            godot_error!("[gdext_net] socket set_nonblocking failed: {e}");
            return false;
        }
        let now = match SystemTime::now().duration_since(UNIX_EPOCH) {
            Ok(d) => d,
            Err(_) => {
                godot_error!("[gdext_net] system clock before UNIX epoch");
                return false;
            }
        };
        let auth = ClientAuthentication::Secure {
            connect_token: token,
        };
        let transport = match NetcodeClientTransport::new(now, auth, socket) {
            Ok(t) => t,
            Err(e) => {
                godot_error!("[gdext_net] NetcodeClientTransport::new failed: {e}");
                return false;
            }
        };
        self.client = Some(RenetClient::new(connection_config_matching_server()));
        self.transport = Some(transport);
        self.was_connected = false;
        true
    }

    /// Tear down the transport and drop the client. Idempotent.
    #[func]
    fn disconnect_now(&mut self) {
        if let Some(t) = self.transport.as_mut() {
            t.disconnect();
        }
        self.client = None;
        self.transport = None;
        self.was_connected = false;
    }

    /// True only when the renet handshake has completed AND the connection
    /// is still live. Named `is_world_connected` so it doesn't shadow the
    /// inherited `Object::is_connected(signal, callable)`.
    #[func]
    fn is_world_connected(&self) -> bool {
        self.client
            .as_ref()
            .map(|c| c.is_connected())
            .unwrap_or(false)
    }

    /// Pump renet for one frame. Call from `_process(delta)` in GDScript.
    #[func]
    fn poll(&mut self, delta: f64) {
        let dt = Duration::from_secs_f64(delta.max(0.0));
        let pending = self.tick_renet(dt);
        self.fire(pending);
    }

    /// Send the app-layer `Connect` on channel 0. Server replies with
    /// `ConnectOk` (→ `connect_ok` signal) or `Kick` (→ `kicked` signal).
    /// `session_token` must be exactly 32 raw bytes — i.e. `bytes.from_hex_string`
    /// of the launcher's hex token, NOT the hex string itself.
    #[func]
    fn send_app_connect(
        &mut self,
        session_token: PackedByteArray,
        char_id: i64,
        client_version: GString,
    ) -> bool {
        let token_vec = session_token.to_vec();
        if token_vec.len() != 32 {
            godot_error!(
                "[gdext_net] session_token must be 32 bytes; got {}",
                token_vec.len()
            );
            return false;
        }
        let mut token = [0u8; 32];
        token.copy_from_slice(&token_vec);
        let msg = ClientWorldMsg::Connect {
            session_token: token,
            char_id: char_id as u64,
            client_version: client_version.to_string(),
        };
        self.send_app(CHANNEL_SYSTEM, &msg)
    }

    #[func]
    fn send_disconnect(&mut self) -> bool {
        self.send_app(CHANNEL_SYSTEM, &ClientWorldMsg::Disconnect)
    }

    /// Track 4 follow-up E: client signals it has left the lobby. Server
    /// gates EntitySpawn / Position fan-out on this so peers don't see
    /// ghost bodies while the local player is at the Enter World screen.
    #[func]
    fn send_enter_world(&mut self) -> bool {
        self.send_app(CHANNEL_SYSTEM, &ClientWorldMsg::EnterWorld)
    }

    #[func]
    fn send_heartbeat(&mut self) -> bool {
        self.send_app(CHANNEL_SYSTEM, &ClientWorldMsg::Heartbeat)
    }

    /// Send a Move intent on the unreliable position channel. Server clamps
    /// `direction` to unit length and applies the speed cap; we just relay it.
    #[func]
    fn send_move(&mut self, sequence: i64, direction: Vector3, jumping: bool) -> bool {
        let msg = ClientWorldMsg::Move {
            sequence: sequence.max(0) as u32,
            direction: WireVec3 {
                x: direction.x,
                y: direction.y,
                z: direction.z,
            },
            jumping,
        };
        self.send_app(CHANNEL_POSITION, &msg)
    }

    // (Track 6 removed `send_resource_update`. Resources are now
    // server-authoritative: HP/MP/Stamina come from the DB at spawn and
    // mutate via server-side regen + combat. The client receives
    // HealthUpdate / ManaUpdate / StaminaUpdate as the source of truth.)

    /// Track 6 — owning client transitions to seated. Server uses this to
    /// scale regen rates server-side; movement auto-stands on either side.
    #[func]
    fn send_sit(&mut self) -> bool {
        self.send_app(CHANNEL_SYSTEM, &ClientWorldMsg::Sit)
    }

    /// Track 6 — owning client stands up. Pair to `send_sit`.
    #[func]
    fn send_stand(&mut self) -> bool {
        self.send_app(CHANNEL_SYSTEM, &ClientWorldMsg::Stand)
    }

    /// Track 6 — owning client respawned (local death-timer elapsed).
    /// Server resets conn.hp/mp/stamina to max and fans HealthUpdate /
    /// ManaUpdate / StaminaUpdate so peer RemotePlayer bars stand back
    /// up. Pair to send_death_broadcast.
    #[func]
    fn send_respawn(&mut self) -> bool {
        self.send_app(CHANNEL_SYSTEM, &ClientWorldMsg::Respawn)
    }

    /// Track 6 sub-task 3 — client tells the server its current total
    /// armor class. Server applies AC/(AC+100) reduction to incoming
    /// damage in tick step 4h.
    #[func]
    fn send_equip_update(&mut self, armor: i64) -> bool {
        let msg = ClientWorldMsg::EquipUpdate { armor: armor as i32 };
        self.send_app(CHANNEL_SYSTEM, &msg)
    }

    /// Track 6 sub-task 3 — dev /pvp toggle. Both attacker and target
    /// must have this on for `combat::can_attack` to permit PvP damage.
    #[func]
    fn send_pvp_toggle(&mut self, on: bool) -> bool {
        let msg = ClientWorldMsg::PvpToggle { on };
        self.send_app(CHANNEL_SYSTEM, &msg)
    }

    /// Track 6 sub-task 3 dev intent — server-side self damage.
    /// Verifies server-driven HP fan-out without waiting on the
    /// CastSpell port. Mirror of send_heal_self.
    #[func]
    fn send_damage_self(&mut self, amount: i64) -> bool {
        let msg = ClientWorldMsg::DamageSelf { amount: amount as i32 };
        self.send_app(CHANNEL_SYSTEM, &msg)
    }

    /// Track 6 sub-task 3 dev intent — server-side self heal.
    /// Closes the heal regression for testing without needing the full
    /// CastSpell port. Will be removed (or gated to GM only) once the
    /// spell table lives server-side.
    #[func]
    fn send_heal_self(&mut self, amount: i64) -> bool {
        let msg = ClientWorldMsg::HealSelf { amount: amount as i32 };
        self.send_app(CHANNEL_SYSTEM, &msg)
    }

    /// Track 4 sub-task 2 — owning client tells the server it started
    /// casting a spell. Server relays as ServerWorldMsg::CastStart to
    /// in_world peers so they can render a cast bar.
    #[func]
    fn send_cast_start_broadcast(&mut self, spell_name: GString, duration: f32) -> bool {
        let msg = ClientWorldMsg::CastStartBroadcast {
            spell_name: spell_name.to_string(),
            duration,
        };
        self.send_app(CHANNEL_SYSTEM, &msg)
    }

    #[func]
    fn send_cast_complete_broadcast(&mut self, spell_name: GString) -> bool {
        let msg = ClientWorldMsg::CastCompleteBroadcast {
            spell_name: spell_name.to_string(),
        };
        self.send_app(CHANNEL_SYSTEM, &msg)
    }

    #[func]
    fn send_cast_fail_broadcast(&mut self, reason: GString) -> bool {
        let msg = ClientWorldMsg::CastFailBroadcast {
            reason: reason.to_string(),
        };
        self.send_app(CHANNEL_SYSTEM, &msg)
    }

    /// Track 4 sub-task 3 — full buff snapshot. `names` and `durations`
    /// must be the same length; extra entries in either are silently
    /// truncated to the shorter. Empty inputs mean "no active buffs".
    /// Track 4 sub-task 4 combat broadcasts. `dmg_type` is the u8
    /// discriminant of protocol::world::DamageType (clamped to the
    /// known range; out-of-range falls back to Physical).
    #[func]
    fn send_hit_broadcast(
        &mut self,
        target: i64,
        amount: i64,
        crit: bool,
        dmg_type: i64,
    ) -> bool {
        let msg = ClientWorldMsg::HitBroadcast {
            target: target as u64,
            amount: amount as i32,
            crit,
            dmg_type: damage_type_from_u8(dmg_type as u8),
        };
        self.send_app(CHANNEL_SYSTEM, &msg)
    }

    #[func]
    fn send_miss_broadcast(&mut self, target: i64) -> bool {
        let msg = ClientWorldMsg::MissBroadcast {
            target: target as u64,
        };
        self.send_app(CHANNEL_SYSTEM, &msg)
    }

    #[func]
    fn send_evade_broadcast(&mut self, target: i64) -> bool {
        let msg = ClientWorldMsg::EvadeBroadcast {
            target: target as u64,
        };
        self.send_app(CHANNEL_SYSTEM, &msg)
    }

    /// Track 4 sub-task 5 — dying client signals HP-zero. Server relays
    /// as ServerWorldMsg::EntityDied to in_world peers.
    #[func]
    fn send_death_broadcast(&mut self) -> bool {
        self.send_app(CHANNEL_SYSTEM, &ClientWorldMsg::DeathBroadcast)
    }

    /// Track 6 sub-task 2 — player → server attack intent against a
    /// specific entity. Carries the equipped weapon's resource path
    /// (empty string for bare-handed) and a main-hand vs offhand flag;
    /// the server runs the damage formula. Server validates target
    /// liveness + range, broadcasts Hit/Miss + HealthUpdate (+
    /// EntityDied on kill).
    #[func]
    fn send_attack(
        &mut self,
        target_id: i64,
        weapon_path: GString,
        is_offhand: bool,
        dmg_type: i64,
    ) -> bool {
        let msg = ClientWorldMsg::Attack {
            target_id: target_id as u64,
            weapon_path: weapon_path.to_string(),
            is_offhand,
            dmg_type: damage_type_from_u8(dmg_type as u8),
        };
        self.send_app(CHANNEL_SYSTEM, &msg)
    }

    /// Track 6 sub-task 5 — group intents. The server's intent sweep
    /// resolves names against the connections map (NOCASE), so the
    /// inviter doesn't need the target's char_id. Accept carries the
    /// inviter's char_id so the server can match against the pending
    /// invite map.
    #[func]
    fn send_group_invite(&mut self, name: GString) -> bool {
        let msg = ClientWorldMsg::GroupInvite { name: name.to_string() };
        self.send_app(CHANNEL_SYSTEM, &msg)
    }

    #[func]
    fn send_group_accept_invite(&mut self, from: i64) -> bool {
        let msg = ClientWorldMsg::GroupAcceptInvite { from: from as u64 };
        self.send_app(CHANNEL_SYSTEM, &msg)
    }

    #[func]
    fn send_group_leave(&mut self) -> bool {
        self.send_app(CHANNEL_SYSTEM, &ClientWorldMsg::GroupLeave)
    }

    #[func]
    fn send_group_kick(&mut self, name: GString) -> bool {
        let msg = ClientWorldMsg::GroupKick { name: name.to_string() };
        self.send_app(CHANNEL_SYSTEM, &msg)
    }

    /// Track 6 sub-task 3b — player → server cast intent. Carries the
    /// canonical spell_name (key into the server's spells.toml) and
    /// the chosen target id. `target_id = 0` encodes "no target" for
    /// SELF / NONE spells; ENEMY / AOE require a non-zero id.
    /// Server validates mana cost / target / range and applies
    /// authoritative damage or heal, fanning HealthUpdate +
    /// ManaUpdate + (for enemy targets) Hit.
    #[func]
    fn send_cast_spell(&mut self, spell_name: GString, target_id: i64) -> bool {
        let target = if target_id == 0 {
            None
        } else {
            Some(target_id as u64)
        };
        let msg = ClientWorldMsg::CastSpell {
            spell_name: spell_name.to_string(),
            target_id: target,
        };
        self.send_app(CHANNEL_SYSTEM, &msg)
    }

    /// Track 5 sub-task 4 — player → server intent to claim one slot
    /// of a server-owned loot bag. Server validates range + slot
    /// bounds, sends LootGranted privately on success and re-broadcasts
    /// the bag's snapshot to all in_world peers.
    #[func]
    fn send_loot_item(&mut self, bag_id: i64, slot: i64) -> bool {
        let msg = ClientWorldMsg::LootItem {
            bag_id: bag_id as u64,
            slot: slot as u32,
        };
        self.send_app(CHANNEL_SYSTEM, &msg)
    }

    /// "Take everything in this bag" intent. Acts like one LootItem
    /// per remaining slot. Bag despawns when emptied.
    #[func]
    fn send_loot_all(&mut self, bag_id: i64) -> bool {
        let msg = ClientWorldMsg::LootAll {
            bag_id: bag_id as u64,
        };
        self.send_app(CHANNEL_SYSTEM, &msg)
    }

    #[func]
    fn send_buff_snapshot_broadcast(
        &mut self,
        names: PackedStringArray,
        durations: PackedFloat32Array,
    ) -> bool {
        let names_vec: Vec<GString> = names.to_vec();
        let durations_vec: Vec<f32> = durations.to_vec();
        let n = names_vec.len().min(durations_vec.len());
        let mut buffs: Vec<(String, f32)> = Vec::with_capacity(n);
        for i in 0..n {
            buffs.push((names_vec[i].to_string(), durations_vec[i]));
        }
        let msg = ClientWorldMsg::BuffSnapshotBroadcast { buffs };
        self.send_app(CHANNEL_SYSTEM, &msg)
    }
}

/// Pending side-effects from a single `tick_renet` call. We collect these
/// inside the `&mut self.client` borrow and emit them after, so signal
/// handlers can re-enter our own methods without violating borrow rules.
#[derive(Default)]
struct Pending {
    transport_connected: bool,
    transport_disconnected: Option<String>,
    incoming: Vec<Incoming>,
}

enum Incoming {
    ConnectOk {
        player_id: i64,
        name: String,
        race: String,
        class: String,
        level: u32,
    },
    Heartbeat,
    Kick {
        reason: String,
        code: String,
    },
    Position {
        id: i64,
        pos: WireVec3,
        vel: WireVec3,
        yaw: f32,
        sequence: u32,
    },
    EntitySpawn {
        id: i64,
        name: String,
        race: String,
        class: String,
        level: u32,
        pos: WireVec3,
        yaw: f32,
    },
    EntityDespawn {
        id: i64,
    },
    HealthUpdate {
        id: i64,
        hp: f32,
        max_hp: f32,
    },
    ManaUpdate {
        id: i64,
        mp: f32,
        max_mp: f32,
    },
    StaminaUpdate {
        id: i64,
        stamina: f32,
        max: f32,
    },
    CastStart {
        caster: i64,
        spell_name: String,
        duration: f32,
    },
    CastComplete {
        caster: i64,
        spell_name: String,
    },
    CastFail {
        caster: i64,
        reason: String,
    },
    BuffSnapshot {
        target: i64,
        buffs: Vec<(String, f32)>,
    },
    Hit {
        attacker: i64,
        target: i64,
        amount: i32,
        crit: bool,
        dmg_type: u8,
    },
    Miss {
        attacker: i64,
        target: i64,
    },
    Evade {
        attacker: i64,
        target: i64,
    },
    EntityDied {
        id: i64,
    },
    EnemySpawn {
        id: i64,
        mob_name: String,
        level: u32,
        max_hp: f32,
        hp: f32,
        pos: WireVec3,
        yaw: f32,
    },
    EntityTarget {
        id: i64,
        target: Option<i64>,
    },
    LootBagSpawn {
        bag_id: i64,
        pos: WireVec3,
        items: Vec<(String, u32)>,
    },
    LootGranted {
        item_path: String,
        count: u32,
    },
    XpGained {
        amount: i32,
        current: i32,
        to_next: i32,
    },
    GroupInvited {
        from_id: i64,
        from_name: String,
    },
    GroupRoster {
        group_id: i64,
        leader_id: i64,
        member_ids: Vec<i64>,
        member_names: Vec<String>,
    },
    Raw {
        channel: u8,
        bytes: Vec<u8>,
    },
}

impl NetClient {
    fn send_app(&mut self, channel: u8, msg: &ClientWorldMsg) -> bool {
        let Some(client) = self.client.as_mut() else {
            return false;
        };
        if !client.is_connected() {
            return false;
        }
        let bytes = match bincode::serde::encode_to_vec(msg, bincode_cfg()) {
            Ok(b) => b,
            Err(e) => {
                godot_error!("[gdext_net] encode ClientWorldMsg: {e}");
                return false;
            }
        };
        client.send_message(channel, bytes);
        true
    }

    fn tick_renet(&mut self, dt: Duration) -> Pending {
        let mut p = Pending::default();
        let (Some(client), Some(transport)) =
            (self.client.as_mut(), self.transport.as_mut())
        else {
            return p;
        };

        client.update(dt);
        if let Err(e) = transport.update(dt, client) {
            p.transport_disconnected = Some(format!("transport: {e}"));
        }

        let now_connected = client.is_connected();
        if now_connected && !self.was_connected {
            p.transport_connected = true;
        } else if !now_connected
            && self.was_connected
            && p.transport_disconnected.is_none()
        {
            // Server-initiated drop or timeout: lift renet's reason if any.
            let r = client
                .disconnect_reason()
                .map(|r| format!("{r:?}"))
                .unwrap_or_else(|| "unknown".to_string());
            p.transport_disconnected = Some(r);
        }
        self.was_connected = now_connected;

        for &channel in &[CHANNEL_SYSTEM, CHANNEL_POSITION] {
            while let Some(bytes) = client.receive_message(channel) {
                let raw = bytes.to_vec();
                match decode_server(&raw) {
                    Some(msg) => p.incoming.push(classify(channel, msg, &raw)),
                    None => p.incoming.push(Incoming::Raw {
                        channel,
                        bytes: raw,
                    }),
                }
            }
        }

        if let Err(e) = transport.send_packets(client) {
            godot_error!("[gdext_net] send_packets: {e}");
        }
        p
    }

    fn fire(&mut self, p: Pending) {
        if p.transport_connected {
            self.base_mut()
                .emit_signal("transport_connected", &[]);
        }
        if let Some(reason) = p.transport_disconnected {
            // Drop renet state so the caller can re-`connect_to_server` cleanly.
            self.client = None;
            self.transport = None;
            self.was_connected = false;
            let var = GString::from(reason.as_str()).to_variant();
            self.base_mut()
                .emit_signal("transport_disconnected", &[var]);
        }
        for ev in p.incoming {
            match ev {
                Incoming::ConnectOk {
                    player_id,
                    name,
                    race,
                    class,
                    level,
                } => {
                    self.base_mut().emit_signal(
                        "connect_ok",
                        &[
                            player_id.to_variant(),
                            GString::from(name.as_str()).to_variant(),
                            GString::from(race.as_str()).to_variant(),
                            GString::from(class.as_str()).to_variant(),
                            (level as i64).to_variant(),
                        ],
                    );
                }
                Incoming::Heartbeat => {
                    self.base_mut().emit_signal("heartbeat", &[]);
                }
                Incoming::Kick { reason, code } => {
                    self.base_mut().emit_signal(
                        "kicked",
                        &[
                            GString::from(reason.as_str()).to_variant(),
                            GString::from(code.as_str()).to_variant(),
                        ],
                    );
                }
                Incoming::Position {
                    id,
                    pos,
                    vel,
                    yaw,
                    sequence,
                } => {
                    self.base_mut().emit_signal(
                        "position",
                        &[
                            id.to_variant(),
                            Vector3::new(pos.x, pos.y, pos.z).to_variant(),
                            Vector3::new(vel.x, vel.y, vel.z).to_variant(),
                            yaw.to_variant(),
                            (sequence as i64).to_variant(),
                        ],
                    );
                }
                Incoming::EntitySpawn {
                    id,
                    name,
                    race,
                    class,
                    level,
                    pos,
                    yaw,
                } => {
                    self.base_mut().emit_signal(
                        "entity_spawn",
                        &[
                            id.to_variant(),
                            GString::from(name.as_str()).to_variant(),
                            GString::from(race.as_str()).to_variant(),
                            GString::from(class.as_str()).to_variant(),
                            (level as i64).to_variant(),
                            Vector3::new(pos.x, pos.y, pos.z).to_variant(),
                            yaw.to_variant(),
                        ],
                    );
                }
                Incoming::EntityDespawn { id } => {
                    self.base_mut()
                        .emit_signal("entity_despawn", &[id.to_variant()]);
                }
                Incoming::HealthUpdate { id, hp, max_hp } => {
                    self.base_mut().emit_signal(
                        "health_update",
                        &[
                            id.to_variant(),
                            hp.to_variant(),
                            max_hp.to_variant(),
                        ],
                    );
                }
                Incoming::ManaUpdate { id, mp, max_mp } => {
                    self.base_mut().emit_signal(
                        "mana_update",
                        &[
                            id.to_variant(),
                            mp.to_variant(),
                            max_mp.to_variant(),
                        ],
                    );
                }
                Incoming::StaminaUpdate { id, stamina, max } => {
                    self.base_mut().emit_signal(
                        "stamina_update",
                        &[
                            id.to_variant(),
                            stamina.to_variant(),
                            max.to_variant(),
                        ],
                    );
                }
                Incoming::CastStart {
                    caster,
                    spell_name,
                    duration,
                } => {
                    self.base_mut().emit_signal(
                        "cast_start",
                        &[
                            caster.to_variant(),
                            GString::from(spell_name.as_str()).to_variant(),
                            duration.to_variant(),
                        ],
                    );
                }
                Incoming::CastComplete {
                    caster,
                    spell_name,
                } => {
                    self.base_mut().emit_signal(
                        "cast_complete",
                        &[
                            caster.to_variant(),
                            GString::from(spell_name.as_str()).to_variant(),
                        ],
                    );
                }
                Incoming::CastFail { caster, reason } => {
                    self.base_mut().emit_signal(
                        "cast_fail",
                        &[
                            caster.to_variant(),
                            GString::from(reason.as_str()).to_variant(),
                        ],
                    );
                }
                Incoming::Hit {
                    attacker,
                    target,
                    amount,
                    crit,
                    dmg_type,
                } => {
                    self.base_mut().emit_signal(
                        "hit",
                        &[
                            attacker.to_variant(),
                            target.to_variant(),
                            (amount as i64).to_variant(),
                            crit.to_variant(),
                            (dmg_type as i64).to_variant(),
                        ],
                    );
                }
                Incoming::Miss { attacker, target } => {
                    self.base_mut().emit_signal(
                        "miss",
                        &[attacker.to_variant(), target.to_variant()],
                    );
                }
                Incoming::Evade { attacker, target } => {
                    self.base_mut().emit_signal(
                        "evade",
                        &[attacker.to_variant(), target.to_variant()],
                    );
                }
                Incoming::EntityDied { id } => {
                    self.base_mut()
                        .emit_signal("entity_died", &[id.to_variant()]);
                }
                Incoming::BuffSnapshot { target, buffs } => {
                    let mut names = PackedStringArray::new();
                    let mut durations = PackedFloat32Array::new();
                    for (n, d) in &buffs {
                        names.push(&GString::from(n.as_str()));
                        durations.push(*d);
                    }
                    self.base_mut().emit_signal(
                        "buff_snapshot",
                        &[
                            target.to_variant(),
                            names.to_variant(),
                            durations.to_variant(),
                        ],
                    );
                }
                Incoming::EnemySpawn {
                    id,
                    mob_name,
                    level,
                    max_hp,
                    hp,
                    pos,
                    yaw,
                } => {
                    self.base_mut().emit_signal(
                        "enemy_spawn",
                        &[
                            id.to_variant(),
                            GString::from(mob_name.as_str()).to_variant(),
                            (level as i64).to_variant(),
                            max_hp.to_variant(),
                            hp.to_variant(),
                            Vector3::new(pos.x, pos.y, pos.z).to_variant(),
                            yaw.to_variant(),
                        ],
                    );
                }
                Incoming::EntityTarget { id, target } => {
                    // `target == None` encodes as 0 over the wire (see
                    // signal docs). Mint a non-collision sentinel because
                    // GDScript Variant doesn't carry an Option type.
                    let target_id = target.unwrap_or(0);
                    self.base_mut().emit_signal(
                        "entity_target",
                        &[id.to_variant(), target_id.to_variant()],
                    );
                }
                Incoming::LootBagSpawn { bag_id, pos, items } => {
                    let mut paths = PackedStringArray::new();
                    let mut counts = PackedInt32Array::new();
                    for (path, count) in &items {
                        paths.push(&GString::from(path.as_str()));
                        counts.push(*count as i32);
                    }
                    self.base_mut().emit_signal(
                        "loot_bag_spawn",
                        &[
                            bag_id.to_variant(),
                            Vector3::new(pos.x, pos.y, pos.z).to_variant(),
                            paths.to_variant(),
                            counts.to_variant(),
                        ],
                    );
                }
                Incoming::LootGranted { item_path, count } => {
                    self.base_mut().emit_signal(
                        "loot_granted",
                        &[
                            GString::from(item_path.as_str()).to_variant(),
                            (count as i64).to_variant(),
                        ],
                    );
                }
                Incoming::XpGained { amount, current, to_next } => {
                    self.base_mut().emit_signal(
                        "xp_gained",
                        &[
                            (amount as i64).to_variant(),
                            (current as i64).to_variant(),
                            (to_next as i64).to_variant(),
                        ],
                    );
                }
                Incoming::GroupInvited { from_id, from_name } => {
                    self.base_mut().emit_signal(
                        "group_invited",
                        &[
                            from_id.to_variant(),
                            GString::from(from_name.as_str()).to_variant(),
                        ],
                    );
                }
                Incoming::GroupRoster {
                    group_id,
                    leader_id,
                    member_ids,
                    member_names,
                } => {
                    let mut ids_arr = PackedInt64Array::new();
                    ids_arr.resize(member_ids.len());
                    for (i, v) in member_ids.iter().enumerate() {
                        ids_arr[i] = *v;
                    }
                    let mut names_arr = PackedStringArray::new();
                    names_arr.resize(member_names.len());
                    for (i, n) in member_names.iter().enumerate() {
                        names_arr[i] = GString::from(n.as_str());
                    }
                    self.base_mut().emit_signal(
                        "group_roster",
                        &[
                            group_id.to_variant(),
                            leader_id.to_variant(),
                            ids_arr.to_variant(),
                            names_arr.to_variant(),
                        ],
                    );
                }
                Incoming::Raw { channel, bytes } => {
                    let pba = packed_byte_array_from(&bytes);
                    self.base_mut().emit_signal(
                        "unhandled_server_message",
                        &[(channel as i64).to_variant(), pba.to_variant()],
                    );
                }
            }
        }
    }
}

fn classify(channel: u8, msg: ServerWorldMsg, raw: &[u8]) -> Incoming {
    match msg {
        ServerWorldMsg::ConnectOk {
            player_id,
            name,
            race,
            class,
            level,
        } => Incoming::ConnectOk {
            player_id: player_id as i64,
            name,
            race,
            class,
            level,
        },
        ServerWorldMsg::Heartbeat => Incoming::Heartbeat,
        ServerWorldMsg::Kick { reason, code, .. } => Incoming::Kick {
            reason,
            code: format!("{code:?}"),
        },
        ServerWorldMsg::Position {
            id,
            pos,
            vel,
            yaw,
            sequence,
        } => Incoming::Position {
            id: id as i64,
            pos,
            vel,
            yaw,
            sequence,
        },
        ServerWorldMsg::EntitySpawn {
            id,
            name,
            race,
            class,
            level,
            pos,
            yaw,
        } => Incoming::EntitySpawn {
            id: id as i64,
            name,
            race,
            class,
            level,
            pos,
            yaw,
        },
        ServerWorldMsg::EntityDespawn { id } => Incoming::EntityDespawn { id: id as i64 },
        ServerWorldMsg::HealthUpdate { id, hp, max_hp } => Incoming::HealthUpdate {
            id: id as i64,
            hp,
            max_hp,
        },
        ServerWorldMsg::ManaUpdate { id, mp, max_mp } => Incoming::ManaUpdate {
            id: id as i64,
            mp,
            max_mp,
        },
        ServerWorldMsg::StaminaUpdate { id, stamina, max } => Incoming::StaminaUpdate {
            id: id as i64,
            stamina,
            max,
        },
        ServerWorldMsg::CastStart {
            caster,
            spell_name,
            duration,
        } => Incoming::CastStart {
            caster: caster as i64,
            spell_name,
            duration,
        },
        ServerWorldMsg::CastComplete {
            caster,
            spell_name,
        } => Incoming::CastComplete {
            caster: caster as i64,
            spell_name,
        },
        ServerWorldMsg::CastFail { caster, reason } => Incoming::CastFail {
            caster: caster as i64,
            reason,
        },
        ServerWorldMsg::BuffSnapshot { target, buffs } => Incoming::BuffSnapshot {
            target: target as i64,
            buffs,
        },
        ServerWorldMsg::Hit {
            attacker,
            target,
            amount,
            crit,
            dmg_type,
        } => Incoming::Hit {
            attacker: attacker as i64,
            target: target as i64,
            amount,
            crit,
            dmg_type: damage_type_to_u8(dmg_type),
        },
        ServerWorldMsg::Miss { attacker, target } => Incoming::Miss {
            attacker: attacker as i64,
            target: target as i64,
        },
        ServerWorldMsg::Evade { attacker, target } => Incoming::Evade {
            attacker: attacker as i64,
            target: target as i64,
        },
        ServerWorldMsg::EntityDied { id } => Incoming::EntityDied { id: id as i64 },
        ServerWorldMsg::EnemySpawn {
            id,
            mob_name,
            level,
            max_hp,
            hp,
            pos,
            yaw,
        } => Incoming::EnemySpawn {
            id: id as i64,
            mob_name,
            level,
            max_hp,
            hp,
            pos,
            yaw,
        },
        ServerWorldMsg::EntityTarget { id, target } => Incoming::EntityTarget {
            id: id as i64,
            target: target.map(|t| t as i64),
        },
        ServerWorldMsg::LootBagSpawn { bag_id, pos, items } => Incoming::LootBagSpawn {
            bag_id: bag_id as i64,
            pos,
            items,
        },
        ServerWorldMsg::LootGranted { item_path, count } => Incoming::LootGranted {
            item_path,
            count,
        },
        ServerWorldMsg::XpGained { amount, current, to_next } => Incoming::XpGained {
            amount,
            current,
            to_next,
        },
        ServerWorldMsg::GroupInvited { from_id, from_name } => Incoming::GroupInvited {
            from_id: from_id as i64,
            from_name,
        },
        ServerWorldMsg::GroupRoster { group_id, leader_id, members } => {
            let mut member_ids = Vec::with_capacity(members.len());
            let mut member_names = Vec::with_capacity(members.len());
            for (id, name) in members {
                member_ids.push(id as i64);
                member_names.push(name);
            }
            Incoming::GroupRoster {
                group_id: group_id as i64,
                leader_id: leader_id as i64,
                member_ids,
                member_names,
            }
        }
        // Other variants (BuffApplied, ChatMessage, ...) get
        // bubbled up raw. As their handlers land, add typed `match` arms here.
        _ => Incoming::Raw {
            channel,
            bytes: raw.to_vec(),
        },
    }
}

/// DamageType u8-discriminant conversion. Kept inside this crate so the
/// wire enum can grow without touching call sites. Unknown values fall
/// back to Physical — same direction the existing GDScript mirror
/// (NetProtocol.DamageType) assumes.
fn damage_type_to_u8(t: protocol::world::DamageType) -> u8 {
    use protocol::world::DamageType as DT;
    match t {
        DT::Physical => 0,
        DT::Fire => 1,
        DT::Ice => 2,
        DT::Lightning => 3,
        DT::Arcane => 4,
        DT::Holy => 5,
        DT::Nature => 6,
        DT::Spirit => 7,
        DT::Shadow => 8,
        DT::Poison => 9,
    }
}

fn damage_type_from_u8(n: u8) -> protocol::world::DamageType {
    use protocol::world::DamageType as DT;
    match n {
        1 => DT::Fire,
        2 => DT::Ice,
        3 => DT::Lightning,
        4 => DT::Arcane,
        5 => DT::Holy,
        6 => DT::Nature,
        7 => DT::Spirit,
        8 => DT::Shadow,
        9 => DT::Poison,
        _ => DT::Physical,
    }
}

fn decode_server(bytes: &[u8]) -> Option<ServerWorldMsg> {
    bincode::serde::decode_from_slice::<ServerWorldMsg, _>(bytes, bincode_cfg())
        .ok()
        .map(|(m, _)| m)
}

fn packed_byte_array_from(bytes: &[u8]) -> PackedByteArray {
    bytes.iter().copied().collect()
}

/// Channel layout matching `crates/projectdawn-server/src/world/mod.rs`. Any
/// drift in `available_bytes_per_tick`, `max_memory_usage_bytes`, or the
/// `SendType` per channel will silently fail the renet handshake.
fn connection_config_matching_server() -> ConnectionConfig {
    let chans = vec![
        ChannelConfig {
            channel_id: CHANNEL_SYSTEM,
            max_memory_usage_bytes: 64 * 1024,
            send_type: SendType::ReliableOrdered {
                resend_time: Duration::from_millis(150),
            },
        },
        ChannelConfig {
            channel_id: CHANNEL_POSITION,
            max_memory_usage_bytes: 32 * 1024,
            send_type: SendType::Unreliable,
        },
    ];
    ConnectionConfig {
        available_bytes_per_tick: 60_000,
        server_channels_config: chans.clone(),
        client_channels_config: chans,
    }
}
