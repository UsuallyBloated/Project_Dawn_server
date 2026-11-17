//! Server-authoritative enemy entity. Owns its position, HP, AI state,
//! and aggro table. The tick loop holds these in
//! `HashMap<EntityId, Entity>` keyed by the server-minted enemy id
//! (partition starts at `protocol::world::ENEMY_ID_BASE`).
//!
//! The state machine mirrors the GDScript `enemy.gd` semantics —
//! Idle / Chase / Attack / Leash / Dead, plus the timers that gate
//! transitions — but with no Godot dependency. Client-side `enemy.gd`
//! remains the legacy single-player Test Room implementation; the
//! launcher-mode client will be a render-only consumer (sub-task 2).

use super::buffs::{ActiveBuff, BuffEffect};
use super::obstacles::{BODY_RADIUS, P2};
use super::roam::{self, Roam};
use super::roam_map::RoamMap;
use super::{connection::Vec3f, zones::MobTemplate};
use protocol::world::{EntityId, ENEMY_ID_BASE, PET_ID_BASE};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

/// Below this much movement since the last Position send, a mob counts as
/// standing still: `step_toward`'s epsilon snap can nudge a stationary entity
/// by less, and a float compare would call that "moved" every tick.
const MOVED_EPSILON: f32 = 0.005;
/// Below this much turn since the last send, a mob counts as not having
/// turned. `face_toward` recomputes yaw from the same atan2 while its target
/// stands still, so this only has to absorb float noise.
const YAW_EPSILON: f32 = 0.001;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CcKind {
    Mez,
    Root,
    Snare {
        factor_pct: u8,
    }, // 0–100; 50 = half speed
    AttackSlow {
        factor_pct: u8,
    }, // extra delay fraction × 100
    /// Shield Bash (active skills, 2026-10-19): frozen like mez, but damage
    /// does not end it.
    Stun,
}

#[derive(Debug, Clone)]
pub struct ActiveCc {
    pub kind: CcKind,
    pub remaining: f32,
}

/// A damage-over-time spell on a mob or pet (spell batch step 5). Damage is
/// dealt in whole ticks of `dps x DOT_TICK_SECS`, the first one
/// `DOT_TICK_SECS` after it lands (the direct hit is the cast's own).
#[derive(Debug, Clone)]
pub struct ActiveDot {
    /// The spell's name: one DoT per name on a target, the newer replacing
    /// the older, so re-casting refreshes rather than stacks.
    pub name: String,
    /// Who gets the aggro, the threat and the kill credit.
    pub caster: EntityId,
    pub dps: f32,
    pub dmg_type: protocol::world::DamageType,
    pub remaining: f32,
    /// Seconds accumulated towards the next tick.
    pub tick_acc: f32,
    /// Whole ticks still to deal, set by `apply_dot` from the duration.
    /// The DoT lives until they are dealt, so float drift between
    /// `remaining` and `tick_acc` can never lose the last one.
    pub ticks_left: u32,
}

/// One DoT tick the AI sweep dealt, for the tick loop to fan and to credit.
#[derive(Debug, Clone)]
pub struct DotTick {
    pub name: String,
    pub caster: EntityId,
    pub amount: i32,
    pub dmg_type: protocol::world::DamageType,
}

impl ActiveCc {
    pub fn new_mez(duration: f32) -> Self {
        Self {
            kind: CcKind::Mez,
            remaining: duration,
        }
    }
    pub fn new_root(duration: f32) -> Self {
        Self {
            kind: CcKind::Root,
            remaining: duration,
        }
    }
    pub fn new_stun(duration: f32) -> Self {
        Self {
            kind: CcKind::Stun,
            remaining: duration,
        }
    }
    pub fn new_snare(slow_amount: f32, duration: f32) -> Self {
        let factor_pct = (slow_amount.clamp(0.0, 1.0) * 100.0) as u8;
        Self {
            kind: CcKind::Snare { factor_pct },
            remaining: duration,
        }
    }
    pub fn new_attack_slow(slow_amount: f32, duration: f32) -> Self {
        let factor_pct = (slow_amount.clamp(0.0, 1.0) * 100.0) as u8;
        Self {
            kind: CcKind::AttackSlow { factor_pct },
            remaining: duration,
        }
    }
}

/// Track 15.3 — pet stance. Only meaningful for pets (`owner.is_some()`);
/// non-pet enemies leave this at `Follow` (the default) and the
/// non-pet AI never reads it. `Follow` is legacy follow-the-owner-
/// and-inherit-target behaviour. `Guard` parks the pet at its current
/// position (no following, but still chases commanded / inherited
/// targets if attacked; the inheritance pre-pass in `tick/enemy_ai.rs` skips
/// auto-inheritance from owner attacks). `Sit` is full passive — pet
/// drops any current target, doesn't follow, doesn't aggro on
/// incoming damage.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PetStance {
    #[default]
    Follow,
    Guard,
    Sit,
}

/// State machine for an `Entity`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EnemyState {
    /// Standing at spawn; scanning for aggro targets within `aggro_range`.
    Idle,
    /// Locked target; moving toward it. Transitions to `Attack` when in
    /// melee range, `Leash` when target leaves `leash_range`.
    Chase,
    /// In melee range of target; firing on `attack_interval`. Returns
    /// to `Chase` if target steps out.
    Attack,
    /// Returning to spawn after losing aggro. Heals on arrival.
    Leash,
    /// HP hit zero. Holds at the death position until the corpse linger
    /// expires, then `EntityDespawn` is broadcast and the entity is
    /// removed from the world map.
    Dead,
}

#[derive(Debug)]
pub struct Entity {
    pub id: EntityId,
    /// Index into the world's `spawn_points` vec. Used so the spawn
    /// point can be notified on death and start its respawn timer.
    pub spawn_point_idx: usize,
    /// Authored archetype data (display name, base stats, speeds). Cloned
    /// from the spawn point on instantiation so resists / overrides are
    /// per-entity if a future zone needs them.
    pub mob: MobTemplate,
    /// Spawn position (canonical, no jitter applied) — leash target.
    pub spawn_pos: Vec3f,
    pub pos: Vec3f,
    pub yaw: f32,
    pub hp: f32,
    pub max_hp: f32,

    pub state: EnemyState,
    /// Current aggro target. `None` outside Chase/Attack.
    pub target: Option<EntityId>,
    /// Aggro / credit table — accumulated damage per attacker, used
    /// for kill-credit XP routing on death. Pets contribute under
    /// their OWNER's id so XP routes to the owner naturally.
    pub aggro: HashMap<EntityId, f32>,
    /// Track 12 Piece A2 — per-actual-attacker threat table. Pets
    /// contribute under their own id here (not the owner's). Used
    /// only by the AI's `maybe_switch_target_by_threat` re-eval so
    /// the enemy can pull onto a pet that out-damages its current
    /// target by >= THREAT_SWITCH_MULT.
    pub threat: HashMap<EntityId, f32>,

    /// Time of last melee swing. Compared against
    /// `mob.attack_interval` to gate attack firing.
    pub last_attack_at: Option<Instant>,
    /// Time the entity entered its current state. Gates the corpse-
    /// linger window before EntityDespawn fires.
    pub state_entered_at: Instant,

    /// Monotonic sequence for Position broadcasts. Same role as
    /// `PerConnection.last_move_seq` — the client uses it to drop
    /// out-of-order updates on the unreliable channel.
    pub seq: u32,
    /// What the last Position fan-out carried, and when. The tick's
    /// `position_broadcast_due` compares against these so a mob standing
    /// still fans only the `ENEMY_POSITION_KEEPALIVE`, not every tick.
    /// `None` = never sent (the first tick after spawn always sends).
    pub last_bcast_pos: Vec3f,
    pub last_bcast_yaw: f32,
    pub last_bcast_at: Option<Instant>,

    /// Active crowd-control effects. Ticked every AI frame; empty is the
    /// common case (no per-tick allocation cost when idle).
    pub active_cc: Vec<ActiveCc>,
    /// Damage over time on this mob or pet (spell batch step 5): one entry
    /// per spell name, the newer cast replacing the older. Ticks every
    /// `DOT_TICK_SECS` in the AI sweep, builds aggro and threat for its
    /// caster, breaks mez, and kills with the same rewards as a direct hit.
    /// Cleared the moment the mob starts leashing home, and when its caster
    /// dies, ports or logs out, so nothing can be tagged and left to die
    /// from safety.
    pub active_dots: Vec<ActiveDot>,
    /// The status names (buffs, DoTs, CC) last fanned as this entity's
    /// BuffSnapshot; the AI sweep re-fans when the set changes, so the
    /// target frame shows what is on a mob (spell batch step 5).
    pub last_status_fanned: Vec<String>,

    /// Track 11 — player-owned pet. `None` for world-spawned enemies
    /// (the default); `Some(owner_char_id)` for pets summoned via
    /// PET_SUMMON. Owner determines despawn-on-disconnect and (later)
    /// follow-and-attack AI; identity also drives id partition
    /// (>= PET_ID_BASE).
    pub owner: Option<EntityId>,

    /// Track 12 Piece A — sticky timestamp set when the owner issues
    /// a PetCommand (Attack or Back). The pre-AI inheritance pass
    /// skips re-targeting from `last_attacked_enemy` while this is
    /// within `PET_COMMAND_STICKY_SECS`, so a commanded target isn't
    /// blown away every tick.
    pub command_at: Option<Instant>,

    /// Track 12 Piece C — charm expiration. `Some(when)` on a pet
    /// produced by the PET_CHARM re-key path; the charm-sweep
    /// despawns the pet (mob "runs away") once this passes.
    /// `None` for normal summoned pets and world-spawned enemies.
    pub charm_expires_at: Option<Instant>,
    /// The damage ledger (`aggro`) the mob carried when it was charmed, kept
    /// on the pet and given back by `released_from_charm`. Without it a charm
    /// wiped the record of who had hurt the mob while keeping its wounds, so
    /// charming a nearly dead mob and letting it go took the kill from
    /// whoever did the work (and since a new charm releases the old one at
    /// once, that took two casts). Empty for everything but a charmed pet.
    /// `tick::wipe_hate` clears a player from it as from the live ledger.
    pub charm_ledger: HashMap<EntityId, f32>,
    /// Named-mob enrage. `None` for ordinary mobs and for named mobs whose
    /// `enrage_threshold` is 0. Once it fires it stays on for the rest of the
    /// life — enrage is a last stand, not a temporary buff, so unlike
    /// `active_cc` it has no expiry.
    pub enrage: Option<EnrageState>,

    /// Track 15.3 — pet stance. Set by `PetCommand::Guard` / `Sit` /
    /// `Back` (the latter restores Follow). Defaults to Follow on
    /// spawn. Non-pet entities leave this at Follow and the AI never
    /// reads it.
    pub stance: PetStance,

    /// Track 13 — primary stats. `stats` is the live value (base +
    /// active buff deltas); `base_stats` is the unbuffed baseline.
    /// World enemies leave both at zero (they never read stats); pets
    /// get a level-derived base on summon. Melee damage adds the STR
    /// buff delta `(stats.strength - base_stats.strength) / 5` on top of
    /// `mob.dmg`, mirroring `combat::calc_swing` while preserving the
    /// authored pet damage when unbuffed (delta 0).
    pub stats: PrimaryStats,
    pub base_stats: PrimaryStats,

    /// Track 13 — beneficial buffs applied to a pet (ALLY buffs cast on
    /// it). Empty for world enemies (never buffed). Ticked alongside the
    /// AI; StatBuff entries also mutate `stats` / `max_hp` on apply and
    /// undo on expire. Pet-irrelevant effects (mp regen, accuracy/crit)
    /// aren't routed here — pets have no mana / crit model.
    pub active_buffs: Vec<ActiveBuff>,

    /// Since when, and toward what, this entity has been held by the slope
    /// rule or by what stands on the ground (`StuckClock`); `None` while it
    /// is getting somewhere.
    pub stuck: Option<StuckClock>,

    /// Which side this entity last went round something that held its step
    /// (`step_on`): 1 or -1, 0 while its steps go as aimed. It keeps to the
    /// side, so it follows a wall to its end and does not dither at it.
    pub detour: i8,

    /// A wanderer's region and where it is in its walk (`roam.rs`,
    /// `tick_roam`); `None` for a camp mob, a dev spawn and a summoned pet.
    /// A charmed wanderer keeps it (and does not walk: it is a pet) so the
    /// mob handed back is a wanderer again.
    pub roam: Option<Roam>,
}

/// The stuck clock (the first hills): started by a step the slope rule refused
/// (a face the entity cannot climb), and kept until the entity comes
/// `STUCK_PROGRESS_M` closer to the same goal than it was then, or the goal
/// changes. Read against `MOB_STUCK_SECS`. A step taken does not stop it: a mob
/// sliding along a face's foot after a player walking the lip above moves, and
/// gets no closer.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct StuckClock {
    pub since: Instant,
    /// The target or owner it was heading for, or `None` for its home.
    pub goal: Option<EntityId>,
    /// Its distance to the goal when the clock started.
    pub start_dist: f32,
    /// The last step the slope rule refused (`Entity::is_evading`).
    pub last_refused: Instant,
}

/// Track 13 — five-stat block carried by an Entity. Used by pets so
/// ALLY stat buffs apply the same way they do to a player
/// `PerConnection`; zero for world enemies.
#[derive(Debug, Clone, Copy, Default)]
pub struct PrimaryStats {
    pub strength: i32,
    pub agility: i32,
    pub intelligence: i32,
    pub wisdom: i32,
    pub constitution: i32,
}

/// Outcome of one AI tick. Carries the events the tick loop needs to
/// fan out after the per-entity mutation pass.
#[derive(Debug, Default)]
pub struct AiEvents {
    /// `Some(new_target)` when this tick changed the entity's target
    /// (which may be `Some(id)` for a fresh acquisition or `None` for a
    /// drop). `None` (outer) means no change this tick.
    pub target_changed: Option<Option<EntityId>>,
    /// Filled when the entity's Attack state fired a melee swing this
    /// tick.
    pub hit: Option<HitIntent>,
    /// The mob reached home from a leash this tick and healed to full: its
    /// HP is told to every player, or their bars keep the HP it left with.
    pub healed_home: bool,
}

/// A named mob's enrage, once it has fired.
///
/// Deliberately not modelled as an `ActiveCc` or a buff: those expire, and
/// enrage does not. It is the mob's last stand, so it lasts until death. The
/// multipliers are read every tick by `melee_swing_damage` and by the chase /
/// leash speed lookups.
#[derive(Debug, Clone, Copy)]
pub struct EnrageState {
    pub damage_mult: f32,
    pub speed_mult: f32,
}

#[derive(Debug, Clone, Copy)]
pub struct HitIntent {
    pub target: EntityId,
    pub amount: i32,
}

impl Entity {
    pub fn from_spawn(
        spawn_point_idx: usize,
        spawn_pos: Vec3f,
        mob: MobTemplate,
        now: Instant,
    ) -> Self {
        let id = mint_enemy_id();
        let hp = mob.hp;
        let named = mob
            .named_id
            .as_deref()
            .and_then(crate::world::named::lookup);
        let mut e = Self {
            id,
            spawn_point_idx,
            spawn_pos,
            pos: spawn_pos,
            yaw: 0.0,
            hp,
            max_hp: hp,
            state: EnemyState::Idle,
            target: None,
            aggro: HashMap::new(),
            threat: HashMap::new(),
            last_attack_at: None,
            state_entered_at: now,
            mob,
            seq: 0,
            last_bcast_pos: spawn_pos,
            last_bcast_yaw: 0.0,
            last_bcast_at: None,
            active_cc: Vec::new(),
            active_dots: Vec::new(),
            last_status_fanned: Vec::new(),
            owner: None,
            command_at: None,
            charm_expires_at: None,
            charm_ledger: HashMap::new(),
            enrage: None,
            stance: PetStance::Follow,
            stats: PrimaryStats::default(),
            base_stats: PrimaryStats::default(),
            active_buffs: Vec::new(),
            stuck: None,
            detour: 0,
            roam: None,
        };
        // Single choke point for both spawn paths (camp spawner and dev
        // spawn), so a named mob is scaled exactly once no matter how it
        // entered the world.
        if let Some(n) = named {
            e.apply_named(n);
        }
        e
    }

