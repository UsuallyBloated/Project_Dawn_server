# Server Architecture Design

Authoritative server design for Project Dawn's online MMORPG mode. This document is the contract between the GDScript client (this repo) and the Rust server (`Projects/server/`). Read it before touching either side's gameplay code.

> **Status**: Design — server implementation has not started. Client-side code today operates client-authoritatively for local-save alpha builds. Once the server is online, client-side authoritative state is replaced by replication.

---

## Contents

1. [Goals & non-goals](#goals--non-goals)
2. [Topology](#topology)
3. [Transport](#transport)
4. [State ownership](#state-ownership)
5. [Authentication & sessions](#authentication--sessions)
6. [Wire protocol](#wire-protocol)
7. [Database schema](#database-schema)
8. [Tick model & simulation](#tick-model--simulation)
9. [Persistence cadence](#persistence-cadence)
10. [Connection lifecycle](#connection-lifecycle)
11. [Reconnect handling](#reconnect-handling)
12. [GM commands](#gm-commands)
13. [Version handshake](#version-handshake)
14. [Server restart & shutdown](#server-restart--shutdown)
15. [Backup strategy](#backup-strategy)
16. [Anti-cheat posture](#anti-cheat-posture)
17. [Launcher protocol](#launcher-protocol)
18. [Repo layout](#repo-layout)
19. [Client migration path](#client-migration-path)
20. [Open questions](#open-questions)

---

## Goals & non-goals

### Goals

- **Server-authoritative gameplay from message 1.** Client sends *intent* (move-here, attack-this, cast-this); server resolves and replicates results. No `client → "I dealt 20 damage"` packets, ever.
- **Friends-only alpha** with username/password auth, hosted on a single Linux box at home, accessed via Tailscale (no public DNS or Let's Encrypt for now).
- **Multiple shards eventually.** Account database is global; character database is per-shard. No transfers between shards.
- **Pre-alpha local-save build keeps shipping** until the server is online. Local saves are throwaway — wipes are expected.
- **Production hygiene from day one** for the things that bite later: atomic inventory transactions (no dupes), DB backups, version handshakes, session tokens with expiry, GM audit log.

### Non-goals (for now)

- Public-internet hosting with TLS / DDNS / domain — defer until Steam-launch era.
- Steam integration — separate track, weeks-to-months out.
- Full anti-cheat enforcement — server-authoritative protocol prevents the worst, manual GM observation handles the rest in the alpha.
- Voice chat — Discord covers it; proximity chat is "future fun" and not designed here.
- Cross-shard play, mailbox, auction house — out of scope.
- Federated auth or external account systems (itch.io / Steam SSO) — own auth only.
- High-availability / horizontal scaling — single process, single box, restart-tolerant.

---

## Topology

For the alpha, **one Rust process** with two logical services:

```
┌──────────────────────────┐        ┌─────────────────────────┐
│   Launcher (Godot .exe)  │        │     Game (Godot .exe)   │
│  - login form            │        │  - 3D world, HUD, etc.  │
│  - char select/create    │        │                         │
└────────────┬─────────────┘        └────────────┬────────────┘
             │ WebSocket (auth)                   │ UDP/renet (gameplay)
             │   - Login / Register               │   - Move, Attack, Cast
             │   - CharList / CharCreate          │   - Inventory, Chat
             │   - issues session_token           │   - validates token on Connect
             ▼                                    ▼
┌────────────────────────────────────────────────────────────┐
│                  projectdawn-server (Rust)                 │
│  ┌────────────────┐    ┌────────────────────────────────┐ │
│  │  auth handler  │    │   world handler / simulation   │ │
│  │  (WebSocket)   │    │   (renet, 20 Hz tick loop)     │ │
│  └───────┬────────┘    └───────────────┬────────────────┘ │
│          │                              │                  │
│          └──────────────┬───────────────┘                  │
│                         ▼                                  │
│                  SQLite (sqlx)                             │
│  accounts · sessions · characters · inventory · etc.       │
└────────────────────────────────────────────────────────────┘
```

**Future split** (when load demands): extract auth into its own crate/process with its own DB. The world server queries auth via internal RPC. Architectural seam is preserved by keeping the protocol crate separate.

---

## Transport

| Channel | Transport | Format | Why |
|---|---|---|---|
| Auth & lobby | WebSocket (TCP) over Tailscale | JSON | Easy to debug, firewall-friendly, no UDP for things that should be reliable. Friends connect to `ws://<tailscale-ip>:8765`. |
| Gameplay | UDP via [`renet`](https://crates.io/crates/renet) | bincode | Low-latency, channel-aware (reliable/unreliable), purpose-built for game networking. |

**TLS / encryption.** Tailscale provides end-to-end WireGuard encryption inside the tailnet. We don't add TLS on top for the alpha. When public hosting arrives, both endpoints get TLS (`wss://` for auth, DTLS or QUIC for gameplay).

**renet channel layout** (configured at handshake):

| Channel | Mode | Use |
|---|---|---|
| 0 | Reliable Ordered | Inventory ops, equipment changes, quest updates, chat |
| 1 | Reliable Unordered | Combat events (hit / miss / damage notifications) |
| 2 | Unreliable Sequenced | Position / rotation snapshots (newer drops older) |
| 3 | Reliable Ordered | System / kick / shutdown messages |

---

## State ownership

The single most important table in this document. **Server owns everything that affects gameplay outcomes.** Client owns presentation and input.

| State | Owner | Replicated to |
|---|---|---|
| Player position / velocity | Server | All players in same area-of-interest |
| Player HP / MP / stamina | Server | Owner + nearby (HP bars) |
| Stats (str/dex/agi/int/wis/cha/con) | Server | Owner only |
| Max HP / MP / stamina | Server | Owner only |
| Level / XP / XP-to-next | Server | Owner only |
| Coins | Server | Owner only |
| Bind point | Server | Owner only |
| Alignment score | Server | Owner only |
| Transformation (lich, revenant, etc.) | Server | Owner + nearby (visual) |
| Inventory (base slots + bag contents) | Server | Owner only |
| Equipment (paperdoll) | Server | Owner only (full); nearby (visible slots only) |
| Active buffs / debuffs | Server | Owner + nearby (visible auras) |
| Cooldowns (spells / skills) | Server | Owner only |
| Quest state | Server | Owner only |
| Passive skills (weapon / armor / casting) | Server | Owner only |
| Pet state (HP, position, target) | Server | Owner + nearby |
| Time of day | Server | All clients (interpolated) |
| Loot bag contents | Server | Players who can see the bag |
| Enemy state (HP, position, target) | Server | Players in area-of-interest |
| Targeting (`current_target`) | **Hybrid** | Client picks `target_id`, server validates and stores |
| Camera / look direction | Client | — (not replicated) |
| HUD layout / window positions | Client | — |
| UI scale / settings.cfg | Client | — |
| Chat scrollback (local) | Client | — |
| Combat log lines (local cache) | Client | Built from server events |
| Floating combat text | Client | Built from server events |

**Authoritative principle**: anything in the server-owned column, the client may *display* (cached locally) but never *decide*. Damage, drops, item gains, XP gains, level-ups, alignment shifts, buff applications — **all server-decided.**

---

## Authentication & sessions

### Account creation

Launcher → POST WebSocket `Register { username, password, email? }`.

- Username: 3–20 chars, `[a-zA-Z0-9_]`, unique.
- Password: ≥8 chars, no other restrictions for alpha. Hashed with **Argon2id** at default parameters before insert.
- Email: optional in alpha (used later for password reset).

Server replies `RegisterOk { account_id }` or `Error { code, msg }`.

### Login

Launcher → `Login { username, password, client_version }`.

Server:
1. Look up account by username.
2. Verify password against argon2 hash.
3. Check `client_version` against `MIN_CLIENT_VERSION` (config). On mismatch → `Error { code: "version_mismatch", msg: "Please update your launcher.", min_version }`.
4. Create row in `sessions` table: random 256-bit token, account_id, issued_at, expires_at = now + 30 minutes, last_seen = now.
5. Reply `LoginOk { session_token, account_id, characters: [...], world_endpoint, is_gm }`.

`world_endpoint` is `"<tailscale-ip>:7777"` for the alpha. Provided by server config so the launcher doesn't hardcode it.

### Session lifecycle

- Tokens expire 30 minutes after `last_seen`.
- Every authenticated request (auth WebSocket OR world UDP packet that includes a token) updates `last_seen`.
- Logout / disconnect → server sets `expires_at = now`.
- On token expiry mid-game, server kicks with `KickReason { code: "session_expired" }`. Launcher re-auth flow kicks in.
- Tokens are opaque random bytes, not JWTs (simpler for alpha; revoke is just a DELETE).

### Banning

- `accounts.is_banned BOOLEAN` and `accounts.ban_reason TEXT NULL`. Login with `is_banned = TRUE` → `Error { code: "banned", msg: ban_reason }`.
- GM command sets the flag and kicks any active sessions.

---

## Wire protocol

Two namespaces: **auth** (JSON over WebSocket) and **world** (bincode over renet).

The Rust crate `crates/protocol/` defines all message types. The client mirrors them in `scripts/net/protocol.gd` (manually maintained for alpha; codegen later).

### Auth messages (JSON over WebSocket)

#### Client → Server

```jsonc
{ "type": "Register",   "username": "...", "password": "...", "email": "..." }
{ "type": "Login",      "username": "...", "password": "...", "client_version": "0.2.0" }
{ "type": "CharList",   "session_token": "..." }
{ "type": "CharCreate", "session_token": "...", "name": "...", "race": "Troll", "class": "Shadow Knight" }
{ "type": "CharDelete", "session_token": "...", "char_id": 42 }
{ "type": "Logout",     "session_token": "..." }
```

#### Server → Client

```jsonc
{ "type": "RegisterOk",   "account_id": 1 }
{ "type": "LoginOk",      "session_token": "...", "account_id": 1, "is_gm": false,
                          "world_endpoint": "100.x.y.z:7777",
                          "characters": [
                            { "id": 12, "name": "Chortle", "race": "Troll", "class": "Shadow Knight", "level": 27, "zone": "newbie" }
                          ] }
{ "type": "CharList",     "characters": [ ... ] }
{ "type": "CharCreated",  "char_id": 42 }
{ "type": "CharDeleted" }
{ "type": "LogoutOk" }
{ "type": "Error",        "code": "name_taken" | "version_mismatch" | "auth_failed" | "session_expired" | "banned" | "internal",
                          "msg": "human-readable", "extra": { ... } }
```

### World messages (bincode over renet)

Pseudo-Rust types — actual definitions live in `crates/protocol/src/world.rs`.

#### Client → Server

```rust
enum ClientMsg {
    // Connection
    Connect { session_token: [u8; 32], char_id: u64, client_version: String },
    Disconnect,
    Heartbeat,

    // Movement (sent at ~20 Hz; server reconciles)
    Move { sequence: u32, direction: Vec3, jumping: bool },

    // Targeting & combat (intent only — server resolves outcomes)
    SetTarget { target_id: Option<EntityId> },
    Attack,                                         // uses current_target
    CastSpell { spell_id: u32, target_id: Option<EntityId> },
    UseSkill { skill_id: u32, target_id: Option<EntityId> },
    CancelCast,

    // Inventory (intent only — server validates and replies)
    MoveItem { from: SlotRef, to: SlotRef },
    EquipItem { from: SlotRef },
    UnequipItem { slot: EquipSlot },
    DropItem { slot: SlotRef, count: u32 },
    UseConsumable { slot: SlotRef },
    StackAll,

    // World interaction
    Interact { entity_id: EntityId },               // NPC, station, mining node, corpse
    DialogueResponse { node_id: String, choice_idx: u32 },
    BuyItem { vendor_id: EntityId, item_name: String, qty: u32 },
    SellItem { slot: SlotRef, qty: u32 },
    LootItem { bag_id: EntityId, slot: u32 },
    LootAll { bag_id: EntityId },

    // Quests
    AcceptQuest { quest_id: String, giver_id: EntityId },
    AbandonQuest { quest_id: String },
    TurnInQuest { quest_id: String, npc_id: EntityId },

    // Crafting & gathering
    StartCombine { recipe_id: String, station_id: EntityId },
    StartMining { node_id: EntityId },
    StartSkinning { corpse_id: EntityId },

    // Social
    Chat { channel: ChatChannel, text: String },    // SAY / OOC / GROUP / TELL{name}
    Sit, Stand,

    // Sit / bind
    BindAtCurrentLocation,                          // server validates legal-bind zone

    // Group
    GroupInvite { name: String },
    GroupAcceptInvite { from: u64 },
    GroupLeave,
    GroupKick { name: String },

    // GM (gated to is_gm accounts)
    GmCommand { line: String },                     // parsed server-side
}
```

#### Server → Client

```rust
enum ServerMsg {
    // Connection
    ConnectOk { player_id: EntityId, snapshot: WorldSnapshot },
    Kick { reason: String, code: KickCode, reconnect_after_secs: Option<u32> },
    Heartbeat,

    // Entity replication (snapshot delta-encoded against last ack)
    EntitySpawn { id: EntityId, kind: EntityKind, ... },
    EntityDespawn { id: EntityId },
    Position { id: EntityId, pos: Vec3, vel: Vec3, yaw: f32, sequence: u32 },

    // Stats / resources
    HealthUpdate { id: EntityId, hp: f32, max_hp: f32 },
    ManaUpdate { id: EntityId, mp: f32, max_mp: f32 },          // owner only
    StaminaUpdate { id: EntityId, stamina: f32, max: f32 },     // owner only
    StatsUpdate { full_stats_dict },                            // owner only
    CoinsUpdate { coins: i64 },                                 // owner only
    XpGained { amount: i32, current: i32, to_next: i32 },      // owner only; current/to_next now real (PD_W0018)
    LevelUp { new_level: u32, xp: i32, xp_to_next: i32 },       // owner only; up on xp, DOWN on death penalty (PD_W0018)
    AlignmentChanged { score: i32, tier: String },

    // Combat events
    Hit { attacker: EntityId, target: EntityId, amount: i32, crit: bool, dmg_type: DamageType },
    Miss { attacker: EntityId, target: EntityId },
    Evade { attacker: EntityId, target: EntityId },
    DamageDealt { ... }, DamageTaken { ... },                   // routed to log
    EntityDied { id: EntityId },

    // Buffs
    BuffApplied { target: EntityId, buff_id: String, duration: f32 },
    BuffRemoved { target: EntityId, buff_id: String },
    HotTick { target: EntityId, amount: i32, source: String },
    DotTick { target: EntityId, amount: i32, source: String },

    // Casting
    CastStart { caster: EntityId, spell_id: u32, duration: f32 },
    CastComplete { caster: EntityId, spell_id: u32 },
    CastFail { reason: String },
    Cooldown { spell_or_skill_id: u32, remaining: f32, total: f32 },

    // Inventory / equipment (full snapshot or delta)
    InventoryUpdate { snapshot: InventoryState },               // owner only
    EquipmentUpdate { snapshot: EquipmentState },               // owner only
    EquipmentVisual { id: EntityId, visible_slots: VisibleEquipment },  // public

    // World interaction
    OpenVendor { vendor_id: EntityId, name: String, vtype: String, stock: Vec<ItemRef> },
    OpenDialogue { npc_name: String, node_id: String, text: String, responses: Vec<Response> },
    LootBagOpened { bag_id: EntityId, contents: Vec<ItemStack> },
    LootResult { ok: bool, msg: String, snapshot: InventoryState },

    // Quests
    QuestUpdate { quest_id: String, status: QuestStatus, objectives: [...] },
    QuestRewards { quest_id: String, xp: i32, items: Vec<ItemRef>, coins: i64 },

    // Time
    TimeOfDay { hour: f32 },                                    // every 1 minute

    // Chat
    ChatMessage { speaker: String, channel: ChatChannel, text: String, lang: String },

    // System
    Error { code: String, msg: String },
    BroadcastMessage { msg: String },                           // GM /gm broadcast
}
```

### Wire format

- **Auth**: `serde_json` — debuggable, slow, fine for the half-dozen messages exchanged at login.
- **World**: `bincode 2.x` — compact (no field names on the wire), fast, but harder to debug. We pair it with a debug-build flag to dump packets as JSON when needed.

### Versioning

Each handshake includes `client_version: String` (semver). Server config has:

```toml
[protocol]
min_client_version = "0.2.0"
max_client_version = "0.2.999"
```

Mismatches → `Error { code: "version_mismatch", min_version, max_version }` and the launcher prompts the user to update.

---

## Database schema

SQLite for the alpha (`world.db`). Postgres migration is mechanical when load demands. All migrations under `migrations/` managed by `sqlx`.

### Accounts (global, all shards eventually share)

```sql
CREATE TABLE accounts (
    id            INTEGER PRIMARY KEY AUTOINCREMENT,
    username      TEXT    NOT NULL UNIQUE COLLATE NOCASE,
    password_hash TEXT    NOT NULL,                    -- argon2id
    email         TEXT,
    is_gm         BOOLEAN NOT NULL DEFAULT FALSE,
    is_banned     BOOLEAN NOT NULL DEFAULT FALSE,
    ban_reason    TEXT,
    created_at    TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
    last_login    TIMESTAMP
);

CREATE TABLE sessions (
    token       BLOB PRIMARY KEY,                       -- 32 random bytes
    account_id  INTEGER NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    issued_at   TIMESTAMP NOT NULL,
    expires_at  TIMESTAMP NOT NULL,
    last_seen   TIMESTAMP NOT NULL
);
CREATE INDEX idx_sessions_account ON sessions(account_id);
```

### Characters (per-shard)

```sql
CREATE TABLE characters (
    id              INTEGER PRIMARY KEY AUTOINCREMENT,
    account_id      INTEGER NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    name            TEXT    NOT NULL UNIQUE COLLATE NOCASE,
    race            TEXT    NOT NULL,
    class           TEXT    NOT NULL,
    level           INTEGER NOT NULL DEFAULT 1,
    xp              INTEGER NOT NULL DEFAULT 0,
    xp_to_next      INTEGER NOT NULL DEFAULT 100,
    -- Intrinsic (gear-free) stats — see PlayerStats M1 split
    base_strength       INTEGER NOT NULL DEFAULT 10,
    base_dexterity      INTEGER NOT NULL DEFAULT 10,
    base_agility        INTEGER NOT NULL DEFAULT 10,
    base_intelligence   INTEGER NOT NULL DEFAULT 10,
    base_wisdom         INTEGER NOT NULL DEFAULT 10,
    base_charisma       INTEGER NOT NULL DEFAULT 10,
    base_constitution   INTEGER NOT NULL DEFAULT 10,
    base_max_hp         REAL    NOT NULL DEFAULT 100.0,
    base_max_mp         REAL    NOT NULL DEFAULT 100.0,
    base_max_stamina    REAL    NOT NULL DEFAULT 100.0,
    -- Current resources
    hp                  REAL    NOT NULL,
    mp                  REAL    NOT NULL,
    stamina             REAL    NOT NULL,
    coins               INTEGER NOT NULL DEFAULT 0,
    -- Persistent meta
    alignment_score     INTEGER NOT NULL DEFAULT 0,
    bind_zone           TEXT,
    bind_entry          TEXT,
    bind_zone_name      TEXT,
    transformation      TEXT,
    -- Last known location
    zone                TEXT,
    pos_x               REAL,
    pos_y               REAL,
    pos_z               REAL,
    yaw                 REAL,
    -- Audit
    created_at          TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
    last_played_at      TIMESTAMP,
    deleted_at          TIMESTAMP                       -- soft delete
);
CREATE INDEX idx_chars_account ON characters(account_id);
```

### Inventory & equipment

```sql
CREATE TABLE inventory_slots (
    id              INTEGER PRIMARY KEY AUTOINCREMENT,
    character_id    INTEGER NOT NULL REFERENCES characters(id) ON DELETE CASCADE,
    base_slot       INTEGER NOT NULL,                   -- 0..7
    bag_slot        INTEGER,                            -- NULL if directly in base; 0..n if inside bag
    item_path       TEXT,                               -- res:// path for .tres-backed items
    item_snapshot   TEXT,                               -- JSON snapshot for runtime-built items (see ItemData.to_save_dict)
    count           INTEGER NOT NULL DEFAULT 1,
    UNIQUE (character_id, base_slot, bag_slot)
);

CREATE TABLE equipment_slots (
    character_id    INTEGER NOT NULL REFERENCES characters(id) ON DELETE CASCADE,
    slot            TEXT    NOT NULL,                   -- weapon, offhand, head, chest, legs, feet, hands, ring, neck
    item_path       TEXT,
    item_snapshot   TEXT,
    PRIMARY KEY (character_id, slot)
);
```

**Critical**: every inventory mutation (move, equip, drop, buy, sell, loot) is **one SQL transaction** that touches all affected rows. This is the entire dupe-prevention strategy. No client-pending state, no two-step writes.

### Skills, quests, spell books

```sql
CREATE TABLE character_skills (
    character_id    INTEGER NOT NULL REFERENCES characters(id) ON DELETE CASCADE,
    tracker         TEXT    NOT NULL,                   -- 'weapon', 'armor', 'casting'
    skill_name      TEXT    NOT NULL,
    value           INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (character_id, tracker, skill_name)
);

CREATE TABLE character_quests (
    character_id    INTEGER NOT NULL REFERENCES characters(id) ON DELETE CASCADE,
    quest_id        TEXT    NOT NULL,
    status          TEXT    NOT NULL,                   -- ACTIVE | COMPLETED | FAILED
    objectives_json TEXT    NOT NULL,                   -- JSON: per-objective progress
    accepted_at     TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
    completed_at    TIMESTAMP,
    PRIMARY KEY (character_id, quest_id)
);

CREATE TABLE character_languages (
    character_id    INTEGER NOT NULL REFERENCES characters(id) ON DELETE CASCADE,
    language        TEXT    NOT NULL,
    skill           INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (character_id, language)
);
```

### Audit & GM log

```sql
CREATE TABLE gm_actions (
    id              INTEGER PRIMARY KEY AUTOINCREMENT,
    actor_account   INTEGER NOT NULL REFERENCES accounts(id),
    target_account  INTEGER REFERENCES accounts(id),
    target_char     INTEGER REFERENCES characters(id),
    command         TEXT    NOT NULL,
    args            TEXT,
    timestamp       TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP
);
```

Every `/gm ...` invocation writes a row. Cheap insurance.

### Server config

```sql
CREATE TABLE server_config (
    key     TEXT PRIMARY KEY,
    value   TEXT NOT NULL
);
-- Example rows:
-- ('shard_name', 'Test Shard 1')
-- ('motd', 'Welcome to Project Dawn alpha!')
-- ('min_client_version', '0.2.0')
```

---

## Tick model & simulation

**Server tick: 20 Hz (50 ms).** Each tick:

1. **Drain incoming packets** from all connected clients. Validate session tokens (cheap — just touch sessions.last_seen).
2. **Apply player inputs**:
    - Movement intents → integrate position, validate against speed & terrain.
    - Combat intents (`Attack`, `CastSpell`, `UseSkill`) → enqueue events.
    - Inventory intents → run as DB transactions, reply with delta.
3. **Run AI**: enemies with `next_action_at <= now` evaluate behavior tree, possibly attack.
4. **Resolve combat events**: damage, hit/miss/crit/evade, on-hit procs, buff applications.
5. **Tick buffs / DoTs / HoTs / cooldowns**: decrement timers, apply periodic effects.
6. **Tick TimeOfDay**: advance world clock, broadcast every 1 minute.
7. **Build per-client snapshots**: delta-encoded against the last sequence each client acked. Includes only entities in their area-of-interest.
8. **Send snapshots** via renet's `send_message` per channel. renet handles retries / acks.

**Client tick**:

- Reads input, builds intent packets, sends at ~20 Hz on channel 2 (unreliable sequenced for movement).
- Receives snapshots; reconciles position against client-prediction; corrects if server differs by > epsilon.
- Renders at display rate (60–144 Hz), interpolating between received snapshots.

**Combat is event-driven, not per-tick**. When the player presses Attack, the auto-attack timer resolves on the server-side timer; the server emits a `Hit` / `Miss` event whenever the swing lands. The 20 Hz tick polls timer state but doesn't introduce 50 ms of latency on top.

**Area of interest (AOI)**: each player has a 100 m bubble (configurable). Server only sends entity updates for things inside the bubble. Saves bandwidth and CPU on large worlds.

---

## Persistence cadence

Disk writes happen on:

- **Login** — load full character.
- **Logout / disconnect** — save full character + inventory + equipment + skills + quests in one transaction.
- **Level up** — save character row.
- **Zone change** — save character row + position.
- **Inventory mutation** — every `MoveItem`, `EquipItem`, `BuyItem`, `LootItem`, etc. is itself a DB transaction.
- **Quest accept / progress / complete** — save quest row.
- **Periodic checkpoint** — every 5 minutes for active characters: save character row (current resources, position) + dirty quest rows. Cheap with SQLite WAL.
- **Server shutdown** — final save pass for all connected players (see [restart](#server-restart--shutdown)).

**No client-driven save calls.** The current `SaveManager.save()` autoload-on-level-up pattern in the client becomes a no-op once the server is online.

---

## Connection lifecycle

### Auth phase (WebSocket)

1. Launcher connects to `ws://<server>:8765`.
2. Sends `Login`. On `version_mismatch`, prompts user to update launcher; aborts.
3. On `LoginOk`, displays char list. User picks or creates a character.
4. On Play, launcher hands off to game `.exe`:
    ```
    projectdawn.exe --auth-token=<hex32> --char-id=<id> --world-endpoint=<host:port>
    ```
5. Launcher closes the WebSocket (or stays open for char-list refresh; alpha closes it).

### Game phase (renet UDP)

1. Game parses CLI args. If absent in alpha-with-fallback mode, falls back to local-save Test Room. In server-only mode, errors out: "Please launch via the launcher."
2. Connects to renet endpoint.
3. Sends `Connect { session_token, char_id, client_version }` on channel 3.
4. Server validates token + char ownership; loads character into memory; spawns entity in zone.
5. Server replies `ConnectOk { player_id, snapshot }`. Client transitions out of loading screen.
6. Client begins sending `Move` packets at 20 Hz; server begins sending entity snapshots.

### Disconnect

A character must remain in the world ~30 s before it actually leaves. Whether
that wait is paid up-front or after the fact depends on how the connection ends:

- **Clean disconnect** (`clean_disconnect` flag set): client sends `Disconnect`
  (ESC → Quit Game, or a completed `/camp`). Server reaps immediately — saves,
  despawns, broadcasts `EntityDespawn`, frees the account. (`/camp` is the
  voluntary case: the ~30 s wait already happened as the camp countdown.)
- **Unclean disconnect** (timeout or crash): the body goes **linkdead**. It stays
  in the world for `LINKDEAD_SECS` (~30 s), still targetable and **killable** by
  mobs and PvP, frozen in place (movement integration skips it). A same-account
  relogin is refused during the window (see below). When the window elapses, the
  reaper runs the same save + despawn + account-free path. Triggers: no app-layer
  packets for `HEARTBEAT_TIMEOUT` (10 s), or a renet transport drop (netcode
  timeout 15 s). Detection cost adds to the linger, so a hard crash takes up to
  ~45 s before the account frees. Last save was at most `CHECKPOINT_INTERVAL`
  (60 s) ago (periodic checkpoint).

The pet despawns immediately on linkdead (a linkdead player can't command it);
group membership is kept for the window so a brief drop doesn't double-vanish the
player from the roster. See `docs/design/camp_and_linkdead.md` for the full model
and the locked decisions.

---

## Reconnect handling

v1 is **wait-then-fresh-login**: there is no seamless reconnect-resume. A relogin
attempted while the previous session is still in the world (live, or lingering
linkdead) is **refused** by the one-character-per-account deny-login with
"You already have a character in this world." If the refused session is linkdead,
the `Kick.reconnect_after_secs` field carries the remaining linkdead seconds so
the client can show a retry countdown. Once the linkdead window elapses and the
body reaps, a fresh `Connect` succeeds and the client gets a `ConnectOk` with a
fresh snapshot (a full load, possibly in a different zone if the server moved them,
e.g. died and respawned at bind point).

Seamless resume on a brief network blip (the old 60 s frozen-and-untargetable
model) is a deferred enhancement, not built; it would need session resurrection,
AOI re-attach, and connect-token juggling.

**Session token expired**: server replies `Error { code: "session_expired" }`. Client must re-authenticate via the launcher.

**Server unreachable**: client retries up to 3 times over 10 seconds. After that, displays "Lost connection — relaunch when ready." Save state is whatever the server last persisted.

---

## GM commands

Chat lines starting with `/gm` from accounts with `is_gm = TRUE` are parsed server-side. Each invocation logs to `gm_actions`.

| Command | Effect |
|---|---|
| `/gm tp <x> <y> <z>` | Teleport self to coordinates in current zone |
| `/gm tp <player_name>` | Teleport self to player |
| `/gm summon <player_name>` | Bring player to me |
| `/gm spawn <mob_name> [count]` | Spawn enemies in front of me |
| `/gm spawn_named <id>` | Spawn a named/boss mob (`NamedMobDefinitions.ALL` keys) |
| `/gm despawn <radius>` | Despawn enemies within radius (default 50 m) |
| `/gm setlevel <n>` | Set self level (re-applies stat curve) |
| `/gm give <item_path> [count]` | Add item to self inventory |
| `/gm gold <amount>` | Add coins to self (negative to subtract) |
| `/gm alignment <delta>` | Modify self alignment score |
| `/gm settime <hour>` | Force time of day |
| `/gm pausetime` / `/gm resumetime` | Pause / resume day/night cycle |
| `/gm noclip` | Toggle collision for self |
| `/gm invuln` | Toggle damage immunity for self |
| `/gm speed <mult>` | Set move speed multiplier |
| `/gm kick <player_name>` | Disconnect player |
| `/gm ban <player_name> <reason>` | Set is_banned, kick |
| `/gm unban <player_name>` | Clear ban |
| `/gm broadcast <msg>` | Server-wide chat message styled as system broadcast |
| `/gm shutdown <minutes> [reason]` | Schedule restart with countdown broadcasts |
| `/gm save` | Force checkpoint of all connected players |
| `/gm players` | List connected players (account, char, zone, IP) |

GM flag is set with one-time SQL: `UPDATE accounts SET is_gm = TRUE WHERE username = 'you';`. No in-game promote command in the alpha — too risky.

---

## Version handshake

Both the auth and world handshakes carry `client_version: String` (semver, e.g. `"0.2.3"`). The server's config has:

```toml
[protocol]
min_client_version = "0.2.0"   # inclusive
```

If `client_version < min_client_version`, server replies `Error { code: "version_mismatch", min_version: "0.2.0" }`. The launcher displays "Please update — minimum required: 0.2.0" and links to the update endpoint.

### Auto-update manifest

Launcher hits `https://<server>/update_manifest.json` on startup:

```json
{
  "latest_version": "0.2.3",
  "min_supported": "0.2.0",
  "download_url": "https://<server>/builds/projectdawn-0.2.3.zip",
  "size_bytes": 158234112,
  "sha256": "...",
  "release_notes": "..."
}
```

If `current_version < latest_version`, launcher prompts to update and downloads on user confirm. Verifies sha256 before unzipping.

---

## Server restart & shutdown

`/gm shutdown <N>` where N is minutes:

- T-N: `BroadcastMessage { msg: "Server restarting in N minutes." }`
- T-1 minute: broadcast every 30 seconds.
- T-30 seconds: broadcast every 10 seconds.
- T-10 seconds: broadcast every 1 second.
- T = 0:
    1. Stop accepting new connections.
    2. Send `Kick { reason: "Server restarting", code: "restart", reconnect_after_secs: 30 }` to all clients.
    3. Persist all character state in one transaction-per-character.
    4. Close DB.
    5. Exit cleanly with code 0.
- systemd auto-restarts the process.

**Crash recovery**: if process panics, last persistence checkpoint is restored on next start. Active players reconnect; they lose at most 5 minutes of progress (periodic checkpoint cadence). Inventory is never lost since every mutation is its own transaction.

---

## Backup strategy

Nightly cron at 04:00 local time:

```bash
#!/usr/bin/env bash
# /opt/projectdawn/scripts/backup.sh
set -euo pipefail
DB=/var/lib/projectdawn/world.db
DEST=/var/lib/projectdawn/backups
mkdir -p "$DEST"
sqlite3 "$DB" ".backup '$DEST/world-$(date +%Y%m%d-%H%M%S).db'"
find "$DEST" -name 'world-*.db' -mtime +7 -delete
```

systemd timer or `cron.d`:

```
0 4 * * * projectdawn /opt/projectdawn/scripts/backup.sh
```

`.backup` uses SQLite's online backup API — safe to run while the server is writing. No downtime.

**Restore drill** (do this once before relying on it): copy a backup to `world.db`, start the server, log in, verify your character. Don't skip this.

---

## Anti-cheat posture

### Alpha stance

> Friends won't cheat. Server-authoritative protocol means they *can't* without major effort. We don't proactively detect or mitigate.

### What we do enforce from day one

These are protocol-level — they cost nothing to implement now and prevent entire classes of future bugs/abuse:

1. **Server is the only source of truth for game state.** Already covered above.
2. **Client sends intent only, never values.** No `"I dealt 47 damage"` packets. The protocol enums above are deliberately structured so this can't accidentally be added later.
3. **Inventory mutations are atomic transactions.** No split-state, no client-side pending. Eliminates dupe bugs by construction.
4. **Position validation**: server tracks `last_position` and `last_position_time` per player. Any client-sent move packet that implies speed > `max_speed * 1.5` is clamped to last valid position and a `position_anomaly` warning is logged. Doesn't trip on lag spikes; does flag obvious teleport.
5. **Action rate limits**: max 30 inventory ops/second/player, max 1 attack/0.5 s, etc. Configurable; rejects with `rate_limited` error.
6. **All GM actions audited.**

### What we add later (post-Steam-launch)

- Stricter position validation (account for terrain, jump arcs, gravity).
- Server-side replays of inputs (record the last 30s of inputs per player; reconstruct on demand).
- Per-account rate limit on logins (anti brute-force).
- Optional client integrity check (binary hash sent at login; server verifies against allowlist).
- Item duplication detection: nightly query for items with the same UUID in two locations. Should never find any with atomic transactions; safety net.

We deliberately do **not** ship kernel-level anti-cheat. It's hostile to users, expensive to maintain, and indie-scale projects don't need it.

---

## Launcher protocol

### Build

Godot 4 app, separate project at `Projects/launcher/` (separate from the game scene tree, but can share `data/` resources via symlink or copy at build time).

### UI sketch

1. **Splash / update check** — hit `update_manifest.json`, show progress bar if downloading.
2. **Login** — username field, password field (masked), "Log In" / "Register" buttons. Error label for auth failures.
3. **Char select** — list of characters from `LoginOk.characters`. Click to select. Buttons: **Play**, **Create**, **Delete**.
4. **Char create** — name, race dropdown, class dropdown, "Create" / "Cancel". Race/class restrictions enforced server-side; client just shows the full menu.
5. **Server status** (small text) — connected via Tailscale ✓, ping XXms.

### Play flow

User clicks **Play** with a character selected:

1. Launcher sends current `session_token` + `char_id` + `world_endpoint` to game `.exe` via:
    ```
    projectdawn.exe \
      --auth-token=<32-byte-hex> \
      --char-id=<id> \
      --world-endpoint=<host:port> \
      --client-version=<semver>
    ```
2. Launcher minimizes (or stays open for friends to chat / re-launch quickly).
3. Game `.exe` parses args, connects, plays.
4. When game exits, launcher returns to char-select.

### Auto-update

Launcher self-updates by downloading a new `launcher.exe` and replacing itself (Windows: rename old, write new, restart). Same `update_manifest.json` includes `launcher_version` and `launcher_url`.

---

## Repo layout

`Projects/server/` — Cargo workspace.

```
server/
├── Cargo.toml                     # workspace
├── README.md                      # build, deploy, run instructions
├── rust-toolchain.toml            # pinned rustc version
├── .env.example                   # config template
├── crates/
│   ├── protocol/                  # wire-format types (auth + world)
│   │   ├── Cargo.toml
│   │   └── src/
│   │       ├── lib.rs
│   │       ├── auth.rs            # JSON-serializable auth messages
│   │       └── world.rs           # bincode-serializable world messages
│   ├── projectdawn-server/        # the actual binary
│   │   ├── Cargo.toml
│   │   └── src/
│   │       ├── main.rs            # tokio runtime entry
│   │       ├── config.rs          # TOML config loader
│   │       ├── auth/              # WebSocket auth handler
│   │       ├── world/             # renet game handler + simulation
│   │       │   ├── tick.rs
│   │       │   ├── combat.rs
│   │       │   ├── inventory.rs
│   │       │   ├── ai.rs
│   │       │   └── replication.rs
│   │       ├── db/                # sqlx queries + migrations
│   │       └── gm/                # /gm command parser
│   └── shared/                    # gameplay constants, formulas
│       └── src/lib.rs
├── migrations/
│   ├── 0001_init.sql
│   └── 0002_add_languages.sql
├── scripts/
│   ├── backup.sh
│   ├── deploy.sh
│   └── dev-run.sh
└── tests/
    └── integration/
```

`Projects/launcher/` — separate Godot project (smaller scene tree, shares aesthetic).

`Projects/Project_Dawn/` — this repo, the game client.

### Cargo crate dependencies (initial sketch)

```toml
[workspace.dependencies]
tokio        = { version = "1", features = ["full"] }
renet        = "0.0.16"  # or latest 0.x
serde        = { version = "1", features = ["derive"] }
serde_json   = "1"
bincode      = "2"
sqlx         = { version = "0.7", features = ["runtime-tokio", "sqlite", "macros", "migrate", "chrono"] }
argon2       = "0.5"
rand         = "0.8"
tracing      = "0.1"
tracing-subscriber = "0.3"
tokio-tungstenite = "0.21"
glam         = "0.25"   # Vec3, math (matches Godot)
chrono       = { version = "0.4", features = ["serde"] }
anyhow       = "1"
thiserror    = "1"
toml         = "0.8"
```

Crate selection is pragmatic — swap any choice that doesn't pan out during the week-1 spike.

---

## Client migration path

Once the server is online, the GDScript client changes in three layers:

### Layer 1: net adapter (new)

`autoloads/net.gd` — single autoload owning the renet client (via Godot's native ENet client + a thin Rust shim, OR via a GDExtension wrapping `renet`'s client side). Sends intents; receives events; emits Godot signals.

This isolates ALL network code in one place. Existing autoloads subscribe to `Net.signal_x` instead of computing locally.

### Layer 2: autoload "shadows"

PlayerStats, Inventory, Equipment, etc. become **read-only mirrors** of server state. Their `save_state()` methods are dead-code-removed (server saves now). Mutations route through Net:

```gdscript
# Before
Inventory.add_item(item, count)

# After
Net.send(ClientMsg.MoveItem { ... })
# Inventory updates locally when Net emits inventory_changed in response
```

The autoload's *shape* doesn't change — combat math, UI bindings, signal names all stay. Only the *truth source* moves.

### Layer 3: deletions

- `autoloads/save_manager.gd` — deleted entirely (alpha builds keep it via a `--local-save` flag, but server builds drop it).
- `scripts/lobby.gd` Test Room / Test Dungeon flow — replaced by launcher-handoff flow. Local Test Room remains for dev-only `--local-save` mode.
- All client-side authoritative gameplay (`Combat.deal_damage`, `Loot._on_enemy_died`, etc.) moves into the Rust server. Client receives events, plays VFX, updates UI.

The `--local-save` flag preserves dev iteration: a developer can run the client without standing up a server, using local saves and client-side simulation, for quick gameplay tweaks. Friends-test builds ship without the flag.

---

## Open questions

These are deliberately deferred and need answers before the relevant work starts. Listed here so they don't get lost.

1. **Item identity**: should runtime-built unique items (e.g. crafted gear with custom names, named-mob drops with scaled stats) get server-issued UUIDs and live in their own table? Or stay as JSON snapshots in `inventory_slots.item_snapshot`? Snapshot is simpler; UUID enables item history / dupe-detection. Defer until first dupe-detection feature is requested.
2. **Pet persistence**: do summoned pets persist across logout? Beast Master warder probably yes; conjured magician pets probably no. Need design call.
3. **Character "respawn at bind" time penalty**: instant or 30-second resurrection sickness? Affects PvP later.
4. **Group state across logout**: if leader logs out, does group disband? Or hold the slot for a reconnect window?
5. **Banking / shared storage**: out of scope for alpha but the schema would need it. Earmark for later.
6. **Mail system**: same — defer until requested.
7. **Trade window between players**: secure trading is a non-trivial protocol (escrow + confirm). Add when first two players want to trade.
8. **Localization**: protocol is English-only for the alpha. Wrap UI text in `tr()` calls preemptively so the i18n migration is just a translation file later.
9. **Telemetry / analytics**: do we want anonymized gameplay stats (login frequency, level distribution, drop rates)? Privacy-respecting opt-in only. Defer.
10. **Test runner**: integration tests need a fixture server. Probably `cargo test` with a per-test SQLite file + an in-process server harness. Set up in week 2.
