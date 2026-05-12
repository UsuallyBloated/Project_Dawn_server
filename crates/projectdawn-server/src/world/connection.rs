//! Per-connection state held by the tick loop. One entry per connected
//! `client_id` (which equals the renet `ConnectToken.client_id` we minted,
//! which equals the player's `char_id`).

use std::time::Instant;

#[derive(Debug, Clone, Copy)]
pub struct Vec3f {
    pub x: f32,
    pub y: f32,
    pub z: f32,
}

impl Vec3f {
    pub const ZERO: Self = Self { x: 0.0, y: 0.0, z: 0.0 };

    pub fn from_tuple(t: (f32, f32, f32)) -> Self {
        Self { x: t.0, y: t.1, z: t.2 }
    }

    pub fn into_tuple(self) -> (f32, f32, f32) {
        (self.x, self.y, self.z)
    }

    pub fn length(self) -> f32 {
        (self.x * self.x + self.y * self.y + self.z * self.z).sqrt()
    }

    /// Clamp length to `max`. Used to enforce the server-side speed cap on
    /// move intents — anything longer becomes a unit-vector × max.
    pub fn clamp_length(self, max: f32) -> Self {
        let len = self.length();
        if len > max && len > 0.0 {
            let s = max / len;
            Self { x: self.x * s, y: self.y * s, z: self.z * s }
        } else {
            self
        }
    }
}

#[derive(Debug)]
#[allow(dead_code)] // account_id/name/level land in audit + chat + stat scaling next slice
pub struct PerConnection {
    pub char_id: i64,
    pub account_id: i64,
    pub name: String,
    pub race: String,
    pub class: String,
    pub level: i32,
    pub zone: Option<String>,
    pub pos: Vec3f,
    pub yaw: f32,
    /// Last application-layer message arrival. Used for our 10 s app-level
    /// heartbeat timeout; renet's transport has its own.
    pub last_packet: Instant,
    /// Tracks last persisted snapshot so we can skip checkpoint writes
    /// when nothing has moved.
    pub last_persisted_pos: Vec3f,
    pub last_persisted_yaw: f32,
    /// Set after the application-layer Connect handshake completes. Until
    /// then we won't broadcast positions to this client.
    pub ready: bool,
    /// Set after the client sends `EnterWorld` (i.e. left the lobby). Gates
    /// EntitySpawn fan-out so peers don't see ghost bodies for clients
    /// still on the Enter World screen.
    pub in_world: bool,
    /// Highest move sequence we've accepted from this client. Out-of-order
    /// packets get dropped (unreliable channel, so reorder is expected).
    pub last_move_seq: u32,
    /// Most recent movement intent (unit vector, or zero for "stop").
    /// Updated when a Move message is accepted; integrated once per
    /// tick by the tick loop, NOT per-message. Per-message integration
    /// would 3× speed under typical client send rates.
    pub latest_direction: Vec3f,
    /// Wall-clock time of the most recent accepted Move. The tick loop
    /// stops integrating `latest_direction` once this gets older than
    /// [`super::STALE_MOVE_THRESHOLD`] so a crashed client doesn't keep
    /// visually moving until heartbeat timeout.
    pub last_move_received: Option<Instant>,

    /// Last resources broadcast by this client (Track 4). Cached so newly
    /// joining peers can be sent the current resources in step 4a alongside
    /// EntitySpawn. `resource_state_set` flips true on first
    /// `ResourceUpdate` — until then we have nothing to forward.
    pub last_hp: f32,
    pub last_max_hp: f32,
    pub last_mp: f32,
    pub last_max_mp: f32,
    pub last_stamina: f32,
    pub last_max_stamina: f32,
    pub resource_state_set: bool,
}

impl PerConnection {
    pub fn from_spawn(spawn: crate::db::CharacterSpawn, now: Instant) -> Self {
        let pos = Vec3f::from_tuple(spawn.pos);
        Self {
            char_id: spawn.char_id,
            account_id: spawn.account_id,
            name: spawn.name,
            race: spawn.race,
            class: spawn.class,
            level: spawn.level,
            zone: spawn.zone,
            pos,
            yaw: spawn.yaw,
            last_packet: now,
            last_persisted_pos: pos,
            last_persisted_yaw: spawn.yaw,
            ready: false,
            in_world: false,
            last_move_seq: 0,
            latest_direction: Vec3f::ZERO,
            last_move_received: None,
            last_hp: 0.0,
            last_max_hp: 0.0,
            last_mp: 0.0,
            last_max_mp: 0.0,
            last_stamina: 0.0,
            last_max_stamina: 0.0,
            resource_state_set: false,
        }
    }

    /// True if the connection has gone silent for at least
    /// [`super::HEARTBEAT_TIMEOUT`] of wall-clock time.
    pub fn is_app_idle(&self, now: Instant) -> bool {
        now.duration_since(self.last_packet) >= super::HEARTBEAT_TIMEOUT
    }

    pub fn touch(&mut self, now: Instant) {
        self.last_packet = now;
    }

    /// Has the in-memory position/yaw drifted from the last persisted
    /// snapshot? Cheap exact-equality check — the 60s checkpoint cadence
    /// makes float-equality drift a non-issue (we'll write again next pass).
    pub fn is_dirty_for_persist(&self) -> bool {
        let p = self.pos;
        let lp = self.last_persisted_pos;
        p.x != lp.x || p.y != lp.y || p.z != lp.z || self.yaw != self.last_persisted_yaw
    }

    pub fn mark_persisted(&mut self) {
        self.last_persisted_pos = self.pos;
        self.last_persisted_yaw = self.yaw;
    }
}