    /// Track 11 — instantiate a player-owned pet. Uses the pet id
    /// partition (`>= PET_ID_BASE`) so the client routes pet-related
    /// broadcasts separately from enemies. `spawn_point_idx` is set
    /// to `usize::MAX` since pets aren't tied to a respawn point;
    /// nothing in the code path that consumes this field runs for
    /// pets (corpse cleanup arms by id partition for the despawn
    /// fan-out instead).
    pub fn from_pet_summon(owner: EntityId, pos: Vec3f, mob: MobTemplate, now: Instant) -> Self {
        let id = mint_pet_id();
        let hp = mob.hp;
        // Track 13 — pets get a level-derived primary-stat baseline so
        // ALLY stat buffs have something to add to. The absolute value is
        // display-facing only; melee damage uses the buff delta over this
        // baseline, so an unbuffed pet still deals exactly `mob.dmg`.
        let s = 50 + mob.level as i32 * 2;
        let base = PrimaryStats {
            strength: s,
            agility: s,
            intelligence: s,
            wisdom: s,
            constitution: s,
        };
        Self {
            id,
            spawn_point_idx: usize::MAX,
            spawn_pos: pos,
            pos,
            yaw: 0.0,
            hp,
            max_hp: hp,
            state: EnemyState::Idle,
            target: None,
            aggro: HashMap::new(),
            threat: HashMap::new(),
            last_attack_at: None,
            state_entered_at: now,
            mob,
            seq: 0,
            last_bcast_pos: pos,
            last_bcast_yaw: 0.0,
            last_bcast_at: None,
            active_cc: Vec::new(),
            active_dots: Vec::new(),
            last_status_fanned: Vec::new(),
            owner: Some(owner),
            command_at: None,
            charm_expires_at: None,
            charm_ledger: HashMap::new(),
            enrage: None,
            stance: PetStance::Follow,
            stats: base,
            base_stats: base,
            active_buffs: Vec::new(),
            stuck: None,
            detour: 0,
            roam: None,
        }
    }

    /// A charm ending: the pet becomes the enemy it was (user call
    /// 2026-10-06, EQ's rule; the v1 "mob runs away" despawn is gone). A
    /// fresh enemy id (ids are never reused), the camp slot and home position
    /// the charm carried along, the pet's current HP and position, the same
    /// template, and the damage ledger it had when charmed (`charm_ledger`).
    /// `from_spawn`'s named scaling is skipped on purpose:
    /// `apply_named` multiplied this template's hp and dmg in place when the
    /// mob first spawned and the charm cloned it scaled, so a second pass
    /// would double a named mob. Hostility is the caller's choice (`turn_on`):
    /// an owner who logged out is not there to be hated.
    pub fn released_from_charm(pet: &Entity, now: Instant) -> Self {
        let mut mob = pet.mob.clone();
        let named_id = mob.named_id.take();
        let mut e = Self::from_spawn(pet.spawn_point_idx, pet.spawn_pos, mob, now);
        e.mob.named_id = named_id;
        e.pos = pet.pos;
        e.yaw = pet.yaw;
        e.last_bcast_pos = pet.pos;
        e.hp = pet.hp;
        e.max_hp = pet.max_hp;
        // Who had hurt it before the charm still has (see `charm_ledger`).
        e.aggro = pet.charm_ledger.clone();
        // A wanderer is a wanderer again, its walk forgotten. Its home is the
        // one it had when charmed, never where the charm ended (often in
        // town, where the charmer took it).
        e.roam = pet.roam.map(Roam::rested);
        e
    }

    /// A charm that breaks sends the mob straight for the one who charmed it.
    /// Threat is what the AI reads for its target; the `aggro` ledger stays
    /// untouched because the charmer has dealt it no damage.
    pub fn turn_on(&mut self, former_owner: EntityId, now: Instant) {
        self.threat.insert(former_owner, self.max_hp.max(1.0));
        self.target = Some(former_owner);
        self.transition(EnemyState::Chase, now);
    }

    /// Nobody to hate (the charmer logged out): the returned mob walks home
    /// like one that lost its target, rather than standing wherever the pet
    /// happened to be (playtest 2026-10-07, the walk-away and quit cases
    /// should look the same). Already home, it stays idle.
    pub fn go_home(&mut self, now: Instant) {
        if self.pos.distance_to(self.spawn_pos) >= LEASH_HOME_TOLERANCE {
            self.transition(EnemyState::Leash, now);
        }
    }

    /// Track 13 — add a stat buff's deltas to the pet's live stats +
    /// max_hp. Mirror of `buffs::apply_stat_deltas` for a connection.
    /// `max_mp_delta` is ignored — pets have no mana pool.
    pub fn apply_stat_deltas(&mut self, s: i32, a: i32, i: i32, w: i32, c: i32, max_hp_delta: f32) {
        self.stats.strength += s;
        self.stats.agility += a;
        self.stats.intelligence += i;
        self.stats.wisdom += w;
        self.stats.constitution += c;
        self.max_hp = (self.max_hp + max_hp_delta).max(1.0);
    }

    /// Track 13 — reverse a stat buff's deltas (expire / refresh).
    /// Clamps current hp against the reduced max_hp.
    pub fn undo_stat_deltas(&mut self, s: i32, a: i32, i: i32, w: i32, c: i32, max_hp_delta: f32) {
        self.stats.strength -= s;
        self.stats.agility -= a;
        self.stats.intelligence -= i;
        self.stats.wisdom -= w;
        self.stats.constitution -= c;
        self.max_hp = (self.max_hp - max_hp_delta).max(1.0);
        if self.hp > self.max_hp {
            self.hp = self.max_hp;
        }
    }

    /// Track 13 — apply (or refresh) a buff on this pet. A same-named
    /// re-cast undoes the old stat deltas before applying the new ones,
    /// so refresh never double-stacks. Mirrors the connection-side
    /// `apply_buff` + `apply_stat_buff` in tick/spell_buffs.rs.
    pub fn apply_buff(&mut self, buff: ActiveBuff) {
        if let Some(idx) = self.active_buffs.iter().position(|b| b.name == buff.name) {
            if let BuffEffect::StatBuff {
                strength,
                agility,
                intelligence,
                wisdom,
                constitution,
                max_hp_delta,
                ..
            } = self.active_buffs[idx].effect
            {
                self.undo_stat_deltas(
                    strength,
                    agility,
                    intelligence,
                    wisdom,
                    constitution,
                    max_hp_delta,
                );
            }
            self.active_buffs.remove(idx);
        }
        if let BuffEffect::StatBuff {
            strength,
            agility,
            intelligence,
            wisdom,
            constitution,
            max_hp_delta,
            ..
        } = buff.effect
        {
            self.apply_stat_deltas(
                strength,
                agility,
                intelligence,
                wisdom,
                constitution,
                max_hp_delta,
            );
        }
        self.active_buffs.push(buff);
    }

    /// Track 13 — tick this pet's active buffs by `dt`: apply HoT heal,
    /// decrement durations, and remove + undo expired entries (StatBuff
    /// deltas are reversed on expire). Returns `(hp_changed, set_changed)`
    /// so the caller fans a HealthUpdate / BuffSnapshot. No-op (returns
    /// false,false) when there are no buffs — the common case for enemies.
    pub fn tick_buffs(&mut self, dt: f32) -> (bool, bool) {
        if self.active_buffs.is_empty() {
            return (false, false);
        }
        // Decrement durations + accumulate whole-HP HoT heals (sub-1-HP
        // remainders carry in tick_acc, matching the connection buff tick).
        let mut heal: f32 = 0.0;
        for b in self.active_buffs.iter_mut() {
            b.remaining -= dt;
            if let BuffEffect::Hot { hps } = b.effect {
                b.tick_acc += hps * dt;
                if b.tick_acc >= 1.0 {
                    let whole = b.tick_acc.floor();
                    b.tick_acc -= whole;
                    heal += whole;
                }
            }
        }
        // Remove expired; reverse StatBuff deltas first so stats/max_hp
        // return to baseline.
        let mut set_changed = false;
        let mut i = 0;
        while i < self.active_buffs.len() {
            if self.active_buffs[i].remaining <= 0.0 {
                if let BuffEffect::StatBuff {
                    strength,
                    agility,
                    intelligence,
                    wisdom,
                    constitution,
                    max_hp_delta,
                    ..
                } = self.active_buffs[i].effect
                {
                    self.undo_stat_deltas(
                        strength,
                        agility,
                        intelligence,
                        wisdom,
                        constitution,
                        max_hp_delta,
                    );
                }
                self.active_buffs.remove(i);
                set_changed = true;
            } else {
                i += 1;
            }
        }
        let mut hp_changed = false;
        if heal > 0.0 {
            let new_hp = (self.hp + heal).min(self.max_hp);
            if new_hp != self.hp {
                self.hp = new_hp;
                hp_changed = true;
            }
        }
        // A stat-buff expire may have lowered max_hp (and clamped hp) — fan
        // a HealthUpdate so the bar reflects the new cap.
        if set_changed {
            hp_changed = true;
        }
        (hp_changed, set_changed)
    }

    pub fn is_pet(&self) -> bool {
        self.owner.is_some()
    }

    pub fn is_alive(&self) -> bool {
        !matches!(self.state, EnemyState::Dead)
    }

    /// WoW's evade (the user's call, 2026-11-05): a mob that cannot fight
    /// back takes no damage. True while it walks home (`Leash`), and while
    /// it chases a target the slope rule holds it below: held past
    /// `EVADE_GRACE_SECS` and refused within `EVADE_REFUSAL_RECENT_SECS`.
    /// Without it, a caster on a ledge above a mob nuked it for the 8 s it was
    /// held and the whole walk home, and a mob pulled past its leash could be
    /// nuked all the way home; neither ever hit back. Never a pet: a pet has
    /// an owner to answer for it. Every place that damages a mob reads this
    /// (the swing, the pet's swing, the spell pre-flight, an AoE's victims, a
    /// DoT's tick).
    pub fn is_evading(&self, now: Instant) -> bool {
        if self.is_pet() {
            return false;
        }
        match self.state {
            EnemyState::Leash => true,
            EnemyState::Chase => self.stuck.is_some_and(|c| {
                now.duration_since(c.since).as_secs_f32() >= crate::world::EVADE_GRACE_SECS
                    && now.duration_since(c.last_refused).as_secs_f32()
                        <= crate::world::EVADE_REFUSAL_RECENT_SECS
            }),
            _ => false,
        }
    }

    pub fn transition(&mut self, new_state: EnemyState, now: Instant) {
        if self.state == new_state {
            return;
        }
        // A mob that starts walking home sheds every DoT on it (spell batch
        // step 5): tagging a mob and letting it leash to death from safety
        // would otherwise pay full rewards for no risk. A dead one has
        // nothing left to tick.
        if matches!(new_state, EnemyState::Leash | EnemyState::Dead) {
            self.active_dots.clear();
        }
        self.state = new_state;
        self.state_entered_at = now;
        self.stuck = None;
    }

    /// One step toward `to`, standing on the ground: turned aside by what
    /// stands on it exactly as a player's step is (`Obstacles::resolve`, the
    /// same body radius, so a mob fits wherever a player does), and refused
    /// up a face the slope rule forbids (the player's own rule,
    /// `Terrain::slope_ok`, which also refuses a diagonal onto a face).
    ///
    /// A step that is held (it gets less than `DETOUR_BELOW` of the way: a
    /// wall square in front) is tried again turned 45 and then 90 degrees to
    /// one side, then the other, keeping to the side it last went: the mob
    /// walks along the wall to its end and comes round.
    ///
    /// Returns whether the step got on with it: taken as aimed, or turned
    /// aside but still most of the way (a glance off a trunk). A step the
    /// slope rule refused, or one held so short that it needed a way round
    /// (whether or not one was found), returns false: the stuck clock starts
    /// on those and ends when the mob is a metre nearer its goal, and the
    /// evade rule reads them as "not on a clear run at its target". A mob
    /// sliding past trees in a chase is never held; one following a wall is,
    /// for as long as it takes.
    fn step_on(&mut self, terrain: &crate::world::terrain::Terrain, to: Vec3f) -> bool {
        // Sine and cosine of the two turns tried.
        const TURNS: [(f32, f32); 2] = [
            (
                std::f32::consts::FRAC_1_SQRT_2,
                std::f32::consts::FRAC_1_SQRT_2,
            ),
            (1.0, 0.0),
        ];
        let obstacles = terrain.obstacles();
        let from = P2::of(self.pos);
        let aim = P2::of(to);
        let want = from.dist_to(aim);
        let direct = obstacles.resolve(from, aim, BODY_RADIUS);
        let mut end = P2::new(direct.x, direct.z);
        let held =
            direct.deflected && want > 0.0 && from.dist_to(end) < want * crate::world::DETOUR_BELOW;
        if !direct.deflected {
            self.detour = 0;
        } else if held {
            let first: f32 = if self.detour < 0 { -1.0 } else { 1.0 };
            let (dx, dz) = (aim.x - from.x, aim.z - from.z);
            'sides: for side in [first, -first] {
                for (sin, cos) in TURNS {
                    let s = sin * side;
                    let turned = P2::new(from.x + dx * cos - dz * s, from.z + dx * s + dz * cos);
                    let r = obstacles.resolve(from, turned, BODY_RADIUS);
                    let got = P2::new(r.x, r.z);
                    if from.dist_to(got) >= want * crate::world::DETOUR_BELOW {
                        end = got;
                        self.detour = side as i8;
                        break 'sides;
                    }
                }
            }
        }
        let to = Vec3f {
            x: end.x,
            y: to.y,
            z: end.z,
        };
        if !terrain.slope_ok(self.pos, to) {
            return false;
        }
        self.pos = terrain.ground_vec(to);
        !held
    }

    /// The stuck clock after a step toward `goal`, now `dist` away, was tried
    /// (`StuckClock`): cleared by a new goal or by coming `STUCK_PROGRESS_M`
    /// closer, started by a refused step. True once it has run
    /// `MOB_STUCK_SECS`.
    fn stuck_too_long(
        &mut self,
        moved: bool,
        goal: Option<EntityId>,
        dist: f32,
        now: Instant,
    ) -> bool {
        if let Some(c) = self.stuck {
            if c.goal != goal || dist <= c.start_dist - crate::world::STUCK_PROGRESS_M {
                self.stuck = None;
            }
        }
        if !moved {
            match self.stuck.as_mut() {
                Some(c) => c.last_refused = now,
                None => {
                    self.stuck = Some(StuckClock {
                        since: now,
                        goal,
                        start_dist: dist,
                        last_refused: now,
                    })
                }
            }
        }
        self.stuck.is_some_and(|c| {
            now.duration_since(c.since).as_secs_f32() >= crate::world::MOB_STUCK_SECS
        })
    }

    /// The present attacker with the most threat on this mob within its
    /// leash, if any: who it turns on when nothing else has its attention.
    fn best_hater_within_leash(&self, targets: &[(EntityId, Vec3f)]) -> Option<EntityId> {
        let mut best: Option<(EntityId, f32)> = None;
        for (&id, &threat) in &self.threat {
            if threat <= 0.0 {
                continue;
            }
            let Some((_, pos)) = targets.iter().find(|(t, _)| *t == id) else {
                continue;
            };
            if self.pos.distance_to(*pos) > self.leash_range() {
                continue;
            }
            match best {
                None => best = Some((id, threat)),
                Some((_, b)) if threat > b => best = Some((id, threat)),
                _ => {}
            }
        }
        best.map(|(id, _)| id)
    }

    /// How far from itself a mob keeps chasing, and how far out it will
    /// turn on something that hits it. Never below `MIN_LEASH_RANGE` for a
    /// mob that aggros at all, so nothing in spell reach is out of its reach.
    pub fn leash_range(&self) -> f32 {
        let configured = self.mob.leash.unwrap_or(self.mob.aggro * 2.0);
        if self.mob.aggro > 0.0 {
            configured.max(super::MIN_LEASH_RANGE)
        } else {
            configured
        }
    }

    pub fn melee_range(&self) -> f32 {
        self.mob.melee_range.unwrap_or(1.8)
    }

    pub fn attack_interval(&self) -> f32 {
        let base = self.mob.attack_interval.unwrap_or(2.5);
        // Attack slow adds fractional delay; haste divides it out. Pet
        // attack pacing is server-driven (unlike players, who pace their
        // swings locally), so a pet's Haste buff actually speeds it up here.
        let slow_mult = 1.0 + self.attack_slow_factor();
        base * slow_mult / (1.0 + self.haste_total())
    }

    /// Track 13 — sum of active Haste buff amounts (0.0 if none).
    fn haste_total(&self) -> f32 {
        self.active_buffs
            .iter()
            .filter_map(|b| match b.effect {
                BuffEffect::Haste { amount } => Some(amount),
                _ => None,
            })
            .sum()
    }

    /// Track 13 — base move speed scaled by the highest active Speed buff
    /// (Spirit of Wolf). 1.0× for enemies and unbuffed pets.
    pub fn move_speed(&self) -> f32 {
        let mut mult = 1.0_f32;
        for b in &self.active_buffs {
            if let BuffEffect::Speed { mult: m } = b.effect {
                if m > mult {
                    mult = m;
                }
            }
        }
        self.enraged_speed() * mult
    }

    /// Track 13 — melee swing damage: `mob.dmg` plus the STR buff bonus
    /// (current STR over the unbuffed baseline, `/5` like the player
    /// formula in `combat::calc_swing`). Unbuffed entities — every world
    /// enemy, and any pet with no stat buff — return `mob.dmg` unchanged.
    /// Floored at 1.
    pub fn melee_swing_damage(&self) -> i32 {
        let str_bonus = (self.stats.strength - self.base_stats.strength) / 5;
        let base = (self.mob.dmg + str_bonus).max(1);
        match self.enrage {
            Some(e) => ((base as f32) * e.damage_mult).round().max(1.0) as i32,
            None => base,
        }
    }

    /// Move speed including a named mob's enrage bonus.
    ///
    /// Every movement path must read speed through this. `move_speed()` below
    /// is consulted only by pet AI, while world mobs read their speed directly
    /// in chase and leash — so a multiplier applied in `move_speed()` alone
    /// would silently do nothing to the very creatures enrage is for.
    fn enraged_speed(&self) -> f32 {
        match self.enrage {
            Some(e) => self.mob.speed * e.speed_mult,
            None => self.mob.speed,
        }
    }

    /// Apply a named-mob definition at spawn: stat multipliers, level and
    /// display name. Enrage is armed here but does not fire until HP crosses
    /// the threshold. Scales whatever template the mob spawned from, so a
    /// tagged camp mob becomes a bigger version of itself.
    pub fn apply_named(&mut self, named: &crate::world::named::NamedMob) {
        self.mob.name = named.full_name();
        self.mob.level = named.level;
        self.mob.hp *= named.hp_mult;
        self.hp = self.mob.hp;
        self.max_hp = self.mob.hp;
        self.mob.dmg = ((self.mob.dmg as f32) * named.damage_mult).round().max(1.0) as i32;
    }

    /// Fire enrage if this is a named mob that enrages, it has not already
    /// fired, and HP has crossed the threshold. Returns true on the tick it
    /// fires so the caller can log it.
    ///
    /// Checked once per AI tick rather than at each damage site: enemy HP is
    /// reduced in six separate places with no shared helper, and the AI sweep
    /// is downstream of all of them in the same tick.
    pub fn maybe_enrage(&mut self, named: &crate::world::named::NamedMob) -> bool {
        if self.enrage.is_some() || !named.enrages() || self.max_hp <= 0.0 {
            return false;
        }
        if self.hp / self.max_hp > named.enrage_threshold {
            return false;
        }
        self.enrage = Some(EnrageState {
            damage_mult: named.enrage_damage_mult,
            speed_mult: named.enrage_speed_mult,
        });
        true
    }

    /// Tick all active CC durations down by `dt`. Call once per AI tick
    /// before the state machine.
    pub fn tick_cc(&mut self, dt: f32) {
        self.active_cc.retain_mut(|cc| {
            cc.remaining -= dt;
            cc.remaining > 0.0
        });
    }

    /// Apply a CC effect, replacing any existing instance of the same kind
    /// (re-cast refreshes duration rather than stacking).
    pub fn apply_cc(&mut self, cc: ActiveCc) {
        // One attack slow at a time, strongest wins (user call 2026-10-05): a
        // weaker or equal slow never replaces a stronger one that still
        // holds. Every other kind replaces its own kind as before.
        if let CcKind::AttackSlow { factor_pct } = cc.kind {
            if self.attack_slow_pct() > factor_pct {
                return;
            }
        }
        let kind_disc = std::mem::discriminant(&cc.kind);
        self.active_cc
            .retain(|c| std::mem::discriminant(&c.kind) != kind_disc);
        self.active_cc.push(cc);
    }

    /// The attack slow holding this entity, as the percentage the spell
    /// data carries (0 when none). The pre-flight reads it to refuse a
    /// weaker slow before any cost.
    pub fn attack_slow_pct(&self) -> u8 {
        self.active_cc
            .iter()
            .filter_map(|c| match c.kind {
                CcKind::AttackSlow { factor_pct } => Some(factor_pct),
                _ => None,
            })
            .max()
            .unwrap_or(0)
    }

    /// Clear all mez effects (called when the enemy takes damage).
    pub fn clear_mez(&mut self) {
        self.active_cc.retain(|c| !matches!(c.kind, CcKind::Mez));
    }

    /// Put a DoT on this entity, replacing an earlier one of the same spell
    /// (a re-cast refreshes, never stacks). A DoT breaks mez like any damage.
    pub fn apply_dot(&mut self, mut dot: ActiveDot) {
        // A mob walking home takes no DoT: it heals and forgets at the door
        // and, idle there, cannot acquire a caster beyond its leash, so a
        // DoT landed on the walk would pay for a kill with no fight.
        if matches!(self.state, EnemyState::Leash) {
            return;
        }
        dot.ticks_left = (dot.remaining / super::DOT_TICK_SECS).floor().max(0.0) as u32;
        dot.tick_acc = 0.0;
        self.active_dots.retain(|d| d.name != dot.name);
        self.active_dots.push(dot);
        self.clear_mez();
    }

    /// Advance every DoT by `dt`, dealing a tick of `dps x DOT_TICK_SECS`
    /// whenever one's accumulator crosses the tick length, and dropping the
    /// ones whose ticks are all dealt. Returns the ticks dealt, for the
    /// caller to fan, credit and, on a kill, reward. Damage comes in whole
    /// ticks only: a duration that is not a multiple of the tick length
    /// deals its floor and the remainder is never dealt.
    pub fn tick_dots(&mut self, dt: f32) -> Vec<DotTick> {
        let mut ticks = Vec::new();
        if self.active_dots.is_empty() || !self.is_alive() {
            return ticks;
        }
        let tick_len = super::DOT_TICK_SECS;
        for dot in self.active_dots.iter_mut() {
            dot.remaining -= dt;
            dot.tick_acc += dt;
            if dot.tick_acc >= tick_len && dot.ticks_left > 0 {
                dot.tick_acc -= tick_len;
                dot.ticks_left -= 1;
                let amount = (dot.dps * tick_len).round().max(0.0) as i32;
                if amount > 0 {
                    ticks.push(DotTick {
                        name: dot.name.clone(),
                        caster: dot.caster,
                        amount,
                        dmg_type: dot.dmg_type,
                    });
                }
            }
        }
        self.active_dots.retain(|d| d.ticks_left > 0);
        if !ticks.is_empty() {
            self.clear_mez();
        }
        ticks
    }

    /// Every DoT this caster has on the entity ends: they died, ported or
    /// logged out, so nothing they left behind keeps paying them.
    pub fn clear_dots_from(&mut self, caster: EntityId) {
        self.active_dots.retain(|d| d.caster != caster);
    }

    /// Everything on this entity that a target frame shows, as the
    /// BuffSnapshot pairs (name, seconds left): its buffs, its DoTs, and its
    /// crowd control under plain labels. The AI sweep fans it whenever the
    /// set of names changes (spell batch step 5).
    pub fn status_pairs(&self) -> Vec<(String, f32)> {
        let mut pairs = super::buffs::snapshot_pairs(&self.active_buffs);
        for d in &self.active_dots {
            pairs.push((d.name.clone(), d.remaining.max(0.0)));
        }
        for cc in &self.active_cc {
            let label = match cc.kind {
                CcKind::Mez => "Mesmerized",
                CcKind::Root => "Rooted",
                CcKind::Snare { .. } => "Snared",
                CcKind::AttackSlow { .. } => "Slowed",
                CcKind::Stun => "Stunned",
            };
            pairs.push((label.to_string(), cc.remaining.max(0.0)));
        }
        pairs
    }

    pub fn is_mezzed(&self) -> bool {
        self.active_cc.iter().any(|c| matches!(c.kind, CcKind::Mez))
    }

    pub fn is_rooted(&self) -> bool {
        self.active_cc
            .iter()
            .any(|c| matches!(c.kind, CcKind::Root))
    }

    pub fn is_stunned(&self) -> bool {
        self.active_cc
            .iter()
            .any(|c| matches!(c.kind, CcKind::Stun))
    }

    /// Speed multiplier from snare (0.0 = full speed, 0.5 = half speed).
    fn snare_factor(&self) -> f32 {
        self.active_cc
            .iter()
            .filter_map(|c| {
                if let CcKind::Snare { factor_pct } = c.kind {
                    Some(factor_pct as f32 / 100.0)
                } else {
                    None
                }
            })
            .fold(0.0_f32, f32::max)
    }

    /// Extra attack-interval fraction from attack-slow effects.
    fn attack_slow_factor(&self) -> f32 {
        self.active_cc
            .iter()
            .filter_map(|c| {
                if let CcKind::AttackSlow { factor_pct } = c.kind {
                    Some(factor_pct as f32 / 100.0)
                } else {
                    None
                }
            })
            .fold(0.0_f32, f32::max)
    }

    /// Whether this tick should fan a Position for the entity: it moved or
    /// turned since the last send, has never been sent, or the keepalive
    /// gap has elapsed. The change-or-keepalive gate `regen.rs` uses for
    /// resources, applied to position.
    pub fn position_broadcast_due(&self, now: Instant) -> bool {
        let Some(sent_at) = self.last_bcast_at else {
            return true;
        };
        self.pos.sub(self.last_bcast_pos).length() > MOVED_EPSILON
            || (self.yaw - self.last_bcast_yaw).abs() > YAW_EPSILON
            || now.duration_since(sent_at) >= crate::world::ENEMY_POSITION_KEEPALIVE
    }

    /// Record a Position fan-out. Call BEFORE building the message: it
    /// advances the sequence the client orders on and snapshots the pos /
    /// yaw that go out, so the next `position_broadcast_due` compares
    /// against what the client actually has.
    pub fn mark_position_broadcast(&mut self, now: Instant) {
        self.seq = self.seq.wrapping_add(1);
        self.last_bcast_pos = self.pos;
        self.last_bcast_yaw = self.yaw;
        self.last_bcast_at = Some(now);
    }

    /// Drive one AI tick. Mutates state / target / pos / last_attack_at;
    /// returns the events the tick loop should fan out (target switch,
    /// melee swing). The caller is responsible
    /// for owning `targets` (a snapshot of aggro-able entity positions —
    /// players plus alive pets — for this tick; we don't borrow connections
    /// across the entity loop) and `enemy_targets` (pet targets — alive
    /// non-pet enemies, used by pet AI to chase its inherited target).
    ///
    /// Caster kiting and healer flee are not wired here; the starter zone
    /// has no caster or healer mobs (all 9 archetypes are melee). When
    /// those land, branch here on `mob.spell_damage > 0` and
    /// `mob.healer_flee_hp > 0.0`.
    ///
    /// `hidden` names the players a Hide holds: an idle mob's proximity scan
    /// passes over them (a mob already provoked still finds them through its
    /// threat table, and the first swing ends the hide anyway).
    ///
    /// `terrain` is the ground (the height model): every step a mob or pet
    /// takes ends standing on it, and an idle mob notices only what it can
    /// see across it.
    pub fn tick_ai(
        &mut self,
        targets: &[(EntityId, Vec3f)],
        enemy_targets: &[(EntityId, Vec3f, bool)],
        hidden: &[EntityId],
        dt: f32,
        now: Instant,
        terrain: &crate::world::terrain::Terrain,
    ) -> AiEvents {
        self.tick_cc(dt);
        let prev_target = self.target;
        let mut events = AiEvents::default();
        // Mez and stun skip the entire state machine (mob stands frozen).
        if self.is_mezzed() || self.is_stunned() {
            return events;
        }
        // Track 11 — pets run a distinct state machine. They follow
        // their owner by default and inherit attack targets from the
        // owner's last melee/spell hit (set by the tick loop before
        // this AI pass runs).
        if self.is_pet() {
            self.tick_pet_ai(targets, enemy_targets, dt, now, &mut events, terrain);
        } else {
            match self.state {
                EnemyState::Idle => self.tick_idle(targets, hidden, now, terrain),
                EnemyState::Chase => self.tick_chase(targets, dt, now, terrain),
                EnemyState::Attack => self.tick_attack(targets, now, &mut events),
                EnemyState::Leash => events.healed_home = self.tick_leash(dt, now, terrain),
                EnemyState::Dead => {}
            }
        }
        if self.target != prev_target {
            events.target_changed = Some(self.target);
        }
        events
    }

    /// Track 11 — pet AI. Follow owner unless owner has acquired an
    /// enemy target recently (set externally by the tick loop into
    /// `self.target`); in that case, chase + melee the target until
    /// it dies or moves out of leash.
    ///
    /// The pet's `spawn_pos` is reused as a "home" reference but
    /// follow doesn't leash to it (pets follow their owner across
    /// the world). Leash transitions are skipped entirely; pets only
    /// despawn when their owner disconnects (handled in the
    /// transport disconnect arm).
    fn tick_pet_ai(
        &mut self,
        targets: &[(EntityId, Vec3f)],
        enemy_targets: &[(EntityId, Vec3f, bool)],
        dt: f32,
        now: Instant,
        events: &mut AiEvents,
        terrain: &crate::world::terrain::Terrain,
    ) {
        const PET_FOLLOW_DISTANCE: f32 = 3.0;
        let Some(owner_id) = self.owner else {
            // Pet with no owner — pathological state, do nothing.
            return;
        };
        // Track 15.3 — SIT drops any current target and stands still
        // before anything else runs. The inheritance pre-pass (tick/enemy_ai.rs)
        // already skips non-Follow stances, but a pre-existing target
        // (commanded or inherited before stance change) still needs
        // clearing here.
        if self.stance == PetStance::Sit {
            if self.target.is_some() {
                self.target = None;
            }
            return;
        }
        let owner_pos_opt = target_pos(targets, owner_id);
        let target_info: Option<(EntityId, Vec3f, bool)> = self.target.and_then(|tid| {
            enemy_targets
                .iter()
                .find(|(id, _, _)| *id == tid)
                .map(|(id, pos, alive)| (*id, *pos, *alive))
        });
        // If we have a live target, engage it; otherwise follow owner
        // (Follow stance) or hold position (Guard stance).
        match target_info {
            Some((target_id, target_pos, true)) => {
                let dist = self.pos.distance_to(target_pos);
                let melee = self.melee_range();
                if dist <= melee {
                    // In melee range — swing on cadence.
                    self.stuck = None;
                    self.face_toward(target_pos);
                    let due = match self.last_attack_at {
                        None => true,
                        Some(t) => now.duration_since(t).as_secs_f32() >= self.attack_interval(),
                    };
                    if due {
                        self.last_attack_at = Some(now);
                        events.hit = Some(HitIntent {
                            target: target_id,
                            amount: self.melee_swing_damage(),
                        });
                    }
                } else if !self.is_rooted() {
                    let snare = self.snare_factor();
                    let step = self.move_speed() * (1.0 - snare) * dt;
                    self.face_toward(target_pos);
                    let moved = self.step_on(terrain, self.pos.step_toward(target_pos, step));
                    let dist = self.pos.distance_to(target_pos);
                    if self.stuck_too_long(moved, Some(target_id), dist, now) {
                        // A target it cannot reach (above a face): drop it.
                        // The follow branch takes over, and the inheritance
                        // pre-pass may hand it back to try again.
                        self.target = None;
                        self.stuck = None;
                    }
                }
            }
            _ => {
                // Drop dead/missing target.
                if target_info.is_some() {
                    self.target = None;
                }
                // Track 15.3 — Guard stance pins the pet at its
                // current position. No follow; just stand here until
                // commanded otherwise or the inheritance/damage path
                // re-targets us.
                if self.stance == PetStance::Guard {
                    self.stuck = None;
                    return;
                }
                let Some(owner_pos) = owner_pos_opt else {
                    // Owner offline — stand still (cleanup runs on
                    // disconnect, so this is short-lived).
                    return;
                };
                // The follow distance is on the ground (XZ): a pet standing
                // under an owner above it (a GM in flight; an owner on a
                // ledge it has warped to) has arrived, where a 3D distance
                // never shrank below the height and the stuck clock warped it
                // to where it already stood every MOB_STUCK_SECS.
                let dist = self.pos.xz_distance_to(owner_pos);
                if dist > PET_FOLLOW_DISTANCE && !self.is_rooted() {
                    let snare = self.snare_factor();
                    let step = self.move_speed() * (1.0 - snare) * dt;
                    self.face_toward(owner_pos);
                    let moved = self.step_on(terrain, self.pos.step_toward(owner_pos, step));
                    let dist = self.pos.xz_distance_to(owner_pos);
                    if self.stuck_too_long(moved, Some(owner_id), dist, now) {
                        // An owner it cannot reach (on a ledge): EQ's pets warp
                        // to their owner. Only to where the owner already is.
                        self.pos = terrain.ground_vec(owner_pos);
                        self.stuck = None;
                    }
                } else {
                    self.stuck = None;
                }
            }
        }
    }

    fn tick_idle(
        &mut self,
        targets: &[(EntityId, Vec3f)],
        hidden: &[EntityId],
        now: Instant,
        terrain: &crate::world::terrain::Terrain,
    ) {
        if let Some((id, _)) =
            nearest_target_within(self.pos, targets, self.mob.aggro, hidden, terrain)
        {
            self.target = Some(id);
            self.transition(EnemyState::Chase, now);
            return;
        }
        // Being attacked IS aggro (playtest 2026-09-22): every damage path
        // already writes the attacker into `threat`, but idle mobs never read
        // it, so a ranged attacker outside the aggro radius could farm a mob
        // that stood still taking it — exploit-shaped, not just wrong-feeling.
        // Acquire the highest-threat attacker still present, but only within
        // leash range: tick_chase drops any target past leash_range() on its
        // first tick, so acquiring beyond it would flap Idle -> Chase -> Leash
        // forever instead of fighting.
        if let Some(id) = self.best_hater_within_leash(targets) {
            self.target = Some(id);
            self.transition(EnemyState::Chase, now);
        }
    }

    fn tick_chase(
        &mut self,
        targets: &[(EntityId, Vec3f)],
        dt: f32,
        now: Instant,
        terrain: &crate::world::terrain::Terrain,
    ) {
        self.maybe_switch_target_by_threat(targets);
        let Some(target_id) = self.target else {
            self.next_hater_or_home(targets, now);
            return;
        };
        let Some(target_pos) = target_pos(targets, target_id) else {
            // Target left the world, died or is no longer a target.
            self.target = None;
            self.next_hater_or_home(targets, now);
            return;
        };
        let dist = self.pos.distance_to(target_pos);
        if dist > self.leash_range() {
            // Out past the leash: forget its hate, so the threat switch
            // cannot hand it straight back, but keep its damage (`aggro`),
            // which is kill credit.
            self.threat.remove(&target_id);
            self.target = None;
            self.next_hater_or_home(targets, now);
            return;
        }
        if dist <= self.melee_range() {
            self.transition(EnemyState::Attack, now);
            return;
        }
        // Root and snare block / slow movement. Rooted is not stuck.
        if self.is_rooted() {
            self.stuck = None;
            return;
        }
        let snare = self.snare_factor();
        let step = self.enraged_speed() * (1.0 - snare) * dt;
        self.face_toward(target_pos);
        let moved = self.step_on(terrain, self.pos.step_toward(target_pos, step));
        let dist = self.pos.distance_to(target_pos);
        if self.stuck_too_long(moved, Some(target_id), dist, now) {
            // EQ's rule: a mob that cannot reach its target (above a face)
            // gives up on it. It forgets that one only and turns on the next
            // attacker it has hate for; with none left it walks home, where
            // tick_leash heals it to full.
            self.threat.remove(&target_id);
            self.aggro.remove(&target_id);
            self.stuck = None;
            self.target = self.best_hater_within_leash(targets);
            if self.target.is_none() {
                self.threat.clear();
                self.aggro.clear();
                self.transition(EnemyState::Leash, now);
            }
        }
    }

    fn tick_attack(&mut self, targets: &[(EntityId, Vec3f)], now: Instant, events: &mut AiEvents) {
        self.maybe_switch_target_by_threat(targets);
        let Some(target_id) = self.target else {
            self.next_hater_or_home(targets, now);
            return;
        };
        let Some(target_pos) = target_pos(targets, target_id) else {
            self.target = None;
            self.next_hater_or_home(targets, now);
            return;
        };
        let dist = self.pos.distance_to(target_pos);
        // Tolerance multiplier matches the GDScript `melee_range * 1.2`
        // hysteresis: avoids flapping Chase↔Attack at the boundary.
        if dist > self.melee_range() * 1.2 {
            self.transition(EnemyState::Chase, now);
            return;
        }
        self.face_toward(target_pos);
        let due = match self.last_attack_at {
            None => true,
            Some(t) => now.duration_since(t).as_secs_f32() >= self.attack_interval(),
        };
        if due {
            self.last_attack_at = Some(now);
            events.hit = Some(HitIntent {
                target: target_id,
                amount: self.melee_swing_damage(),
            });
        }
    }

    /// Its target is gone (dead, ported, out of the world, or past its
    /// leash): EQ's hate list. It turns on the present attacker with the most
    /// hate within its leash and chases them; only with nobody left does it
    /// walk home (where `tick_leash` heals it). Before, it walked home with
    /// the rest of the group still on it, and since the evade (a mob walking
    /// home takes no damage) that walk would have healed it out from under
    /// them.
    fn next_hater_or_home(&mut self, targets: &[(EntityId, Vec3f)], now: Instant) {
        self.target = self.best_hater_within_leash(targets);
        if self.target.is_some() {
            self.transition(EnemyState::Chase, now);
        } else {
            self.transition(EnemyState::Leash, now);
        }
    }

    /// Walks a leashing mob home; true on the tick it arrives and heals.
    fn tick_leash(
        &mut self,
        dt: f32,
        now: Instant,
        terrain: &crate::world::terrain::Terrain,
    ) -> bool {
        let dist = self.pos.distance_to(self.spawn_pos);
        if dist < LEASH_HOME_TOLERANCE {
            self.hp = self.max_hp;
            // Track 12 Piece A2 — clear threat AND aggro on leash
            // home. Returning to spawn is a fight reset; existing
            // attackers shouldn't carry over residual threat into
            // the next engagement.
            self.threat.clear();
            self.aggro.clear();
            // A DoT cast on the way home would otherwise tick on an idle
            // mob that cannot acquire its caster (spell batch step 5).
            self.active_dots.clear();
            self.transition(EnemyState::Idle, now);
            return true;
        }
        let step = self.enraged_speed() * dt;
        self.face_toward(self.spawn_pos);
        let moved = self.step_on(terrain, self.pos.step_toward(self.spawn_pos, step));
        let dist = self.pos.distance_to(self.spawn_pos);
        if self.stuck_too_long(moved, None, dist, now) {
            // Home is up a face it came down: EQ's mobs warp home. The home
            // check next tick heals it and resets the fight.
            self.pos = terrain.ground_vec(self.spawn_pos);
            self.stuck = None;
        }
        false
    }

    /// Track 12 Piece A2 — pet-induced threat redirect. If any
    /// attacker in `threat` has accumulated damage >= the current
    /// target's by THREAT_SWITCH_MULT (1.3×), and that attacker is
    /// still in the targets slice (alive + visible), switch the
    /// enemy onto them. Skips when there's no current target. Skips
    /// when the highest threat IS the current target.
    fn maybe_switch_target_by_threat(&mut self, targets: &[(EntityId, Vec3f)]) {
        const THREAT_SWITCH_MULT: f32 = 1.3;
        let Some(current) = self.target else { return };
        let current_threat = self.threat.get(&current).copied().unwrap_or(0.0);
        let mut best: Option<(EntityId, f32)> = None;
        for (&id, &threat) in &self.threat {
            if id == current {
                continue;
            }
            if !targets.iter().any(|(t, _)| *t == id) {
                continue;
            }
            match best {
                None => best = Some((id, threat)),
                Some((_, b)) if threat > b => best = Some((id, threat)),
                _ => {}
            }
        }
        if let Some((top_id, top_threat)) = best {
            if top_threat >= current_threat * THREAT_SWITCH_MULT && top_threat > 0.0 {
                self.target = Some(top_id);
            }
        }
    }

    // ── Wandering (roam.rs) ──────────────────────────────────────────────

    /// A wanderer's fight began here: where it stands becomes its home, the
    /// place it walks back to and heals at when the fight is over, and its
    /// walk is forgotten. So a leash is a short walk back to where it was
    /// roaming, not a trek to where it first appeared. Called when it picks a
    /// target while idle (its own scan, or a packmate's fight), never when a
    /// charm ends (`turn_on`): a home in town, where a charmer let it go,
    /// would put it at the vendors for good. Only ground on the roam map
    /// becomes home, so home is always somewhere it can walk on from.
    pub fn roam_fight_began(&mut self, map: &RoamMap) {
        let Some(roam) = self.roam else {
            return;
        };
        if map.roamable(self.pos.x, self.pos.z) {
            self.spawn_pos = self.pos;
        }
        self.roam = Some(roam.rested());
    }

    /// Pack social aggro: an idle packmate takes a member's target as its own
    /// (on its own hate list, so its kill pays and answers as any fight's
    /// does) and its fight begins here. The caller has checked that its own
    /// leash reaches the target (`tick_chase` drops one past it at once) and
    /// that the target is not hidden.
    pub fn join_pack_fight(&mut self, target: EntityId, map: &RoamMap, now: Instant) {
        self.threat.entry(target).or_insert(roam::PACK_JOIN_THREAT);
        self.target = Some(target);
        self.roam_fight_began(map);
        self.transition(EnemyState::Chase, now);
    }

    /// One tick of a wanderer's walk, run after the AI step while it is still
    /// idle (so it has already looked for a target this tick, exactly as a
    /// standing mob does, and roaming is never the `Leash` state: a wanderer
    /// on its legs can be hit). It stands a while, picks a point in its
    /// region, ambles there, and stands again. `leader` is its pack leader's
    /// position when it is a follower; a follower keeps to its leader instead
    /// of picking legs of its own.
    ///
    /// Every step ends on the roam map (floors it can walk back from), so a
    /// wanderer never walks onto a face or down one; a step that would is not
    /// taken and the leg is dropped. Root holds it, snare slows it.
    pub fn tick_roam(
        &mut self,
        map: &RoamMap,
        leader: Option<Vec3f>,
        dt: f32,
        now: Instant,
        terrain: &crate::world::terrain::Terrain,
        rng: &mut impl rand::Rng,
    ) {
        let Some(mut walk) = self.roam else {
            return;
        };
        if self.is_pet() || self.state != EnemyState::Idle || self.is_mezzed() || self.is_stunned()
        {
            return;
        }
        // Idle off the roam map (within the metre a walk home stops short of
        // home, or where a charm ended beside it): step onto home itself,
        // which is on the map, so the walk below starts from ground it can
        // leave.
        if !map.roamable(self.pos.x, self.pos.z) {
            self.pos = terrain.ground_vec(self.spawn_pos);
            walk = walk.rested();
        }
        if self.is_rooted() {
            self.roam = Some(walk);
            return;
        }
        let run = self.enraged_speed() * (1.0 - self.snare_factor()) * dt;
        let amble = run * roam::ROAM_SPEED_MULT;

        if let Some(leader_pos) = leader {
            if walk.solo_until.is_none_or(|until| now >= until) {
                walk = walk.rested();
                let dist = self.pos.xz_distance_to(leader_pos);
                if dist > roam::PACK_FOLLOW_DISTANCE {
                    let step = if dist > roam::PACK_CATCHUP_DISTANCE {
                        run
                    } else {
                        amble
                    };
                    let to = self.pos.step_toward(leader_pos, step);
                    // Straight to the leader, unless that line cuts the town
                    // or the next step leaves the map: then it walks its own
                    // legs for a while and tries again.
                    if roam::segment_clears_town(
                        (self.pos.x, self.pos.z),
                        (leader_pos.x, leader_pos.z),
                    ) && self.roam_step(map, terrain, to)
                    {
                        self.face_toward(leader_pos);
                    } else {
                        walk.solo_until = Some(now + secs(roam::PACK_SOLO_SECS));
                    }
                }
                self.roam = Some(walk);
                return;
            }
        }

        if let Some(until) = walk.pause_until {
            if now < until {
                self.roam = Some(walk);
                return;
            }
            walk.pause_until = None;
        }
        let pause = |rng: &mut _| {
            Some(
                now + secs(rand::Rng::gen_range(
                    rng,
                    roam::ROAM_PAUSE_MIN..=roam::ROAM_PAUSE_MAX,
                )),
            )
        };
        let goal = match walk.goal {
            Some(goal) => goal,
            None => match roam::pick_leg(map, &walk.region, self.pos, rng) {
                Some(goal) => goal,
                None => {
                    walk.pause_until = pause(rng);
                    self.roam = Some(walk);
                    return;
                }
            },
        };
        let goal_pos = Vec3f {
            x: goal.0,
            y: self.pos.y,
            z: goal.1,
        };
        self.face_toward(goal_pos);
        let to = self.pos.step_toward(goal_pos, amble);
        if !self.roam_step(map, terrain, to) {
            // A face in the way. The same step would be refused next tick
            // too, so the leg is over now; no clock runs.
            walk.goal = None;
            walk.pause_until = pause(rng);
        } else if self.pos.xz_distance_to(goal_pos) < LEASH_HOME_TOLERANCE {
            walk.goal = None;
            walk.pause_until = pause(rng);
        } else {
            walk.goal = Some(goal);
        }
        self.roam = Some(walk);
    }

    /// One roam step to `to`, taken only onto the roam map (and never against
    /// the slope rule, which the map's floors satisfy anyway), and turned
    /// aside by what stands on the ground as any step is: an ambling animal
    /// slides round a trunk. Returns whether it moved; a step held to less
    /// than `DETOUR_BELOW` of its way (a wall, a rock square in front) does
    /// not, which ends the leg. A refusal here is not a player's and never
    /// counts on the `slope_refusals` health line.
    fn roam_step(
        &mut self,
        map: &RoamMap,
        terrain: &crate::world::terrain::Terrain,
        to: Vec3f,
    ) -> bool {
        let from = P2::of(self.pos);
        let r = terrain.obstacles().resolve(from, P2::of(to), BODY_RADIUS);
        let got = P2::new(r.x, r.z);
        if r.deflected && from.dist_to(got) < from.dist_to(P2::of(to)) * crate::world::DETOUR_BELOW
        {
            return false;
        }
        let to = Vec3f {
            x: got.x,
            y: to.y,
            z: got.z,
        };
        if !map.roamable(to.x, to.z) || !terrain.slope_ok(self.pos, to) {
            return false;
        }
        self.pos = terrain.ground_vec(to);
        true
    }

    fn face_toward(&mut self, target_pos: Vec3f) {
        let dx = target_pos.x - self.pos.x;
        let dz = target_pos.z - self.pos.z;
        // Skip nearly-coincident points to avoid yaw flutter at the threshold.
        if dx * dx + dz * dz < 0.0001 {
            return;
        }
        self.yaw = dx.atan2(dz);
    }
}

/// The nearest target inside `radius` that can be seen from `origin` across
/// the terrain (EQEmu's `CheckWillAggro`: an idle mob notices only what it has
/// line of sight to; once provoked, the threat table finds the attacker
/// without it), passing over the `hidden` ones.
fn nearest_target_within(
    origin: Vec3f,
    targets: &[(EntityId, Vec3f)],
    radius: f32,
    hidden: &[EntityId],
    terrain: &crate::world::terrain::Terrain,
) -> Option<(EntityId, f32)> {
    let mut best: Option<(EntityId, f32)> = None;
    for &(id, pos) in targets {
        if hidden.contains(&id) {
            continue;
        }
        let d = origin.distance_to(pos);
        if d > radius {
            continue;
        }
        if !terrain.can_see(origin, pos) {
            continue;
        }
        match best {
            None => best = Some((id, d)),
            Some((_, b)) if d < b => best = Some((id, d)),
            _ => {}
        }
    }
    best
}

fn target_pos(targets: &[(EntityId, Vec3f)], id: EntityId) -> Option<Vec3f> {
    targets.iter().find(|(p, _)| *p == id).map(|(_, pos)| *pos)
}

fn secs(s: f32) -> std::time::Duration {
    std::time::Duration::from_secs_f32(s)
}

/// How close to its spawn position a leashing mob must get before it counts
/// as home (and resets: full HP, threat and aggro cleared, back to Idle).
pub const LEASH_HOME_TOLERANCE: f32 = 1.0;

/// Monotonic enemy-id counter. Starts at `ENEMY_ID_BASE` and increments
/// for each spawn — never reused even after death, so a stale client
/// reference can be detected unambiguously. u64 wraparound at 2^64 is
/// not a practical concern.
static NEXT_ENEMY_ID: AtomicU64 = AtomicU64::new(ENEMY_ID_BASE);

pub fn mint_enemy_id() -> EntityId {
    NEXT_ENEMY_ID.fetch_add(1, Ordering::Relaxed)
}

/// Track 11 — monotonic pet-id counter, partitioned above bags. Same
/// never-reused semantics as enemy ids.
static NEXT_PET_ID: AtomicU64 = AtomicU64::new(PET_ID_BASE);

pub fn mint_pet_id() -> EntityId {
    NEXT_PET_ID.fetch_add(1, Ordering::Relaxed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use protocol::world::IdKind;
    use std::time::Duration;

    fn template() -> MobTemplate {
        MobTemplate {
            name: "Test".into(),
            level: 1,
            hp: 50.0,
            dmg: 5,
            xp: 10,
            speed: 2.5,
            aggro: 10.0,
            leash: None,
            melee_range: None,
            attack_interval: None,
            named_id: None,
            faction: None,
            alignment: None,
            faction_hit: None,
        }
    }

    fn named_template(id: &str) -> MobTemplate {
        MobTemplate {
            named_id: Some(id.into()),
            ..template()
        }
    }

    // ── Position broadcast gate ───────────────────────────────────────────

    fn idle_entity() -> Entity {
        Entity::from_spawn(
            0,
            Vec3f {
                x: 10.0,
                y: 0.0,
                z: 5.0,
            },
            template(),
            Instant::now(),
        )
    }

    #[test]
    fn position_broadcast_never_sent_is_due() {
        assert!(idle_entity().position_broadcast_due(Instant::now()));
    }

    #[test]
    fn position_broadcast_unchanged_inside_the_gap_is_not_due() {
        let mut e = idle_entity();
        let t0 = Instant::now();
        e.mark_position_broadcast(t0);
        assert!(!e.position_broadcast_due(t0 + Duration::from_millis(100)));
    }

    #[test]
    fn position_broadcast_after_moving_is_due() {
        let mut e = idle_entity();
        let t0 = Instant::now();
        e.mark_position_broadcast(t0);
        e.pos.x += 0.1;
        assert!(e.position_broadcast_due(t0 + Duration::from_millis(50)));
    }

    #[test]
    fn position_broadcast_after_turning_is_due() {
        let mut e = idle_entity();
        let t0 = Instant::now();
        e.mark_position_broadcast(t0);
        e.yaw += 0.5;
        assert!(e.position_broadcast_due(t0 + Duration::from_millis(50)));
    }

    #[test]
    fn position_broadcast_epsilon_nudge_is_not_a_move() {
        let mut e = idle_entity();
        let t0 = Instant::now();
        e.mark_position_broadcast(t0);
        e.pos.x += 0.001;
        assert!(!e.position_broadcast_due(t0 + Duration::from_millis(50)));
    }

    #[test]
    fn position_broadcast_keepalive_fires_after_the_gap() {
        let mut e = idle_entity();
        let t0 = Instant::now();
        e.mark_position_broadcast(t0);
        assert!(e.position_broadcast_due(t0 + crate::world::ENEMY_POSITION_KEEPALIVE));
    }

    #[test]
    fn mark_position_broadcast_advances_the_sequence_and_resets_the_gate() {
        let mut e = idle_entity();
        let t0 = Instant::now();
        e.pos.x += 1.0;
        let before = e.seq;
        e.mark_position_broadcast(t0);
        assert_eq!(e.seq, before + 1);
        assert!(!e.position_broadcast_due(t0 + Duration::from_millis(10)));
    }

    /// A tagged mob is scaled once, at the single spawn choke point, against
    /// whatever template it spawned from. Rotfang is hp x3.5, dmg x1.8, lvl 6.
    #[test]
    fn named_mob_is_scaled_at_spawn() {
        let e = Entity::from_spawn(0, Vec3f::ZERO, named_template("rotfang"), Instant::now());
        assert_eq!(
            e.mob.name, "Rotfang the Feared",
            "nameplate uses full_name()"
        );
        assert_eq!(e.mob.level, 6, "level is overridden, not multiplied");
        assert_eq!(e.max_hp, 50.0 * 3.5);
        assert_eq!(e.hp, e.max_hp, "spawns at full health");
        assert_eq!(e.mob.dmg, 9, "5 * 1.8 = 9");
        assert!(e.enrage.is_none(), "armed but not fired at spawn");
    }

    /// An untagged mob, or one with an unknown id, must behave exactly as
    /// before — this is the safety property that keeps every existing camp
    /// mob unchanged.
    #[test]
    fn untagged_and_unknown_mobs_are_untouched() {
        let now = Instant::now();
        let plain = Entity::from_spawn(0, Vec3f::ZERO, template(), now);
        assert_eq!(plain.mob.name, "Test");
        assert_eq!(plain.max_hp, 50.0);
        assert_eq!(plain.mob.dmg, 5);

        let bogus = Entity::from_spawn(0, Vec3f::ZERO, named_template("no_such_mob"), now);
        assert_eq!(bogus.mob.name, "Test", "unknown id falls back to ordinary");
        assert_eq!(bogus.max_hp, 50.0);
    }

    /// Enrage fires once, below the threshold, and then stays on. It boosts
    /// both damage and speed.
    #[test]
    fn enrage_fires_once_below_the_threshold_and_persists() {
        let named = crate::world::named::lookup("rotfang").expect("rotfang");
        let mut e = Entity::from_spawn(0, Vec3f::ZERO, named_template("rotfang"), Instant::now());
        let calm_dmg = e.melee_swing_damage();
        let calm_speed = e.enraged_speed();

        // Above the 20% threshold: nothing happens.
        e.hp = e.max_hp * 0.5;
        assert!(!e.maybe_enrage(named));
        assert!(e.enrage.is_none());
        assert_eq!(e.melee_swing_damage(), calm_dmg);

        // Crossing it fires exactly once.
        e.hp = e.max_hp * 0.10;
        assert!(e.maybe_enrage(named), "should fire below 20%");
        assert!(!e.maybe_enrage(named), "must not re-fire");

        // And it is felt in both damage and speed.
        assert!(
            e.melee_swing_damage() > calm_dmg,
            "enraged damage {} should exceed calm {}",
            e.melee_swing_damage(),
            calm_dmg
        );
        assert!(
            e.enraged_speed() > calm_speed,
            "enraged speed should exceed calm"
        );

        // Healing back up does not calm it — enrage is a last stand.
        e.hp = e.max_hp;
        assert!(e.enrage.is_some(), "enrage must not wear off");
    }

    /// Sable has enrage disabled with threshold 0, and must never fire even
    /// at 1 HP.
    #[test]
    fn a_named_mob_with_enrage_disabled_never_fires() {
        let named = crate::world::named::lookup("sable").expect("sable");
        let mut e = Entity::from_spawn(0, Vec3f::ZERO, named_template("sable"), Instant::now());
        e.hp = 1.0;
        assert!(!e.maybe_enrage(named));
        assert!(e.enrage.is_none());
    }

    #[test]
    fn melee_swing_is_mob_dmg_when_unbuffed() {
        let now = Instant::now();
        // World enemy: zero stats → delta 0 → exactly mob.dmg.
        let enemy = Entity::from_spawn(0, Vec3f::ZERO, template(), now);
        assert_eq!(enemy.melee_swing_damage(), template().dmg);
        // Pet: level-derived baseline, but unbuffed (stats == base_stats)
        // → still exactly mob.dmg, so no rebalance of existing pets.
        let pet = Entity::from_pet_summon(1, Vec3f::ZERO, template(), now);
        assert_eq!(pet.stats.strength, pet.base_stats.strength);
        assert_eq!(pet.melee_swing_damage(), template().dmg);
    }

    #[test]
    fn str_buff_adds_one_fifth_to_pet_melee() {
        let now = Instant::now();
        let mut pet = Entity::from_pet_summon(1, Vec3f::ZERO, template(), now);
        // +12 STR (a Strength buff) over the baseline → +2 melee dmg,
        // matching the player formula (str/5).
        pet.stats.strength += 12;
        assert_eq!(pet.melee_swing_damage(), template().dmg + 12 / 5);
    }

    #[test]
    fn minted_ids_are_partitioned_above_player_range() {
        let a = mint_enemy_id();
        let b = mint_enemy_id();
        assert_eq!(IdKind::of(a), IdKind::Enemy);
        assert!(b > a);
    }

    #[test]
    fn transition_updates_state_and_timestamp() {
        let now = Instant::now();
        let mut e = Entity::from_spawn(0, Vec3f::ZERO, template(), now);
        assert_eq!(e.state, EnemyState::Idle);
        let later = now + std::time::Duration::from_millis(100);
        e.transition(EnemyState::Chase, later);
        assert_eq!(e.state, EnemyState::Chase);
        assert_eq!(e.state_entered_at, later);
    }

    #[test]
    fn transition_to_same_state_is_a_noop() {
        let now = Instant::now();
        let mut e = Entity::from_spawn(0, Vec3f::ZERO, template(), now);
        let later = now + std::time::Duration::from_millis(100);
        e.transition(EnemyState::Idle, later);
        assert_eq!(e.state_entered_at, now, "state_entered_at must not refresh");
    }

    /// Track 11.4 — enemy targets pets as valid aggro candidates.
    /// When the targets slice contains only a pet id (no player), the
    /// enemy's tick_idle should still pick it up and transition to
    /// Chase. Verifies the rename from `players` to `targets` actually
    /// extended the aggro pool to pets.
    #[test]
    fn enemy_aggros_on_pet_when_no_player_nearby() {
        let now = Instant::now();
        let mut e = Entity::from_spawn(0, Vec3f::ZERO, template(), now);
        // Pet id sits in the dedicated partition; the value doesn't
        // matter for aggro logic (tick_idle picks by distance, not id).
        let pet_id = PET_ID_BASE + 17;
        let targets = vec![(
            pet_id,
            Vec3f {
                x: 2.0,
                y: 0.0,
                z: 0.0,
            },
        )];
        let enemy_targets: Vec<(EntityId, Vec3f, bool)> = vec![];
        let _events = e.tick_ai(
            &targets,
            &enemy_targets,
            &[],
            0.05,
            now,
            crate::world::terrain::compiled(),
        );
        assert_eq!(
            e.target,
            Some(pet_id),
            "enemy should aggro on nearest target regardless of id partition"
        );
        assert!(
            matches!(e.state, EnemyState::Chase),
            "transitioning Idle → Chase on acquisition"
        );
    }

    /// Playtest 2026-09-22 — being attacked IS aggro. An idle mob whose
    /// threat table names an attacker outside the aggro radius (a ranged
    /// attacker) must acquire them, as long as they are within leash range.
    #[test]
    fn idle_enemy_acquires_ranged_attacker_via_threat() {
        let now = Instant::now();
        let mut e = Entity::from_spawn(0, Vec3f::ZERO, template(), now);
        let attacker: EntityId = 1;
        // Just past the aggro radius (template aggro is well under this),
        // inside leash (aggro * 2 by default).
        let pos = Vec3f {
            x: e.mob.aggro + 2.0,
            y: 0.0,
            z: 0.0,
        };
        assert!(pos.x <= e.leash_range(), "test setup: inside leash");
        e.threat.insert(attacker, 12.0);
        let targets = vec![(attacker, pos)];
        let enemy_targets: Vec<(EntityId, Vec3f, bool)> = vec![];
        let _ = e.tick_ai(
            &targets,
            &enemy_targets,
            &[],
            0.05,
            now,
            crate::world::terrain::compiled(),
        );
        assert_eq!(e.target, Some(attacker), "damage must provoke the mob");
        assert!(matches!(e.state, EnemyState::Chase));
    }

    /// Hide (active skills, 2026-10-19): an idle mob's proximity scan passes
    /// over a hidden player, but its threat table still finds them (being
    /// attacked is aggro, hidden or not).
    #[test]
    fn an_idle_mob_does_not_notice_a_hidden_player_but_still_feels_a_hit() {
        let now = Instant::now();
        let mut e = Entity::from_spawn(0, Vec3f::ZERO, template(), now);
        let rogue: EntityId = 7;
        let targets = vec![(
            rogue,
            Vec3f {
                x: 2.0,
                y: 0.0,
                z: 0.0,
            },
        )];
        let enemy_targets: Vec<(EntityId, Vec3f, bool)> = vec![];
        let _ = e.tick_ai(
            &targets,
            &enemy_targets,
            &[rogue],
            0.05,
            now,
            crate::world::terrain::compiled(),
        );
        assert_eq!(
            e.target, None,
            "a hidden rogue two metres away goes unnoticed"
        );
        assert!(matches!(e.state, EnemyState::Idle));
        e.threat.insert(rogue, 5.0);
        let _ = e.tick_ai(
            &targets,
            &enemy_targets,
            &[rogue],
            0.05,
            now,
            crate::world::terrain::compiled(),
        );
        assert_eq!(
            e.target,
            Some(rogue),
            "a hit from the shadows still provokes the mob"
        );
    }

    /// Shield Bash's stun freezes the state machine like mez, and taking
    /// damage does not end it (only mez breaks on damage).
    #[test]
    fn a_stunned_mob_neither_moves_nor_swings_and_a_hit_does_not_wake_it() {
        let now = Instant::now();
        let mut e = Entity::from_spawn(0, Vec3f::ZERO, template(), now);
        let player: EntityId = 1;
        let targets = vec![(
            player,
            Vec3f {
                x: 1.0,
                y: 0.0,
                z: 0.0,
            },
        )];
        let enemy_targets: Vec<(EntityId, Vec3f, bool)> = vec![];
        let _ = e.tick_ai(
            &targets,
            &enemy_targets,
            &[],
            0.05,
            now,
            crate::world::terrain::compiled(),
        );
        assert_eq!(e.target, Some(player));
        e.apply_cc(ActiveCc::new_stun(3.0));
        e.clear_mez();
        assert!(e.is_stunned(), "damage clears mez, never a stun");
        assert!(
            e.status_pairs().iter().any(|(n, _)| n == "Stunned"),
            "the frame says so"
        );
        let mut swung = false;
        for _ in 0..55 {
            let ev = e.tick_ai(
                &targets,
                &enemy_targets,
                &[],
                0.05,
                now,
                crate::world::terrain::compiled(),
            );
            swung |= ev.hit.is_some();
        }
        assert!(!swung, "2.75 s of ticks inside the 3 s stun: no swing");
        assert!(e.pos.distance_to(Vec3f::ZERO) < 1e-3, "and it did not move");
        let mut swung_after = false;
        for _ in 0..30 {
            let ev = e.tick_ai(
                &targets,
                &enemy_targets,
                &[],
                0.05,
                now,
                crate::world::terrain::compiled(),
            );
            swung_after |= ev.hit.is_some();
        }
        assert!(swung_after, "once the stun runs out the mob swings again");
        assert!(!e.is_stunned());
    }

    /// A small aggro radius used to mean a short leash: 8 m aggro, 16 m leash,
    /// nukable from 20 m with no response. The floor closes that for every mob
    /// that aggros at all and leaves a dummy with no aggro passive.
    #[test]
    fn leash_never_falls_short_of_spell_reach() {
        let now = Instant::now();
        let mut small = template();
        small.aggro = 8.0;
        small.leash = None;
        let e = Entity::from_spawn(0, Vec3f::ZERO, small, now);
        assert!((e.leash_range() - super::super::MIN_LEASH_RANGE).abs() < 1e-6);
        // Both are constants, so this one is checked at compile time.
        const {
            assert!(
                super::super::MIN_LEASH_RANGE > super::super::RANGED_ATTACK_RANGE,
                "the floor must sit past bow reach or the hole stays open"
            )
        };
        for spell in super::super::spells::all() {
            assert!(
                super::super::MIN_LEASH_RANGE > spell.reach(),
                "{} reaches {} m, past the leash floor: a mob could be hit from beyond its own leash",
                spell.name,
                spell.reach()
            );
        }
        let mut wide = template();
        wide.aggro = 20.0;
        wide.leash = None;
        let e = Entity::from_spawn(0, Vec3f::ZERO, wide, now);
        assert!(
            (e.leash_range() - 40.0).abs() < 1e-6,
            "a long leash is untouched"
        );
        let mut passive = template();
        passive.aggro = 0.0;
        passive.leash = None;
        let e = Entity::from_spawn(0, Vec3f::ZERO, passive, now);
        assert_eq!(
            e.leash_range(),
            0.0,
            "a mob that never aggros stays passive"
        );
    }

    /// The other half of the same fix: an attacker beyond leash range is NOT
    /// acquired from idle — tick_chase would drop them on its first tick, so
    /// acquiring would only flap Idle -> Chase -> Leash forever.
    #[test]
    fn idle_enemy_ignores_threat_beyond_leash() {
        let now = Instant::now();
        let mut e = Entity::from_spawn(0, Vec3f::ZERO, template(), now);
        let attacker: EntityId = 1;
        let pos = Vec3f {
            x: e.leash_range() + 5.0,
            y: 0.0,
            z: 0.0,
        };
        e.threat.insert(attacker, 12.0);
        let targets = vec![(attacker, pos)];
        let enemy_targets: Vec<(EntityId, Vec3f, bool)> = vec![];
        let _ = e.tick_ai(
            &targets,
            &enemy_targets,
            &[],
            0.05,
            now,
            crate::world::terrain::compiled(),
        );
        assert_eq!(e.target, None, "beyond leash: stand and wait, don't flap");
        assert!(matches!(e.state, EnemyState::Idle));
    }

    /// A charm ending hands the mob back as it was: same camp slot, same
    /// home, current HP, a fresh enemy id, and a named mob scaled once, not
    /// twice. Turned on its charmer it chases with threat, not aggro.
    #[test]
    fn released_from_charm_returns_the_same_mob_scaled_once() {
        let now = Instant::now();
        let home = Vec3f {
            x: 10.0,
            y: 0.0,
            z: -4.0,
        };
        let original = Entity::from_spawn(7, home, named_template("rotfang"), now);
        let scaled_dmg = original.mob.dmg;
        let scaled_hp = original.max_hp;
        // The charm arm's conversion: the scaled template rides onto the pet.
        let mut pet = Entity::from_pet_summon(42, original.pos, original.mob.clone(), now);
        pet.spawn_point_idx = original.spawn_point_idx;
        pet.spawn_pos = original.spawn_pos;
        pet.max_hp = original.max_hp;
        pet.hp = original.hp * 0.5;
        pet.pos = Vec3f {
            x: 12.0,
            y: 0.0,
            z: -1.0,
        };
        pet.charm_expires_at = Some(now);

        let mut back = Entity::released_from_charm(&pet, now);
        assert_eq!(IdKind::of(back.id), IdKind::Enemy, "an enemy id again");
        assert_ne!(back.id, original.id, "ids are never reused");
        assert_eq!(back.spawn_point_idx, 7, "the camp slot it still occupies");
        let close = |a: Vec3f, b: Vec3f| (a.x - b.x).abs() < 1e-6 && (a.z - b.z).abs() < 1e-6;
        assert!(close(back.spawn_pos, home), "home for the leash");
        assert!(close(back.pos, pet.pos), "it stands where the pet stood");
        assert!((back.hp - pet.hp).abs() < 1e-6, "the HP it had as a pet");
        assert!((back.max_hp - scaled_hp).abs() < 1e-6);
        assert_eq!(
            back.mob.dmg, scaled_dmg,
            "named damage multiplied once, not again"
        );
        assert_eq!(back.mob.named_id.as_deref(), Some("rotfang"));
        assert!(!back.is_pet() && back.charm_expires_at.is_none());
        assert_eq!(back.target, None, "hostility is the caller's call");

        back.turn_on(42, now);
        assert_eq!(back.target, Some(42));
        assert!(matches!(back.state, EnemyState::Chase));
        assert!(back.threat.get(&42).copied().unwrap_or(0.0) > 0.0);
        assert!(back.aggro.is_empty(), "no damage dealt, no aggro ledger");

        // Nobody to hate: it walks home from where the pet stood (3.6 m off),
        // and a mob already at home just stands.
        let mut orphan = Entity::released_from_charm(&pet, now);
        orphan.go_home(now);
        assert!(
            matches!(orphan.state, EnemyState::Leash),
            "away from home: walk back"
        );
        assert_eq!(orphan.target, None);
        pet.pos = home;
        let mut at_home = Entity::released_from_charm(&pet, now);
        at_home.go_home(now);
        assert!(
            matches!(at_home.state, EnemyState::Idle),
            "already home: stay"
        );
    }

    /// One attack slow at a time, strongest wins: a weaker slow never
    /// replaces a stronger one that still holds; a stronger one replaces a
    /// weaker; an equal one refreshes. Other kinds are untouched.
    #[test]
    fn a_weaker_attack_slow_never_replaces_a_stronger_one() {
        let now = Instant::now();
        let mut e = Entity::from_spawn(0, Vec3f::ZERO, template(), now);
        assert_eq!(e.attack_slow_pct(), 0);
        e.apply_cc(ActiveCc::new_attack_slow(0.10, 60.0)); // Torpor
        assert_eq!(e.attack_slow_pct(), 10);
        e.apply_cc(ActiveCc::new_attack_slow(0.05, 30.0)); // Slow, weaker
        assert_eq!(e.attack_slow_pct(), 10, "the weaker slow is ignored");
        assert_eq!(e.active_cc.len(), 1, "and never stacks beside it");
        e.apply_cc(ActiveCc::new_attack_slow(0.10, 5.0)); // equal: refresh
        assert!(
            (e.active_cc[0].remaining - 5.0).abs() < 1e-6,
            "an equal slow refreshes"
        );
        e.apply_cc(ActiveCc::new_attack_slow(0.35, 10.0)); // stronger
        assert_eq!(e.attack_slow_pct(), 35, "a stronger slow takes over");
        e.apply_cc(ActiveCc::new_root(3.0));
        assert_eq!(e.active_cc.len(), 2, "a root sits beside the slow");
    }

    /// Spell batch step 5: a DoT deals whole ticks of dps x 3 every three
    /// seconds, a re-cast refreshes rather than stacks, the last partial
    /// tick is never dealt, a tick breaks mez, and the DoTs end the moment
    /// the mob walks home or their caster leaves.
    #[test]
    fn a_dot_ticks_every_three_seconds_refreshes_and_ends_with_the_leash() {
        let now = Instant::now();
        let mut e = Entity::from_spawn(0, Vec3f::ZERO, template(), now);
        let dot = |dps: f32, dur: f32| ActiveDot {
            name: "Entangle".into(),
            caster: 1,
            dps,
            dmg_type: protocol::world::DamageType::Physical,
            remaining: dur,
            tick_acc: 0.0,
            ticks_left: 0,
        };
        e.apply_dot(dot(5.0, 7.0));
        assert!(e.tick_dots(1.0).is_empty(), "no tick in the first second");
        assert!(e.tick_dots(1.9).is_empty(), "nor at 2.9 s");
        let t = e.tick_dots(0.1);
        assert_eq!(t.len(), 1, "one tick at 3 s");
        assert_eq!(t[0].amount, 15, "dps x 3");
        assert_eq!(t[0].caster, 1);
        e.apply_dot(dot(5.0, 7.0));
        assert_eq!(e.active_dots.len(), 1, "a re-cast refreshes, never stacks");
        assert_eq!(
            e.tick_dots(3.0).len(),
            1,
            "the refreshed dot ticks three seconds in"
        );
        assert_eq!(e.tick_dots(3.0).len(), 1, "and again at six");
        assert!(
            e.tick_dots(1.0).is_empty(),
            "seven seconds: no partial tick"
        );
        assert!(e.active_dots.is_empty(), "and the dot is gone");
        e.apply_cc(ActiveCc::new_mez(10.0));
        e.apply_dot(dot(5.0, 30.0));
        assert!(!e.is_mezzed(), "a dot landing breaks mez like any damage");
        e.transition(EnemyState::Leash, now);
        assert!(
            e.active_dots.is_empty(),
            "a mob walking home sheds its dots"
        );
        e.apply_dot(dot(5.0, 30.0));
        assert!(
            e.active_dots.is_empty(),
            "and takes none on the way (the review's walk-home farm)"
        );
        e.transition(EnemyState::Idle, now);
        e.apply_dot(dot(5.0, 30.0));
        e.clear_dots_from(1);
        assert!(
            e.active_dots.is_empty(),
            "the caster leaving ends their dots"
        );
    }

    /// At the real tick rate (20 Hz, dt 0.05) an 18 s Entangle deals exactly
    /// six ticks, the last one included: the count, not float drift between
    /// the two accumulators, decides when the DoT is spent.
    #[test]
    fn a_dot_at_the_real_tick_rate_deals_every_whole_tick() {
        let now = Instant::now();
        let mut e = Entity::from_spawn(0, Vec3f::ZERO, template(), now);
        e.apply_dot(ActiveDot {
            name: "Entangle".into(),
            caster: 1,
            dps: 5.0,
            dmg_type: protocol::world::DamageType::Physical,
            remaining: 18.0,
            tick_acc: 0.0,
            ticks_left: 0,
        });
        let mut dealt = 0;
        for _ in 0..(20 * 18 + 40) {
            dealt += e.tick_dots(0.05).len();
        }
        assert_eq!(dealt, 6, "18 s at 5 dps is six whole ticks");
        assert!(
            e.active_dots.is_empty(),
            "and the dot is gone once they are dealt"
        );
    }

    // ── Mobs at a face (the first hills) ─────────────────────────────────

    /// Three 10 m cells along x, centred on the origin: flat at 0 from
    /// x = -15 to -5, a face rising 20 m to x = 5 (63 degrees), a flat top at
    /// 20 m to x = 15.
    fn foot_and_top() -> crate::world::terrain::Terrain {
        let row = [0.0, 0.0, 20.0, 20.0];
        let heights: Vec<f32> = (0..4).flat_map(|_| row).collect();
        crate::world::terrain::Terrain::from_heights(3, 10.0, [0.0; 3], 0.0, heights).unwrap()
    }

    const TOP: Vec3f = Vec3f {
        x: 10.0,
        y: 20.0,
        z: 0.0,
    };

    /// Ticks `e` at 20 Hz for `secs`, checking it never climbs onto the face.
    fn tick_for(
        e: &mut Entity,
        targets: &[(EntityId, Vec3f)],
        enemy_targets: &[(EntityId, Vec3f, bool)],
        secs: f32,
        t: &crate::world::terrain::Terrain,
        start: Instant,
    ) -> Instant {
        let dt = 0.05;
        let mut now = start;
        for _ in 0..(secs / dt) as usize {
            now += Duration::from_millis(50);
            let _ = e.tick_ai(targets, enemy_targets, &[], dt, now, t);
            assert!(
                e.pos.x < -4.99 || e.pos.y > 19.99,
                "on the face at {:?} ({:?})",
                e.pos,
                e.state
            );
        }
        now
    }

    /// Ticks `e` until its first step is refused (it has reached the foot of
    /// the face), and returns that instant: the stuck clock starts there.
    fn until_stuck(
        e: &mut Entity,
        targets: &[(EntityId, Vec3f)],
        enemy_targets: &[(EntityId, Vec3f, bool)],
        t: &crate::world::terrain::Terrain,
        start: Instant,
    ) -> Instant {
        let mut now = start;
        for _ in 0..200 {
            now += Duration::from_millis(50);
            let _ = e.tick_ai(targets, enemy_targets, &[], 0.05, now, t);
            if e.stuck.is_some() {
                return now;
            }
        }
        panic!("never reached the face: {:?}", e.pos);
    }

    /// A flat plate, 80 m across, with `prisms` standing on it.
    fn ground_with(prisms: &[crate::world::obstacles::Prism]) -> crate::world::terrain::Terrain {
        crate::world::terrain::Terrain::flat(8, 10.0, 0.0).with_obstacles(std::sync::Arc::new(
            crate::world::obstacles::Obstacles::from_prisms(prisms).unwrap(),
        ))
    }

    fn on_plate(x: f32, z: f32) -> Vec3f {
        Vec3f { x, y: 0.0, z }
    }

    /// A mob set chasing `player` from `from`.
    fn chaser(from: Vec3f, player: EntityId, start: Instant) -> Entity {
        let mut e = Entity::from_spawn(0, from, template(), start);
        e.threat.insert(player, 10.0);
        e.target = Some(player);
        e.transition(EnemyState::Chase, start);
        e
    }

    /// A house stands square between a mob and its target. The mob is held
    /// at the wall, walks along it to its end, comes round and attacks; at
    /// no tick is it inside the house. Before obstacles it walked through
    /// the house; with obstacles and no way round, it would push into the
    /// wall until it gave up.
    #[test]
    fn a_mob_walks_round_a_house_to_its_target() {
        use crate::world::obstacles::Prism;
        let t = ground_with(&[Prism::rect(-5.0, -5.0, 5.0, 5.0, 0.0, 4.0)]);
        let start = Instant::now();
        let player: EntityId = 1;
        let mut e = chaser(on_plate(-12.0, 0.0), player, start);
        let targets = vec![(player, on_plate(12.0, 0.0))];
        let mut now = start;
        let mut arrived = None;
        for tick in 0..400 {
            now += Duration::from_millis(50);
            let _ = e.tick_ai(&targets, &[], &[], 0.05, now, &t);
            assert!(
                t.obstacles().is_free(P2::of(e.pos), BODY_RADIUS),
                "inside the house at {:?}",
                e.pos
            );
            if matches!(e.state, EnemyState::Attack) {
                arrived = Some(tick);
                break;
            }
        }
        assert!(
            arrived.is_some(),
            "never came round: {:?} at {:?}",
            e.state,
            e.pos
        );
        assert_eq!(e.target, Some(player));
    }

    /// A trunk on the way costs a mob a step aside and nothing else: it
    /// slides round, arrives, and is never evading on the way (a fight in a
    /// wood must not turn mobs immune at every tree).
    #[test]
    fn a_mob_slides_round_a_trunk_and_never_evades() {
        use crate::world::obstacles::Prism;
        let t = ground_with(&[Prism::round(0.0, 0.2, 0.3, 6, 0.0, 8.0)]);
        let start = Instant::now();
        let player: EntityId = 1;
        let mut e = chaser(on_plate(-8.0, 0.0), player, start);
        let targets = vec![(player, on_plate(8.0, 0.0))];
        let mut now = start;
        let mut arrived = false;
        for _ in 0..200 {
            now += Duration::from_millis(50);
            let _ = e.tick_ai(&targets, &[], &[], 0.05, now, &t);
            assert!(
                t.obstacles().is_free(P2::of(e.pos), BODY_RADIUS),
                "inside the trunk at {:?}",
                e.pos
            );
            assert!(!e.is_evading(now), "evading at a tree, at {:?}", e.pos);
            if matches!(e.state, EnemyState::Attack) {
                arrived = true;
                break;
            }
        }
        assert!(arrived, "never arrived: {:?} at {:?}", e.state, e.pos);
    }

    /// A wall lying at a shallow angle across a chase turns the mob aside,
    /// and the mob slides along it nearly as fast as it ran: that is not
    /// being held. Its stuck clock never starts on the slide, so a mob
    /// brushing past trees and walls after a fleeing player neither turns
    /// immune nor gives the chase up eight seconds later.
    #[test]
    fn a_mob_glancing_along_a_wall_is_not_held() {
        use crate::world::obstacles::{Obstacles, Prism, FLAG_SIGHT, FLAG_WALK};
        // 8.5 m of wall, 0.4 m thick, at twenty degrees to the chase.
        let wall = Prism {
            flags: FLAG_WALK | FLAG_SIGHT,
            base: 0.0,
            top: 3.0,
            sight_scale: 1.0,
            corners: vec![
                P2::new(-4.0, -0.5),
                P2::new(4.0, 2.4),
                P2::new(3.864, 2.776),
                P2::new(-4.136, -0.124),
            ],
        };
        let t = crate::world::terrain::Terrain::flat(8, 10.0, 0.0).with_obstacles(
            std::sync::Arc::new(Obstacles::from_prisms(&[wall]).unwrap()),
        );
        let start = Instant::now();
        let player: EntityId = 1;
        let mut e = chaser(on_plate(-8.0, 0.6), player, start);
        let targets = vec![(player, on_plate(9.0, 2.2))];
        let mut now = start;
        let mut slid = false;
        let mut arrived = false;
        for _ in 0..300 {
            now += Duration::from_millis(50);
            let before = e.pos;
            let _ = e.tick_ai(&targets, &[], &[], 0.05, now, &t);
            // Along the wall's face, short of its far end.
            if e.pos.x > -3.0 && e.pos.x < 3.0 {
                slid |= (e.pos.z - before.z) > 0.01;
                assert!(e.stuck.is_none(), "held on a slide at {:?}", e.pos);
                assert!(!e.is_evading(now));
            }
            if matches!(e.state, EnemyState::Attack) {
                arrived = true;
                break;
            }
        }
        assert!(slid, "the wall turned it aside");
        assert!(
            arrived,
            "and it came round the end: {:?} at {:?}",
            e.state, e.pos
        );
    }

    /// A mob in a dead end (three walls, its target beyond the closed one)
    /// has no way round: it is evading once the grace has passed, so nobody
    /// kills it through the wall, and gives up after `MOB_STUCK_SECS`.
    #[test]
    fn a_mob_in_a_dead_end_evades_and_gives_up() {
        use crate::world::obstacles::Prism;
        let t = ground_with(&[
            Prism::rect(4.0, -6.0, 5.0, 6.0, 0.0, 4.0),
            Prism::rect(-10.0, -6.0, 5.0, -5.0, 0.0, 4.0),
            Prism::rect(-10.0, 5.0, 5.0, 6.0, 0.0, 4.0),
        ]);
        let start = Instant::now();
        let player: EntityId = 1;
        let mut e = chaser(on_plate(-8.0, 0.0), player, start);
        let targets = vec![(player, on_plate(12.0, 0.0))];
        let mut now = start;
        // To the closed end: the clock starts at the first step not taken as aimed.
        while e.stuck.is_none() {
            now += Duration::from_millis(50);
            let _ = e.tick_ai(&targets, &[], &[], 0.05, now, &t);
            assert!(
                now.duration_since(start).as_secs_f32() < 10.0,
                "never reached the wall"
            );
        }
        let held_at = now;
        let mut evaded = false;
        while now.duration_since(held_at).as_secs_f32() < crate::world::MOB_STUCK_SECS - 0.5 {
            now += Duration::from_millis(50);
            let _ = e.tick_ai(&targets, &[], &[], 0.05, now, &t);
            assert!(e.pos.x < 4.0, "through the wall at {:?}", e.pos);
            if now.duration_since(held_at).as_secs_f32() > crate::world::EVADE_GRACE_SECS + 0.2 {
                assert!(e.is_evading(now), "not evading while held, at {:?}", e.pos);
                evaded = true;
            }
        }
        assert!(evaded);
        assert!(matches!(e.state, EnemyState::Chase), "still trying");
        for _ in 0..20 {
            now += Duration::from_millis(50);
            let _ = e.tick_ai(&targets, &[], &[], 0.05, now, &t);
        }
        assert!(
            matches!(e.state, EnemyState::Leash | EnemyState::Idle),
            "gave up: {:?}",
            e.state
        );
        assert_eq!(e.target, None);
    }

    #[test]
    fn a_mob_at_a_face_stops_and_gives_up() {
        let t = foot_and_top();
        let start = Instant::now();
        let player: EntityId = 1;
        let foot = Vec3f {
            x: -8.0,
            y: 0.0,
            z: 0.0,
        };
        let mut e = Entity::from_spawn(0, foot, template(), start);
        e.threat.insert(player, 10.0);
        e.target = Some(player);
        e.transition(EnemyState::Chase, start);
        let targets = vec![(player, TOP)];
        // Walk to the foot; the clock starts at the first refused step.
        let at_foot = until_stuck(&mut e, &targets, &[], &t, start);
        let now = tick_for(
            &mut e,
            &targets,
            &[],
            crate::world::MOB_STUCK_SECS - 0.5,
            &t,
            at_foot,
        );
        assert!(
            matches!(e.state, EnemyState::Chase),
            "still trying: {:?}",
            e.state
        );
        assert!(
            e.pos.x > -5.5,
            "it reached the foot of the face: {:?}",
            e.pos
        );
        tick_for(&mut e, &targets, &[], 1.0, &t, now);
        assert!(
            matches!(e.state, EnemyState::Leash | EnemyState::Idle),
            "gave up: {:?}",
            e.state
        );
        assert_eq!(e.target, None);
        assert!(e.threat.is_empty() && e.aggro.is_empty());

        // The control: the same chase on flat ground reaches the target.
        let flat = crate::world::terrain::Terrain::flat(3, 10.0, 0.0);
        let mut c = Entity::from_spawn(0, foot, template(), start);
        c.threat.insert(player, 10.0);
        c.target = Some(player);
        c.transition(EnemyState::Chase, start);
        let level = vec![(player, Vec3f { y: 0.0, ..TOP })];
        let mut now = start;
        for _ in 0..200 {
            now += Duration::from_millis(50);
            let _ = c.tick_ai(&level, &[], &[], 0.05, now, &flat);
        }
        assert!(matches!(c.state, EnemyState::Attack), "{:?}", c.state);
    }

    /// WoW's evade (`is_evading`, the user's call of 2026-11-05): a mob held
    /// below a face takes damage through the grace, then evades through the
    /// hold and the whole walk home, and fights again at home. One that is no
    /// longer being refused is not held; a pet never evades.
    #[test]
    fn a_mob_held_below_a_face_evades_until_it_is_home() {
        let t = foot_and_top();
        let start = Instant::now();
        let player: EntityId = 1;
        let foot = Vec3f {
            x: -8.0,
            y: 0.0,
            z: 0.0,
        };
        let mut e = Entity::from_spawn(0, foot, template(), start);
        e.threat.insert(player, 10.0);
        e.target = Some(player);
        e.transition(EnemyState::Chase, start);
        assert!(!e.is_evading(start), "a mob getting somewhere fights");
        let targets = vec![(player, TOP)];
        let at_foot = until_stuck(&mut e, &targets, &[], &t, start);
        let grace = Duration::from_secs_f32(crate::world::EVADE_GRACE_SECS);
        let mut now = at_foot;
        let mut leashed = false;
        for _ in 0..1000 {
            now += Duration::from_millis(50);
            let _ = e.tick_ai(&targets, &[], &[], 0.05, now, &t);
            match e.state {
                EnemyState::Chase if now.duration_since(at_foot) < grace => {
                    assert!(!e.is_evading(now), "inside the grace it takes damage");
                }
                EnemyState::Chase => {
                    assert!(e.is_evading(now), "held past the grace it evades");
                    // A mob the slope rule stopped refusing (it brushed the
                    // face, then moved freely) is not held.
                    let free = now
                        + Duration::from_secs_f32(crate::world::EVADE_REFUSAL_RECENT_SECS + 0.1);
                    assert!(!e.is_evading(free), "not refused lately: not held");
                }
                EnemyState::Leash => {
                    leashed = true;
                    assert!(e.is_evading(now), "it evades on the walk home");
                }
                _ => break,
            }
        }
        assert!(leashed, "it gave up and walked home");
        assert!(matches!(e.state, EnemyState::Idle), "{:?}", e.state);
        assert!(!e.is_evading(now), "home, it fights again");

        let mut pet = Entity::from_spawn(0, foot, template(), start);
        pet.owner = Some(9);
        pet.state = EnemyState::Leash;
        assert!(!pet.is_evading(now), "a pet never evades");
    }

    /// EQ's hate list (`next_hater_or_home`): when the tank dies, the mob
    /// turns on the healer who also has hate; when the healer dies too it
    /// walks home. A target that runs past the leash is forgotten the same
    /// way, its damage kept as kill credit.
    #[test]
    fn a_mob_whose_target_dies_turns_on_the_next_hater() {
        let t = crate::world::terrain::Terrain::flat(3, 10.0, 0.0);
        let start = Instant::now();
        let at = |x: f32| Vec3f { x, y: 0.0, z: 0.0 };
        let (tank, healer): (EntityId, EntityId) = (1, 2);
        let mut e = Entity::from_spawn(0, at(0.0), template(), start);
        e.threat.insert(tank, 50.0);
        e.threat.insert(healer, 10.0);
        e.aggro.insert(tank, 50.0);
        e.target = Some(tank);
        e.transition(EnemyState::Attack, start);
        let mut now = start + Duration::from_millis(50);

        // The tank dies: the death sweep (`wipe_hate`) drops its hate and
        // the mob's target, and a dead player leaves the AI's target list.
        e.threat.remove(&tank);
        e.aggro.remove(&tank);
        e.target = None;
        let healer_only = vec![(healer, at(6.0))];
        let _ = e.tick_ai(&healer_only, &[], &[], 0.05, now, &t);
        assert_eq!(e.target, Some(healer), "it turns on the healer");
        assert!(matches!(e.state, EnemyState::Chase), "{:?}", e.state);

        // The healer dies too: nobody left, it walks home.
        e.threat.remove(&healer);
        e.target = None;
        now += Duration::from_millis(50);
        let _ = e.tick_ai(&[], &[], &[], 0.05, now, &t);
        assert_eq!(e.target, None);
        assert!(matches!(e.state, EnemyState::Leash), "{:?}", e.state);

        // A target past the leash: forgotten (no hate, its damage kept) and
        // the mob turns on the attacker still in reach.
        let mut f = Entity::from_spawn(0, at(0.0), template(), start);
        f.threat.insert(tank, 50.0);
        f.threat.insert(healer, 10.0);
        f.aggro.insert(tank, 50.0);
        f.target = Some(tank);
        f.transition(EnemyState::Chase, start);
        let far = f.leash_range() + 5.0;
        let both = vec![(tank, at(far)), (healer, at(6.0))];
        let _ = f.tick_ai(&both, &[], &[], 0.05, now, &t);
        assert_eq!(f.target, Some(healer), "past the leash, the next hater");
        assert!(!f.threat.contains_key(&tank), "the runner's hate is gone");
        assert_eq!(f.aggro.get(&tank), Some(&50.0), "its kill credit stays");
    }

    #[test]
    fn a_pet_that_cannot_reach_its_owner_warps_to_them() {
        let t = foot_and_top();
        let start = Instant::now();
        let owner: EntityId = 1;
        let foot = Vec3f {
            x: -8.0,
            y: 0.0,
            z: 0.0,
        };
        let mut pet = Entity::from_pet_summon(owner, foot, template(), start);
        let targets = vec![(owner, TOP)];
        let at_foot = until_stuck(&mut pet, &targets, &[], &t, start);
        let now = tick_for(
            &mut pet,
            &targets,
            &[],
            crate::world::MOB_STUCK_SECS - 0.5,
            &t,
            at_foot,
        );
        assert!(pet.pos.x < -4.99, "held at the foot: {:?}", pet.pos);
        let _ = tick_for(&mut pet, &targets, &[], 1.0, &t, now);
        assert!(
            pet.pos.distance_to(TOP) < 1.0,
            "beside its owner on top: {:?}",
            pet.pos
        );
    }

    #[test]
    fn a_pet_drops_a_target_it_cannot_reach() {
        let t = foot_and_top();
        let start = Instant::now();
        let owner: EntityId = 1;
        let enemy: EntityId = ENEMY_ID_BASE + 5;
        let foot = Vec3f {
            x: -8.0,
            y: 0.0,
            z: 0.0,
        };
        let mut pet = Entity::from_pet_summon(owner, foot, template(), start);
        pet.target = Some(enemy);
        // The owner stands beside the pet, so the follow branch holds still.
        let targets = vec![(owner, Vec3f { x: -9.0, ..foot })];
        let enemies = vec![(enemy, TOP, true)];
        let at_foot = until_stuck(&mut pet, &targets, &enemies, &t, start);
        let now = tick_for(
            &mut pet,
            &targets,
            &enemies,
            crate::world::MOB_STUCK_SECS - 0.5,
            &t,
            at_foot,
        );
        assert_eq!(pet.target, Some(enemy), "still trying");
        let _ = tick_for(&mut pet, &targets, &enemies, 1.0, &t, now);
        assert_eq!(pet.target, None, "dropped the target it cannot reach");
    }

    /// The review's find: any accepted step used to stop the clock, so a mob
    /// sliding along a face's foot after a player walking the lip above, never
    /// closing, never gave up. Steps that get no closer keep it running; coming
    /// STUCK_PROGRESS_M closer, or a new goal, stops it.
    #[test]
    fn the_stuck_clock_runs_through_steps_that_get_no_closer() {
        let t0 = Instant::now();
        let at = |s: f32| t0 + Duration::from_secs_f32(s);
        let goal = Some(7);
        let mut e = idle_entity();
        assert!(
            !e.stuck_too_long(true, goal, 10.0, t0),
            "a step taken starts nothing"
        );
        assert!(e.stuck.is_none());
        assert!(
            !e.stuck_too_long(false, goal, 10.0, t0),
            "a refused step starts the clock"
        );
        // Steps taken, none closer, past the window: it has been held.
        for i in 1..170 {
            let held = e.stuck_too_long(i % 3 != 0, goal, 10.2, at(i as f32 * 0.05));
            assert_eq!(
                held,
                i as f32 * 0.05 >= crate::world::MOB_STUCK_SECS,
                "tick {i}"
            );
        }
        // A metre closer than it began: no longer stuck.
        assert!(!e.stuck_too_long(true, goal, 8.9, at(8.5)));
        assert!(e.stuck.is_none());
        // A new goal starts afresh (the review: a target switch inherited the
        // old target's time).
        assert!(!e.stuck_too_long(false, goal, 10.0, at(9.0)));
        assert!(
            !e.stuck_too_long(false, Some(8), 4.0, at(17.5)),
            "a new goal is not held yet"
        );
        assert_eq!(e.stuck.map(|c| c.goal), Some(Some(8)));
    }

    /// The review's find: giving up wiped the whole hate list, so a Warrior
    /// meleeing the mob at the foot while a caster on the lip held the top
    /// threat got free swings at a mob walking home. It forgets only the
    /// target it cannot reach and turns on the attacker it can.
    #[test]
    fn a_mob_that_gives_up_turns_on_an_attacker_it_can_reach() {
        let t = foot_and_top();
        let start = Instant::now();
        let caster: EntityId = 1;
        let warrior: EntityId = 2;
        let foot = Vec3f {
            x: -8.0,
            y: 0.0,
            z: 0.0,
        };
        let mut e = Entity::from_spawn(0, foot, template(), start);
        e.threat.insert(caster, 20.0);
        e.threat.insert(warrior, 10.0);
        e.target = Some(caster);
        e.transition(EnemyState::Chase, start);
        let beside = Vec3f {
            x: -12.0,
            y: 0.0,
            z: 6.0,
        };
        let targets = vec![(caster, TOP), (warrior, beside)];
        let at_foot = until_stuck(&mut e, &targets, &[], &t, start);
        tick_for(
            &mut e,
            &targets,
            &[],
            crate::world::MOB_STUCK_SECS + 0.2,
            &t,
            at_foot,
        );
        assert_eq!(
            e.target,
            Some(warrior),
            "turned on the attacker it can reach"
        );
        assert!(!e.threat.contains_key(&caster), "forgot the one it cannot");
        assert!(e.threat.contains_key(&warrior));
        assert!(
            matches!(e.state, EnemyState::Chase | EnemyState::Attack),
            "still fighting: {:?}",
            e.state
        );
    }

    // ── Wandering (roam.rs) ──────────────────────────────────────────────

    use crate::world::roam::Region;
    use crate::world::terrain::Terrain;
    use rand::SeedableRng;

    const TICK: f32 = 0.05;

    /// An 800 m flat plate and its roam map.
    fn roam_plate() -> (Terrain, RoamMap) {
        let t = Terrain::flat(40, 20.0, 0.0);
        let map = RoamMap::build(&t, (0.0, 0.0));
        (t, map)
    }

    /// A circle 300 m south of the town.
    fn south() -> Region {
        Region {
            center: [0.0, 300.0],
            radius: 90.0,
            min_from_town: 0.0,
            z_min: None,
            z_max: None,
        }
    }

    fn wanderer(pos: Vec3f, region: Region, now: Instant) -> Entity {
        let mut e = Entity::from_spawn(60, pos, template(), now);
        e.roam = Some(Roam {
            region,
            night_only: false,
            pack: false,
            pause_until: None,
            goal: None,
            solo_until: None,
        });
        e
    }

    fn xz(x: f32, z: f32) -> Vec3f {
        Vec3f { x, y: 0.0, z }
    }

    /// A wanderer's walk: it picks a leg, ambles along it at `ROAM_SPEED_MULT`
    /// of its speed facing the way it goes, arrives, stands at least
    /// `ROAM_PAUSE_MIN`, and sets off again within `ROAM_PAUSE_MAX`. All the
    /// while it is Idle (never the evading walk home), on the roam map, in
    /// its region and outside the town.
    #[test]
    fn a_wanderer_walks_a_leg_at_an_amble_then_stands_a_while() {
        let (ground, map) = roam_plate();
        let mut rng = rand::rngs::StdRng::seed_from_u64(11);
        let mut now = Instant::now();
        let mut e = wanderer(xz(0.0, 300.0), south(), now);
        let amble = e.mob.speed * roam::ROAM_SPEED_MULT * TICK;
        let mut walked_ticks = 0;
        let mut arrived_at: Option<Instant> = None;
        // The first leg: at most ROAM_LEG_MAX at the amble.
        for _ in 0..(roam::ROAM_LEG_MAX / amble) as usize + 10 {
            let before = e.pos;
            e.tick_roam(&map, None, TICK, now, &ground, &mut rng);
            now += Duration::from_millis(50);
            let step = e.pos.xz_distance_to(before);
            assert!(
                step <= amble + 1e-4,
                "a step of {step} at an amble of {amble}"
            );
            assert_eq!(e.state, EnemyState::Idle);
            assert!(!e.is_evading(now), "a wanderer on its legs can be hit");
            assert!(map.roamable(e.pos.x, e.pos.z) && south().contains(e.pos.x, e.pos.z));
            if step > 0.0 {
                walked_ticks += 1;
                let facing = (e.pos.x - before.x).atan2(e.pos.z - before.z);
                assert!((e.yaw - facing).abs() < 1e-3, "it faces the way it walks");
            }
            let walk = e.roam.expect("a wanderer");
            if walk.goal.is_none() && walk.pause_until.is_some() {
                arrived_at = Some(now);
                break;
            }
        }
        let arrived_at = arrived_at.expect("the leg ends");
        let leg = walked_ticks as f32 * amble;
        assert!(
            (roam::ROAM_LEG_MIN - 1.5..=roam::ROAM_LEG_MAX + 0.5).contains(&leg),
            "a leg of {leg} m"
        );
        // It stands for the pause.
        let stood_at = e.pos;
        let mut moved_after = None;
        for tick in 0..(roam::ROAM_PAUSE_MAX / TICK) as usize + 5 {
            e.tick_roam(&map, None, TICK, now, &ground, &mut rng);
            now += Duration::from_millis(50);
            if e.pos.xz_distance_to(stood_at) > 0.0 {
                moved_after = Some(tick as f32 * TICK);
                break;
            }
        }
        let stood = moved_after.expect("it sets off again");
        assert!(
            stood >= roam::ROAM_PAUSE_MIN - 0.1 && now >= arrived_at,
            "it stood only {stood} s"
        );
    }

    /// A plateau 40 m up with a cliff down its south side, away from the
    /// town: ground at z < 0 is the top, at z > 10 the bottom, between them a
    /// face (76 degrees). The grid is centred on x = 1000.
    fn cliff() -> Terrain {
        let gs = 8;
        let mut heights = vec![0.0; (gs + 1) * (gs + 1)];
        for iz in 0..=4 {
            for ix in 0..=gs {
                heights[iz * (gs + 1) + ix] = 40.0;
            }
        }
        Terrain::from_heights(gs, 10.0, [1000.0, 0.0, 0.0], 0.0, heights).expect("grid")
    }

    fn everywhere() -> Region {
        Region {
            center: [1000.0, 0.0],
            radius: 100.0,
            min_from_town: 0.0,
            z_min: None,
            z_max: None,
        }
    }

    /// A wanderer never walks onto a face, in EITHER direction. The slope
    /// rule alone refuses only a step that rises onto one, and a mob never
    /// falls, so it would walk down a cliff pinned to it and into ground it
    /// cannot leave. A leg across the cliff's lip ends at the lip, at once
    /// (no stuck clock runs), and one from the foot ends at the foot.
    #[test]
    fn a_roam_step_onto_a_face_is_refused_going_down_as_well_as_up() {
        let ground = cliff();
        let mut rng = rand::rngs::StdRng::seed_from_u64(1);
        let now = Instant::now();

        // From the top, toward the bottom.
        let top = RoamMap::build(&ground, (1000.0, -30.0));
        assert!(top.roamable(1000.0, -1.0) && !top.roamable(1000.0, 5.0));
        let mut e = wanderer(ground.ground_vec(xz(1000.0, -3.0)), everywhere(), now);
        e.roam = e.roam.map(|w| Roam {
            goal: Some((1000.0, 30.0)),
            ..w
        });
        let mut t = now;
        for _ in 0..200 {
            e.tick_roam(&top, None, TICK, t, &ground, &mut rng);
            t += Duration::from_millis(50);
            assert!(e.pos.z <= 1e-3, "it walked off the lip to z = {}", e.pos.z);
            assert!((e.pos.y - 40.0).abs() < 1e-3, "it is still on the top");
        }
        let walk = e.roam.expect("a wanderer");
        assert!(e.pos.z > -0.2, "it walked to the lip");
        assert!(e.stuck.is_none(), "no stuck clock runs for a leg");
        assert!(
            walk.goal != Some((1000.0, 30.0)),
            "the leg across the cliff was dropped"
        );
        // The slope rule alone would have let it down.
        assert!(ground.slope_ok(e.pos, xz(1000.0, 0.5)));

        // From the bottom, toward the top.
        let bottom = RoamMap::build(&ground, (1000.0, 30.0));
        let mut e = wanderer(ground.ground_vec(xz(1000.0, 13.0)), everywhere(), now);
        e.roam = e.roam.map(|w| Roam {
            goal: Some((1000.0, -30.0)),
            ..w
        });
        let mut t = now;
        for _ in 0..200 {
            e.tick_roam(&bottom, None, TICK, t, &ground, &mut rng);
            t += Duration::from_millis(50);
            assert!(e.pos.z >= 10.0 - 1e-3 && e.pos.y.abs() < 1e-3);
        }
    }

    /// Root holds a wanderer where it stands and snare slows its amble; a
    /// mezzed or stunned one, a charmed one (a pet) and one in a fight do not
    /// walk at all.
    #[test]
    fn root_holds_a_wanderer_snare_slows_it_and_only_an_idle_mob_walks() {
        let (ground, map) = roam_plate();
        let mut rng = rand::rngs::StdRng::seed_from_u64(2);
        let now = Instant::now();
        let start = xz(0.0, 300.0);
        let step_of = |prepare: &dyn Fn(&mut Entity), rng: &mut rand::rngs::StdRng| {
            let mut e = wanderer(start, south(), now);
            prepare(&mut e);
            e.tick_roam(&map, None, TICK, now, &ground, rng);
            e.pos.xz_distance_to(start)
        };
        let amble = template().speed * roam::ROAM_SPEED_MULT * TICK;
        assert!((step_of(&|_| {}, &mut rng) - amble).abs() < 1e-4);
        assert_eq!(
            step_of(&|e| e.apply_cc(ActiveCc::new_root(10.0)), &mut rng),
            0.0
        );
        let snared = step_of(&|e| e.apply_cc(ActiveCc::new_snare(0.5, 10.0)), &mut rng);
        assert!((snared - amble * 0.5).abs() < 1e-4, "half speed: {snared}");
        assert_eq!(
            step_of(&|e| e.apply_cc(ActiveCc::new_mez(10.0)), &mut rng),
            0.0
        );
        assert_eq!(
            step_of(&|e| e.apply_cc(ActiveCc::new_stun(10.0)), &mut rng),
            0.0
        );
        assert_eq!(step_of(&|e| e.owner = Some(7), &mut rng), 0.0, "a pet");
        assert_eq!(
            step_of(&|e| e.transition(EnemyState::Chase, now), &mut rng),
            0.0
        );
        assert_eq!(
            step_of(&|e| e.transition(EnemyState::Leash, now), &mut rng),
            0.0
        );
        // A camp mob (no roam record) never walks.
        let mut camp = Entity::from_spawn(0, start, template(), now);
        camp.tick_roam(&map, None, TICK, now, &ground, &mut rng);
        assert_eq!(camp.pos.xz_distance_to(start), 0.0);
    }

    /// A wanderer's home is where its fight began (so the walk back after a
    /// leash is short), set when it picks a target while idle and never by a
    /// charm ending: a charmed wanderer handed back keeps the home it had
    /// when charmed, wherever its charmer took it (town, usually), and is a
    /// wanderer again. Ground off the roam map never becomes home.
    #[test]
    fn a_wanderers_home_is_where_its_fight_began_and_never_where_a_charm_ended() {
        let (ground, map) = roam_plate();
        let now = Instant::now();
        let first = xz(0.0, 300.0);
        let mut e = wanderer(first, south(), now);
        e.pos = xz(30.0, 320.0);
        e.roam = e.roam.map(|w| Roam {
            goal: Some((60.0, 330.0)),
            pause_until: Some(now),
            ..w
        });
        e.roam_fight_began(&map);
        assert_eq!(e.spawn_pos.xz_distance_to(xz(30.0, 320.0)), 0.0);
        let walk = e.roam.expect("still a wanderer");
        assert!(walk.goal.is_none() && walk.pause_until.is_none());

        // Off the map (off the plate): the old home stays.
        e.pos = xz(900.0, 320.0);
        e.roam_fight_began(&map);
        assert_eq!(e.spawn_pos.xz_distance_to(xz(30.0, 320.0)), 0.0);

        // Charmed at home, walked into town by its charmer, handed back.
        e.pos = xz(30.0, 320.0);
        let mut pet = Entity::from_pet_summon(42, e.pos, e.mob.clone(), now);
        pet.spawn_point_idx = e.spawn_point_idx;
        pet.spawn_pos = e.spawn_pos;
        pet.roam = e.roam;
        pet.charm_expires_at = Some(now);
        pet.pos = xz(2.0, 3.0);
        // A pet never walks its legs.
        let mut rng = rand::rngs::StdRng::seed_from_u64(4);
        pet.tick_roam(&map, None, TICK, now, &ground, &mut rng);
        assert_eq!(pet.pos.xz_distance_to(xz(2.0, 3.0)), 0.0);
        let mut back = Entity::released_from_charm(&pet, now);
        back.turn_on(42, now);
        assert!(back.roam.is_some(), "a wanderer again");
        assert_eq!(
            back.spawn_pos.xz_distance_to(xz(30.0, 320.0)),
            0.0,
            "its home is not the town"
        );

        // A packmate that joins a fight: on its hate list, chasing, home here.
        let mut mate = wanderer(first, south(), now);
        mate.pos = xz(10.0, 290.0);
        mate.join_pack_fight(99, &map, now);
        assert_eq!(mate.target, Some(99));
        assert_eq!(mate.state, EnemyState::Chase);
        assert_eq!(mate.threat.get(&99), Some(&roam::PACK_JOIN_THREAT));
        assert_eq!(mate.spawn_pos.xz_distance_to(xz(10.0, 290.0)), 0.0);
    }

    /// An idle wanderer standing off the roam map (the walk home stops within
    /// a metre of home, which can be the wrong side of a face's edge) steps
    /// onto its home, which is on the map, instead of standing there for
    /// good with every first step refused.
    #[test]
    fn an_idle_wanderer_off_the_map_steps_onto_its_home() {
        let ground = cliff();
        let top = RoamMap::build(&ground, (1000.0, -30.0));
        let mut rng = rand::rngs::StdRng::seed_from_u64(6);
        let now = Instant::now();
        let home = ground.ground_vec(xz(1000.0, -0.5));
        let mut e = wanderer(home, everywhere(), now);
        // Half a metre past the lip, on the face.
        e.pos = ground.ground_vec(xz(1000.0, 0.4));
        assert!(!top.roamable(e.pos.x, e.pos.z));
        e.apply_cc(ActiveCc::new_root(1.0));
        e.tick_roam(&top, None, TICK, now, &ground, &mut rng);
        assert_eq!(e.pos.xz_distance_to(home), 0.0);
        assert!((e.pos.y - 40.0).abs() < 1e-3);
    }

    /// A pack follower keeps to its leader: it stands when close, ambles
    /// after it, runs when it has fallen behind, and picks no legs of its
    /// own. When the straight line to the leader cuts the town it does not
    /// take it (a wolf through the vendors): it walks its own legs for
    /// `PACK_SOLO_SECS` and tries again.
    #[test]
    fn a_follower_keeps_to_its_leader_and_never_cuts_through_the_town() {
        let (ground, map) = roam_plate();
        let mut rng = rand::rngs::StdRng::seed_from_u64(9);
        let now = Instant::now();
        let leader = xz(0.0, 300.0);
        let speed = template().speed * TICK;
        let step_from = |dz: f32, rng: &mut rand::rngs::StdRng| {
            let start = xz(0.0, 300.0 + dz);
            let mut e = wanderer(start, south(), now);
            e.tick_roam(&map, Some(leader), TICK, now, &ground, rng);
            assert!(
                e.roam.expect("a wanderer").goal.is_none(),
                "no leg of its own"
            );
            (e.pos.z - start.z, e.pos.xz_distance_to(start))
        };
        assert_eq!(step_from(3.0, &mut rng).1, 0.0, "close enough: it stands");
        let (dz, amble) = step_from(8.0, &mut rng);
        assert!(dz < 0.0, "toward the leader");
        assert!((amble - speed * roam::ROAM_SPEED_MULT).abs() < 1e-4);
        let (_, run) = step_from(30.0, &mut rng);
        assert!((run - speed).abs() < 1e-4, "fallen behind: it runs");

        // The leader is on the far side of the town.
        let mut e = wanderer(xz(0.0, 300.0), south(), now);
        let far_leader = xz(0.0, -300.0);
        e.tick_roam(&map, Some(far_leader), TICK, now, &ground, &mut rng);
        let walk = e.roam.expect("a wanderer");
        assert_eq!(
            e.pos.xz_distance_to(xz(0.0, 300.0)),
            0.0,
            "not a step toward town"
        );
        let until = walk.solo_until.expect("it walks alone for a while");
        assert!(until > now);
        // Alone, it walks legs of its own, in its region.
        e.tick_roam(&map, Some(far_leader), TICK, now, &ground, &mut rng);
        let goal = e.roam.expect("a wanderer").goal.expect("a leg of its own");
        assert!(south().contains(goal.0, goal.1));
        // And tries its leader again afterwards.
        let later = until + Duration::from_millis(50);
        e.tick_roam(
            &map,
            Some(xz(e.pos.x, e.pos.z + 8.0)),
            TICK,
            later,
            &ground,
            &mut rng,
        );
        let walk = e.roam.expect("a wanderer");
        assert!(walk.solo_until.is_none() && walk.goal.is_none());
    }
}
