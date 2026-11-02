//! Pets: summoning, following, commands, threat, the Beast Master's warder, and charm.

use crate::common::*;

/// Track 11 â€” server-side pet summon. Necromancer A casts Summon
/// Skeleton; the server spawns a player-owned pet entity, fans
/// `PetSpawn` to AOI peers, and B (in the same cell) receives the
/// broadcast carrying A's char_id as owner and a pet id in the
/// reserved partition.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pet_summon_visible_to_peer() {
    let h = start_both().await;

    let (a_session, a_char_id, a_token) =
        provision_client(&h.auth_url, "xio", "Xiora", "Human", "Necromancer").await;
    // Summon Skeleton requires level 6; provisioned characters start at 1.
    set_char_level(&h.db_url, a_char_id, 6).await;
    let (b_session, b_char_id, b_token) =
        provision_client(&h.auth_url, "yuu", "Yuusu", "Elf", "Cleric").await;

    let mut a = WorldClient::start(a_token, &a_session, a_char_id).await;
    let mut b = WorldClient::start(b_token, &b_session, b_char_id).await;

    // Let both clients settle into the world so EntitySpawn fan-outs
    // complete before we start kicking off the cast.
    for _ in 0..6 {
        tick_one(&mut a.client, &mut a.transport);
        tick_one(&mut b.client, &mut b.transport);
        tokio::time::sleep(TICK_DT).await;
    }

    // Summon Skeleton: cast_time 3.0s. Pump the start packet out
    // before sleeping (tokio::sleep doesn't advance the renet
    // transport â€” same gotcha as Track 10's gate tests).
    a.send_cast_start("Summon Skeleton", 3.0);
    for _ in 0..3 {
        tick_one(&mut a.client, &mut a.transport);
        tick_one(&mut b.client, &mut b.transport);
        tokio::time::sleep(TICK_DT).await;
    }
    a.pump_for(Duration::from_millis(3100)).await;
    a.send_cast_spell("Summon Skeleton", None);
    for _ in 0..6 {
        tick_one(&mut a.client, &mut a.transport);
        tick_one(&mut b.client, &mut b.transport);
        tokio::time::sleep(TICK_DT).await;
    }

    let pet_spawn = b
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(5), |m| {
            matches!(m, ServerWorldMsg::PetSpawn { owner, .. } if *owner == a_char_id as u64)
        })
        .await
        .expect("B receives PetSpawn fanned from A's Summon Skeleton cast");
    if let ServerWorldMsg::PetSpawn { id, owner, pet_name, level, max_hp, hp, .. } = pet_spawn {
        assert!(
            id >= PET_ID_BASE,
            "pet id must be in the pet partition (got {id}, base {PET_ID_BASE})"
        );
        assert_eq!(owner, a_char_id as u64);
        assert_eq!(pet_name, "Skeletal Warrior");
        // Owner-derived level (pet interim A): owner 6 -> base 5, minus
        // the 0..=2 manual-summon variance roll.
        assert!(
            (3..=5).contains(&level),
            "owner-6 skeleton rolls level 3-5 (got {level})"
        );
        // Stats ride the camp curve at PET_STAT_SCALAR (70%); the curve's
        // low levels are the parity audit's (37, 52 and 64 at 3, 4 and 5).
        let expected_hp = match level {
            3 => 25.9,
            4 => 36.4,
            5 => 44.8,
            _ => unreachable!(),
        };
        assert!(
            (max_hp - expected_hp).abs() < 0.1,
            "level-{level} pet hp follows the 70% camp curve (expected {expected_hp}, got {max_hp})"
        );
        assert!((hp - max_hp).abs() < 0.01, "fresh pet spawns at full hp");
    }
}

/// Track 11.2 â€” pet follows its owner across the world. Necromancer
/// summons the skeleton at spawn, then walks away. The peer sees
/// Position updates for the pet id closing the gap to the owner.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pet_follows_owner() {
    let h = start_both().await;

    let (a_session, a_char_id, a_token) =
        provision_client(&h.auth_url, "zii", "Ziorel", "Human", "Necromancer").await;
    // Summon Skeleton requires level 6; provisioned characters start at 1.
    set_char_level(&h.db_url, a_char_id, 6).await;
    let (b_session, b_char_id, b_token) =
        provision_client(&h.auth_url, "qqq", "Qqua", "Elf", "Cleric").await;

    let mut a = WorldClient::start(a_token, &a_session, a_char_id).await;
    let mut b = WorldClient::start(b_token, &b_session, b_char_id).await;

    // Settle both clients in-world.
    for _ in 0..6 {
        tick_one(&mut a.client, &mut a.transport);
        tick_one(&mut b.client, &mut b.transport);
        tokio::time::sleep(TICK_DT).await;
    }

    // Cast Summon Skeleton (cast_time 3.0s) following the gate flow.
    a.send_cast_start("Summon Skeleton", 3.0);
    for _ in 0..3 {
        tick_one(&mut a.client, &mut a.transport);
        tick_one(&mut b.client, &mut b.transport);
        tokio::time::sleep(TICK_DT).await;
    }
    a.pump_for(Duration::from_millis(3100)).await;
    a.send_cast_spell("Summon Skeleton", None);
    for _ in 0..6 {
        tick_one(&mut a.client, &mut a.transport);
        tick_one(&mut b.client, &mut b.transport);
        tokio::time::sleep(TICK_DT).await;
    }

    let pet_id: u64 = match b
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(5), |m| {
            matches!(m, ServerWorldMsg::PetSpawn { owner, .. } if *owner == a_char_id as u64)
        })
        .await
        .expect("B sees A's pet spawn")
    {
        ServerWorldMsg::PetSpawn { id, .. } => id,
        _ => unreachable!(),
    };

    // Owner walks ~5 m east over 2 s. PET_FOLLOW_DISTANCE = 3.0 m,
    // skeleton speed = 3.0 m/s, so the pet has plenty of headroom to
    // close the gap. B should observe at least one Position update
    // for the pet id with x > 0 (it spawned at owner_pos + 1.5 east,
    // so x starts > 0; we want to see x KEEP increasing as the owner
    // walks).
    let dir = Vec3 { x: 1.0, y: 0.0, z: 0.0 };
    let walk_end = Instant::now() + Duration::from_millis(2_500);
    let mut seq: u32 = 1;
    let mut max_pet_x: f32 = f32::MIN;
    while Instant::now() < walk_end {
        a.send_move(seq, dir);
        seq += 1;
        tick_one(&mut a.client, &mut a.transport);
        tick_one(&mut b.client, &mut b.transport);
        // Drain CHANNEL_POSITION on B for pet position updates as we
        // walk; latch the highest x value we see.
        while let Some(bytes) = b.client.receive_message(CHANNEL_POSITION) {
            if let Ok((msg, _)) = bincode::serde::decode_from_slice::<ServerWorldMsg, _>(
                &bytes,
                bincode_cfg(),
            ) {
                if let ServerWorldMsg::Position { id, pos, .. } = msg {
                    if id == pet_id {
                        max_pet_x = max_pet_x.max(pos.x);
                    }
                }
            }
        }
        tokio::time::sleep(TICK_DT).await;
    }

    // Pet spawned at ~(owner_pos.x + 1.5, _, _). Owner walked ~5 m
    // east; the pet should have moved well past its spawn x to keep
    // up. Use 3.0 as the threshold â€” generous to absorb tick jitter
    // and the FOLLOW_DISTANCE hysteresis at the boundary.
    assert!(
        max_pet_x > 3.0,
        "pet should follow owner east (saw max x = {max_pet_x})"
    );
}

/// Track 11.3 â€” pet inherits owner's attack target and damages the
/// enemy. Walk Necromancer A into camp 0 until an enemy is in melee,
/// summon the skeleton, A swings on the enemy once to seed
/// last_attacked_enemy, then the skeleton's swings produce Hit
/// broadcasts with the pet id as attacker.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pet_attacks_owners_target() {
    let h = start_both().await;

    let (a_session, a_char_id, a_token) =
        provision_client(&h.auth_url, "rrr", "Rune", "Human", "Necromancer").await;
    // Summon Skeleton requires level 6; provisioned characters start at 1.
    set_char_level(&h.db_url, a_char_id, 6).await;

    let mut a = WorldClient::start(a_token, &a_session, a_char_id).await;

    // Summon FIRST, in peace, before walking into aggro range â€” the same
    // reorder pet_command_attack_locks_onto_target got: casting the 3 s
    // summon while an enemy swings at you rolls a ~70% interrupt per hit
    // taken (channeling 0), which is why this test spent months on the
    // flaky list. Summoning before the pull does not weaken the test: the
    // point below is that the pet inherits the owner's target from ONE
    // seeding attack, which is unchanged.
    a.send_cast_start("Summon Skeleton", 3.0);
    for _ in 0..3 {
        tick_one(&mut a.client, &mut a.transport);
        tokio::time::sleep(TICK_DT).await;
    }
    a.pump_for(Duration::from_millis(3100)).await;
    a.send_cast_spell("Summon Skeleton", None);
    for _ in 0..3 {
        tick_one(&mut a.client, &mut a.transport);
        tokio::time::sleep(TICK_DT).await;
    }

    // Latch the pet id from the PetSpawn the server fans to A as
    // well (caster receives own PetSpawn â€” caller's AOI cell
    // includes themselves).
    let pet_id: u64 = match a
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
            matches!(m, ServerWorldMsg::PetSpawn { owner, .. } if *owner == a_char_id as u64)
                || matches!(m, ServerWorldMsg::CastFail { caster, .. } if *caster == a_char_id as u64)
        })
        .await
        .expect("A receives own PetSpawn (or a CastFail explaining why not)")
    {
        ServerWorldMsg::PetSpawn { id, .. } => id,
        ServerWorldMsg::CastFail { reason, .. } => {
            panic!("Summon Skeleton failed with CastFail: {reason:?}")
        }
        _ => unreachable!(),
    };

    // Pet in hand; NOW walk into the camp and take a hit to learn an
    // enemy id. Phase 4 layout note: aim at the Bonepile's isolated
    // [-16, 0, -14] spawn â€” the one ring 1 spawn whose aggro circle
    // overlaps no other, so exactly ONE slow level 1 Decrepit Skeleton
    // pulls, the single-puller semantics this test was written against.
    // Walk in on the server's own word, not the wall clock (a slow test
    // thread used to come up short): to 2 m from the spawn point, well
    // inside its aggro whatever the spawn jitter.
    let mut seq: u32 = 1;
    walk_until_within(&mut a, &mut seq, a_char_id, Vec3 { x: 0.0, y: 0.0, z: 0.0 }, BONEPILE_SPAWN, 2.0).await;

    // Wait for an enemy hit on us â€” proves the AI walked an enemy into
    // melee with us.
    let hit_evt = a
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(35), |m| {
            matches!(m, ServerWorldMsg::Hit { target, .. } if *target == a_char_id as u64)
        })
        .await
        .expect("an enemy locks on and swings");
    let enemy_id: u64 = match hit_evt {
        ServerWorldMsg::Hit { attacker, .. } => attacker,
        _ => unreachable!(),
    };

    // Send one Attack against the enemy to seed last_attacked_enemy.
    // Bare-handed swing; the server runs its own calc. We just need
    // the attack to land server-side so the pet inherits the target.
    a.send_attack(enemy_id, "", false, DamageType::Physical);
    for _ in 0..3 {
        tick_one(&mut a.client, &mut a.transport);
        tokio::time::sleep(TICK_DT).await;
    }

    // The skeleton (speed 3 m/s) needs a moment to close to melee +
    // its 2.2 s attack interval. Pump for up to ~8 s; once a Hit
    // arrives with attacker=pet_id, target=enemy_id, the inheritance
    // pipeline is proven end-to-end.
    let pet_hit = a
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(10), |m| {
            matches!(m, ServerWorldMsg::Hit { attacker, target, .. }
                if *attacker == pet_id && *target == enemy_id)
        })
        .await
        .expect("skeleton inherits target and lands a Hit on the enemy");
    if let ServerWorldMsg::Hit { amount, .. } = pet_hit {
        // Owner-derived stats: owner 6 -> level 3-5 -> dmg 5-7 (70% of
        // the camp curve's 7/9/10, rounded).
        assert!(
            (5..=7).contains(&amount),
            "owner-6 skeleton swings for 5-7 (got {amount})"
        );
    }
}

/// Track 12 Piece A â€” explicit `/pet attack` command locks the pet
/// onto a specific enemy id, bypassing the `last_attacked_enemy`
/// inheritance pipeline. Necromancer summons, then commands the
/// pet to attack an enemy WITHOUT first hitting it themselves;
/// assert a Hit with attacker=pet, target=that enemy arrives.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pet_command_attack_locks_onto_target() {
    let h = start_both().await;

    let (a_session, a_char_id, a_token) =
        provision_client(&h.auth_url, "sss", "Suun", "Human", "Necromancer").await;
    // Summon Skeleton requires level 6; provisioned characters start at 1.
    set_char_level(&h.db_url, a_char_id, 6).await;

    let mut a = WorldClient::start(a_token, &a_session, a_char_id).await;

    // Summon FIRST, in peace, before walking into aggro range.
    //
    // Ordering matters: this used to walk in, wait to be hit, and only then
    // start the 3 s Summon Skeleton cast â€” i.e. it cast while an enemy was
    // actively swinging at it. Track 19A's on-hit cast interrupt
    // (`roll_cast_interrupt`) then cleared the cast, so the pet never spawned.
    // That is not a fixable-by-tuning race: the interrupt chance is
    // `max(0.10, 0.70 - channeling_ratio * 0.60)`, which is ~0.70 for a level-6
    // caster and never drops below 0.10 even at max skill, so casting under fire
    // is inherently unreliable. Summoning before the pull avoids the interrupt
    // entirely and does not weaken the test: the point below is that the player
    // never attacks the enemy themselves, which is still true.
    a.send_cast_start("Summon Skeleton", 3.0);
    for _ in 0..3 {
        tick_one(&mut a.client, &mut a.transport);
        tokio::time::sleep(TICK_DT).await;
    }
    a.pump_for(Duration::from_millis(3100)).await;
    a.send_cast_spell("Summon Skeleton", None);
    for _ in 0..3 {
        tick_one(&mut a.client, &mut a.transport);
        tokio::time::sleep(TICK_DT).await;
    }
    let pet_id: u64 = match a
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
            matches!(m, ServerWorldMsg::PetSpawn { owner, .. } if *owner == a_char_id as u64)
                || matches!(m, ServerWorldMsg::CastFail { caster, .. } if *caster == a_char_id as u64)
        })
        .await
        .expect("A receives own PetSpawn (or a CastFail explaining why not)")
    {
        ServerWorldMsg::PetSpawn { id, .. } => id,
        ServerWorldMsg::CastFail { reason, .. } => {
            panic!("Summon Skeleton failed with CastFail: {reason:?}")
        }
        _ => unreachable!(),
    };

    // Now walk into camp 0; wait until at least one enemy aggros and
    // hits the player so we have an enemy id to command on.
    // Phase 4 layout note: every camp-walking test aims at the Bonepile's
    // isolated [-16, 0, -14] spawn â€” the one ring 1 spawn whose aggro circle
    // overlaps no other, so exactly ONE slow (1.8 m/s, 2.5 s swing) level 1
    // Decrepit Skeleton pulls, the same single-puller semantics these tests
    // were written against. Do NOT aim at the Wolf Run: it is a four-wolf
    // pack, and standing in it turns every cast into interrupt rolls.
    // 3.5 s, not 2 s: at 2 s the player only clips camp 0's aggro radius, so
    // whether an enemy locks on before the wait expires depended on where it
    // happened to be wandering. Walking fully in makes the pull deterministic.
    // Walk in on the server's own word, not the wall clock (a slow test
    // thread used to come up short): to 2 m from the spawn point, well
    // inside its aggro whatever the spawn jitter.
    let mut seq: u32 = 1;
    walk_until_within(&mut a, &mut seq, a_char_id, Vec3 { x: 0.0, y: 0.0, z: 0.0 }, BONEPILE_SPAWN, 2.0).await;
    let hit_evt = a
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(35), |m| {
            matches!(m, ServerWorldMsg::Hit { target, .. } if *target == a_char_id as u64)
        })
        .await
        .expect("an enemy aggros and hits the player");
    let enemy_id: u64 = match hit_evt {
        ServerWorldMsg::Hit { attacker, .. } => attacker,
        _ => unreachable!(),
    };

    // Issue ATTACK command. The player has NOT attacked the enemy
    // themselves (only the enemy has attacked them) so without the
    // explicit command, the pet would default to follow.
    a.send_pet_command(protocol::world::pet_command::ATTACK, Some(enemy_id));
    for _ in 0..3 {
        tick_one(&mut a.client, &mut a.transport);
        tokio::time::sleep(TICK_DT).await;
    }

    let pet_hit = a
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(10), |m| {
            matches!(m, ServerWorldMsg::Hit { attacker, target, .. }
                if *attacker == pet_id && *target == enemy_id)
        })
        .await
        .expect("pet attacks the commanded target");
    if let ServerWorldMsg::Hit { amount, .. } = pet_hit {
        // Owner-derived stats: owner 6 -> level 3-5 -> dmg 5-7.
        assert!(
            (5..=7).contains(&amount),
            "owner-6 skeleton swings for 5-7 (got {amount})"
        );
    }
}

/// Track 12 Piece A2: the pet pulls aggro through the threat re-evaluation.
/// A mob targets the player, the pet is sent on it, and once the pet's
/// threat clears 1.3x the player's the mob switches to the pet, fanning an
/// EntityTarget. The mob is the test's own dev spawn (500 HP, 1 damage),
/// not a camp mob: since the EQ parity pass a camp's Decrepit Skeleton has
/// 16 HP and the pet killed it before the switch could happen, so the test
/// read camp numbers instead of the threat rule.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pet_pulls_aggro_via_threat_reaggro() {
    let h = start_both().await;

    let (a_session, a_char_id, _stale_token) =
        provision_client(&h.auth_url, "tnk", "Tanker", "Human", "Necromancer").await;
    // Summon Skeleton requires level 6; provisioned characters start at 1.
    set_char_level(&h.db_url, a_char_id, 6).await;
    // GM for the dev spawn; the token is minted after the grant.
    let pool = db::open(&h.db_url).await.expect("open pool");
    db::set_account_gm(&pool, "tnk", true).await.expect("set is_gm");
    let a_token = request_world_token(&h.auth_url, &a_session, a_char_id).await;

    let mut a = WorldClient::start(a_token, &a_session, a_char_id).await;

    // Summon first, in peace: casting the 3 s summon while a mob swings at
    // you rolls an interrupt per hit taken (channeling 0).
    a.send_cast_start("Summon Skeleton", 3.0);
    for _ in 0..3 {
        tick_one(&mut a.client, &mut a.transport);
        tokio::time::sleep(TICK_DT).await;
    }
    a.pump_for(Duration::from_millis(3100)).await;
    a.send_cast_spell("Summon Skeleton", None);
    for _ in 0..3 {
        tick_one(&mut a.client, &mut a.transport);
        tokio::time::sleep(TICK_DT).await;
    }
    let pet_id: u64 = match a
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
            matches!(m, ServerWorldMsg::PetSpawn { owner, .. } if *owner == a_char_id as u64)
        })
        .await
        .expect("PetSpawn for own pet")
    {
        ServerWorldMsg::PetSpawn { id, .. } => id,
        _ => unreachable!(),
    };

    // A sturdy target 3 m away that notices the player at once and walks
    // into melee: it outlives the whole exchange, and its 1 damage a swing
    // cannot kill the player first.
    a.send_dev_spawn("Threat Dummy", 1, 500.0, 1, 2.0, 10.0);

    // Wait for enemy to lock on us and start swinging. We capture
    // the enemy id and confirm the player is the target.
    let initial_target_evt = a
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(35), |m| {
            matches!(m, ServerWorldMsg::EntityTarget { target: Some(t), .. }
                if *t == a_char_id as u64)
        })
        .await
        .expect("enemy targets the player initially");
    let enemy_id: u64 = match initial_target_evt {
        ServerWorldMsg::EntityTarget { id, .. } => id,
        _ => unreachable!(),
    };

    // One player swing first gives the player real threat, so the switch is
    // the 1.3x rule at work and not a pull off an empty list; the pet is sent
    // in only after it, so no switch can happen (and be skipped by a wait)
    // before the test listens for it. The swing waits for the dummy to reach
    // melee and hit the player, so it is in reach.
    let in_melee = a
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(5), |m| {
            matches!(m, ServerWorldMsg::Hit { attacker, target, .. }
                if *attacker == enemy_id && *target == a_char_id as u64)
        })
        .await;
    assert!(in_melee.is_some(), "the dummy walks into melee and swings at the player");
    a.send_attack(enemy_id, "", false, DamageType::Physical);
    for _ in 0..3 {
        tick_one(&mut a.client, &mut a.transport);
        tokio::time::sleep(TICK_DT).await;
    }

    // Now lock the pet onto the enemy via /pet attack.
    a.send_pet_command(protocol::world::pet_command::ATTACK, Some(enemy_id));

    let switch_evt = a
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(35), |m| {
            matches!(m, ServerWorldMsg::EntityTarget { id, target: Some(t), .. }
                if *id == enemy_id && *t == pet_id)
        })
        .await
        .expect("enemy re-targets onto the pet once threat passes 1.3Ã— the player's");
    let _ = switch_evt;
}

/// Track 12 Piece C â€” Enchanter's Charm converts a targeted enemy
/// into a player-owned pet. Server fan-outs: EntityDespawn for the
/// old enemy id, PetSpawn for a fresh pet id at the same pos with
/// owner == caster.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn charm_converts_enemy_to_pet() {
    let h = start_both().await;

    let (a_session, a_char_id, _stale_token) =
        provision_client(&h.auth_url, "ench", "Enchanted", "Human", "Enchanter").await;
    // Charm requires level 20; provisioned characters start at 1.
    set_char_level(&h.db_url, a_char_id, 20).await;
    // GM for the dev spawn below; re-mint the token so the flag rides it.
    let pool = db::open(&h.db_url).await.expect("open pool");
    db::set_account_gm(&pool, "ench", true).await.expect("set is_gm");
    let a_token = request_world_token(&h.auth_url, &a_session, a_char_id).await;

    let mut a = WorldClient::start(a_token, &a_session, a_char_id).await;

    // Charm a mob that is NOT attacking. This test used to walk into the
    // Bonepile, wait for the skeleton's hit, then cast the 2.0 s Charm while
    // that skeleton kept swinging every 2.5 s: the cast finished about 0.2 s
    // before the next swing, so any stretch of the test loop landed a hit
    // mid-cast and rolled the ~70% channeling-0 interrupt. That race was the
    // whole of its "load sensitivity". A dev-spawned mob with aggro 0 and
    // speed 0 never targets or swings, so an interrupt is impossible rather
    // than unlikely (the same fix the AOE test got on 2026-09-16).
    a.send_dev_spawn("Charm Dummy", 1, 30.0, 0, 0.0, 0.0);
    let spawn_evt = a
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
            matches!(m, ServerWorldMsg::EnemySpawn { mob_name, .. } if mob_name == "Charm Dummy")
        })
        .await
        .expect("the dev-spawned dummy fans an EnemySpawn");
    let enemy_id: u64 = match spawn_evt {
        ServerWorldMsg::EnemySpawn { id, .. } => id,
        _ => unreachable!(),
    };

    // Cast Charm (cast_time 2.0s, mana 40) following the gate flow.
    a.send_cast_start("Charm", 2.0);
    for _ in 0..3 {
        tick_one(&mut a.client, &mut a.transport);
        tokio::time::sleep(TICK_DT).await;
    }
    a.pump_for(Duration::from_millis(2100)).await;
    a.send_cast_spell("Charm", Some(enemy_id));
    for _ in 0..6 {
        tick_one(&mut a.client, &mut a.transport);
        tokio::time::sleep(TICK_DT).await;
    }

    let despawn = a
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(5), |m| {
            matches!(m, ServerWorldMsg::EntityDespawn { id } if *id == enemy_id)
        })
        .await
        .expect("old enemy id is despawned on charm");
    let _ = despawn;

    let pet_spawn = a
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(5), |m| {
            matches!(m, ServerWorldMsg::PetSpawn { owner, .. } if *owner == a_char_id as u64)
        })
        .await
        .expect("a fresh pet id spawns for the caster");
    if let ServerWorldMsg::PetSpawn { id, pet_name, .. } = pet_spawn {
        assert!(id >= PET_ID_BASE, "charmed pet id must be in pet partition");
        // Mob name preserved: the charmed dummy keeps its name on the
        // pet entity.
        assert_eq!(pet_name, "Charm Dummy");
        // And the charm HOLDS. The decay sweep used to compare with a
        // saturating duration, so every charm "expired" 50 ms after it
        // landed (seen in play 2026-10-06). Charm lasts 60 s; nothing may
        // despawn the pet in the next second and a half.
        let early_release = a
            .wait_for(CHANNEL_SYSTEM, Duration::from_millis(1500), |m| {
                matches!(m, ServerWorldMsg::EntityDespawn { id: gone } if *gone == id)
            })
            .await;
        assert!(early_release.is_none(), "a fresh charm must not release on the next tick");
    }
}

/// A charm that ends hands the mob back (user call 2026-10-06, EQ's rule): the
/// pet is despawned, the same mob respawns in its place with the HP it had,
/// and it comes straight for the one who charmed it. Siren's Song is the
/// shortest charm (30 s), so the wait is real time; heartbeats keep the
/// app-layer idle timeout quiet meanwhile. Fails on the previous code at the
/// EnemySpawn: the old sweep deleted the pet and spawned nothing.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_expired_charm_hands_the_mob_back_and_it_turns_on_the_charmer() {
    let h = start_both().await;
    let (a_session, a_char_id, _stale_token) =
        provision_client(&h.auth_url, "siren", "Sirenna", "Human", "Bard").await;
    // Siren's Song is Bard level 14.
    set_char_level(&h.db_url, a_char_id, 14).await;
    let pool = db::open(&h.db_url).await.expect("open pool");
    db::set_account_gm(&pool, "siren", true).await.expect("set is_gm");
    let a_token = request_world_token(&h.auth_url, &a_session, a_char_id).await;
    let mut a = WorldClient::start(a_token, &a_session, a_char_id).await;

    // Aggro 1 m: it does not notice the bard 3 m away before the charm (so
    // the 1.5 s cast cannot be interrupted) and has a real leash afterwards;
    // speed 2 and dmg 5 so, once returned, it can reach and hit the bard.
    a.send_dev_spawn("Charm Dummy", 1, 500.0, 5, 2.0, 1.0);
    let spawn_evt = a
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
            matches!(m, ServerWorldMsg::EnemySpawn { mob_name, .. } if mob_name == "Charm Dummy")
        })
        .await
        .expect("the dummy spawns");
    let enemy_id: u64 = match spawn_evt {
        ServerWorldMsg::EnemySpawn { id, replaces, .. } => {
            assert_eq!(replaces, None, "an ordinary spawn replaces nothing");
            id
        }
        _ => unreachable!(),
    };

    a.send_cast_start("Siren's Song", 1.5);
    a.pump_for(Duration::from_millis(1700)).await;
    a.send_cast_spell("Siren's Song", Some(enemy_id));
    let pet_spawn = a
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(5), |m| {
            matches!(m, ServerWorldMsg::PetSpawn { owner, .. } if *owner == a_char_id as u64)
        })
        .await
        .expect("the charm lands");
    // One creature throughout (EQ): the pet names the enemy it was, so a
    // client that had the mob targeted keeps it targeted.
    let pet_id: u64 = match pet_spawn {
        ServerWorldMsg::PetSpawn { id, replaces, .. } => {
            assert_eq!(replaces, Some(enemy_id), "the charmed pet replaces the enemy id");
            id
        }
        _ => unreachable!(),
    };

    // 30 s of charm. A heartbeat every few seconds so the 10 s idle timeout
    // does not reap the bard first.
    let deadline = Instant::now() + Duration::from_secs(40);
    let mut released = None;
    while released.is_none() && Instant::now() < deadline {
        a.send_heartbeat();
        released = a
            .wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
                matches!(m, ServerWorldMsg::EntityDespawn { id } if *id == pet_id)
            })
            .await;
    }
    assert!(released.is_some(), "the charm ends and the pet is despawned");

    let back = a
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
            matches!(m, ServerWorldMsg::EnemySpawn { mob_name, .. } if mob_name == "Charm Dummy")
        })
        .await
        .expect("the mob comes back as an enemy where the pet stood");
    let (mob_id, hp) = match back {
        ServerWorldMsg::EnemySpawn { id, hp, replaces, .. } => {
            assert_eq!(replaces, Some(pet_id), "the returned mob replaces the pet id");
            (id, hp)
        }
        _ => unreachable!(),
    };
    assert_ne!(mob_id, enemy_id, "a fresh enemy id; the old one is never reused");
    assert!((hp - 500.0).abs() < 1e-3, "it keeps the HP it had, got {hp}");

    a.send_heartbeat();
    let turned = a
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
            matches!(m, ServerWorldMsg::EntityTarget { id, target: Some(t) }
                if *id == mob_id && *t == a_char_id as u64)
        })
        .await;
    assert!(turned.is_some(), "the returned mob targets its former charmer");
    let hit = a
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(10), |m| {
            matches!(m, ServerWorldMsg::Hit { attacker, target, .. }
                if *attacker == mob_id && *target == a_char_id as u64)
        })
        .await;
    assert!(hit.is_some(), "and comes for them: a hit on the bard within seconds");
}

/// Track 12 Piece B â€” Beast Masters auto-summon a Wolf warder when
/// they enter the world. No PET_SUMMON cast required; the server
/// detects the class on first EnterWorld and spawns the warder
/// alongside the EntitySpawn fan-out.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn beast_master_auto_summons_warder() {
    let h = start_both().await;

    let (a_session, a_char_id, a_token) =
        provision_client(&h.auth_url, "bms", "Beastly", "Human", "Beast Master").await;

    // The exact scenario that opened the To-Do item: a level 22 Beast
    // Master whose warder was stuck at the static level 5.
    set_char_level(&h.db_url, a_char_id, 22).await;
    let mut a = WorldClient::start(a_token, &a_session, a_char_id).await;

    let pet_spawn = a
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(5), |m| {
            matches!(m, ServerWorldMsg::PetSpawn { owner, .. } if *owner == a_char_id as u64)
        })
        .await
        .expect("Beast Master receives an auto-summoned warder on EnterWorld");
    if let ServerWorldMsg::PetSpawn { pet_name, level, max_hp, hp, .. } = pet_spawn {
        assert_eq!(pet_name, "Wolf", "Beast Master's auto-summon is a Wolf warder");
        // Deterministic owner - 1: the free auto-summon takes no variance
        // roll (pet interim A).
        assert_eq!(level, 21, "warder tracks its owner: 22 - 1");
        // 70% of the extrapolated camp curve at 21: (365 + 50*7) * 0.7.
        assert!(
            (max_hp - 500.5).abs() < 0.1,
            "level-21 warder hp rides the 70% curve (got {max_hp})"
        );
        assert!((hp - max_hp).abs() < 0.01, "auto-summon spawns at full HP");
    }
}

/// Warder's Mend was the one client-only pet spell: the server had no PET_HEAL
/// arm and refused it as "no effect yet" (playtest 2026-10-07; the tester asked
/// for no client-only pet spells). It now mends the caster's own pet, whatever
/// is targeted. Fails on the previous code: no HealthUpdate for the warder
/// ever arrives.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn warders_mend_heals_the_beast_masters_own_warder() {
    let h = start_both().await;
    let (a_session, a_char_id, a_token) =
        provision_client(&h.auth_url, "mender", "Mendra", "Human", "Beast Master").await;
    set_char_level(&h.db_url, a_char_id, 22).await;
    let mut a = WorldClient::start(a_token, &a_session, a_char_id).await;

    let pet_spawn = a
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(5), |m| {
            matches!(m, ServerWorldMsg::PetSpawn { owner, .. } if *owner == a_char_id as u64)
        })
        .await
        .expect("the warder auto-summons");
    let pet_id: u64 = match pet_spawn {
        ServerWorldMsg::PetSpawn { id, .. } => id,
        _ => unreachable!(),
    };

    // No target at all: the spell finds the caster's own pet.
    a.send_cast_start("Warder's Mend", 1.5);
    a.pump_for(Duration::from_millis(1700)).await;
    a.send_cast_spell("Warder's Mend", None);
    let mended = a
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(5), |m| {
            matches!(m, ServerWorldMsg::HealthUpdate { id, .. } if *id == pet_id)
        })
        .await;
    let Some(ServerWorldMsg::HealthUpdate { hp, max_hp, .. }) = mended else {
        panic!("the server mends the warder and fans its health");
    };
    assert!((hp - max_hp).abs() < 0.01, "an unhurt warder stays at full, got {hp}/{max_hp}");
}

/// Track 12 Piece B â€” non-Beast-Master classes do NOT get an
/// auto-summoned warder. Counter-test to make sure the class check
/// is wired correctly.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn non_beast_master_gets_no_auto_warder() {
    let h = start_both().await;

    let (a_session, a_char_id, a_token) =
        provision_client(&h.auth_url, "wrr", "Warlock", "Human", "Warrior").await;

    let mut a = WorldClient::start(a_token, &a_session, a_char_id).await;

    let stray = a
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(2), |m| {
            matches!(m, ServerWorldMsg::PetSpawn { owner, .. } if *owner == a_char_id as u64)
        })
        .await;
    assert!(stray.is_none(), "Warrior must not receive an auto-summoned pet");
}

/// The pet arm of the same crossing: a pet following its owner across a cell
/// boundary fans a PetSpawn to the players it comes into view of.
#[tokio::test]
async fn pet_following_its_owner_across_a_cell_spawns_for_players() {
    let h = start_both().await;
    let (a_session, a_char_id, a_token) =
        provision_client(&h.auth_url, "petwatch", "Petwatch", "Human", "Warrior").await;
    let (b_session, b_char_id, b_token) =
        provision_client(&h.auth_url, "petwalk", "Petwalk", "Human", "Necromancer").await;
    set_char_level(&h.db_url, b_char_id, 6).await;
    set_char_pos(&h.db_url, b_char_id, 248.0, 60.0).await;

    let mut a = WorldClient::start(a_token, &a_session, a_char_id).await;
    let mut b = WorldClient::start(b_token, &b_session, b_char_id).await;
    pump_both_for(&mut a, &mut b, Duration::from_millis(300)).await;

    let pet_id = summon_skeleton(&mut b, &mut a, b_char_id).await;
    assert!(pet_id >= PET_ID_BASE);
    let early = wait_on_a_pumping_b(&mut a, &mut b, Duration::from_millis(300), |m| {
        matches!(m, ServerWorldMsg::PetSpawn { id, .. } if *id == pet_id)
    })
    .await;
    assert!(early.is_none(), "a pet two cells away must not be seeded to A");

    let mut seq: u32 = 1;
    walk_pumping(&mut b, &mut a, &mut seq, Vec3 { x: -1.0, y: 0.0, z: 0.0 }, 40).await;
    let spawned = wait_on_a_pumping_b(&mut a, &mut b, Duration::from_secs(15), |m| {
        matches!(m, ServerWorldMsg::PetSpawn { id, owner, .. }
            if *id == pet_id && *owner == b_char_id as u64)
    })
    .await;
    assert!(
        spawned.is_some(),
        "A must get a PetSpawn when the pet follows its owner into a cell adjacent to A's"
    );
}

/// Charm had no range check: any live enemy id in the world could be charmed
/// from anywhere. This is that exploit as a client would run it: B, seeded two
/// cells away, conjures a dummy there; A, standing at the starter spawn and
/// never even told the dummy exists, names its id in a Charm. It must be
/// refused with the line, and the dummy must still be standing.
///
/// Nobody walks: an earlier shape walked the caster away from a dummy, and
/// under a loaded run the walk covered more ground in wall-clock time, strayed
/// into a camp, and the cast was interrupted by a mob instead.
#[tokio::test]
async fn charm_refuses_a_target_out_of_range() {
    let h = start_both().await;
    let (a_session, a_char_id, _stale_a) =
        provision_client(&h.auth_url, "farench", "Farench", "Human", "Enchanter").await;
    set_char_level(&h.db_url, a_char_id, 20).await;
    let (b_session, b_char_id, _stale_b) =
        provision_client(&h.auth_url, "farspawn", "Farspawn", "Human", "Warrior").await;
    let pool = db::open(&h.db_url).await.expect("open pool");
    // Both are GMs: B conjures the far dummy, A conjures a near one at the end.
    db::set_account_gm(&pool, "farench", true).await.expect("set is_gm");
    db::set_account_gm(&pool, "farspawn", true).await.expect("set is_gm");
    set_char_pos(&h.db_url, b_char_id, 248.0, 60.0).await;
    let a_token = request_world_token(&h.auth_url, &a_session, a_char_id).await;
    let b_token = request_world_token(&h.auth_url, &b_session, b_char_id).await;

    let mut a = WorldClient::start(a_token, &a_session, a_char_id).await;
    let mut b = WorldClient::start(b_token, &b_session, b_char_id).await;
    pump_both_for(&mut a, &mut b, Duration::from_millis(300)).await;

    b.send_dev_spawn("Far Dummy", 1, 30.0, 0, 0.0, 0.0);
    let spawn_evt = wait_on_a_pumping_b(&mut b, &mut a, Duration::from_secs(3), |m| {
        matches!(m, ServerWorldMsg::EnemySpawn { mob_name, .. } if mob_name == "Far Dummy")
    })
    .await
    .expect("the dummy spawns beside B");
    let enemy_id: u64 = match spawn_evt {
        ServerWorldMsg::EnemySpawn { id, .. } => id,
        _ => unreachable!(),
    };

    a.send_cast_start("Charm", 2.0);
    pump_both_for(&mut a, &mut b, Duration::from_millis(2150)).await;
    a.send_cast_spell("Charm", Some(enemy_id));
    wait_on_a_pumping_b(&mut a, &mut b, Duration::from_secs(5), |m| {
        matches!(m, ServerWorldMsg::CastFail { reason, .. } if reason.contains("too far away"))
    })
    .await
    .expect("an out-of-range charm is refused with the line");

    // Nothing was charmed: no pet for A, and B never sees its dummy despawn.
    let pet = wait_on_a_pumping_b(&mut a, &mut b, Duration::from_millis(600), |m| {
        matches!(m, ServerWorldMsg::PetSpawn { owner, .. } if *owner == a_char_id as u64)
    })
    .await;
    assert!(pet.is_none(), "nothing is charmed from out of range");
    let gone = wait_on_a_pumping_b(&mut b, &mut a, Duration::from_millis(300), |m| {
        matches!(m, ServerWorldMsg::EntityDespawn { id } if *id == enemy_id)
    })
    .await;
    assert!(gone.is_none(), "the dummy is still standing where B conjured it");

    // A refused cast costs NOTHING, cooldown included. Charm's cooldown is
    // 60 s; when refusals were handled after the stamp, the cast above would
    // have locked Charm out for a minute. A charms a dummy at its feet at once.
    a.send_dev_spawn("Near Dummy", 1, 30.0, 0, 0.0, 0.0);
    let near_evt = wait_on_a_pumping_b(&mut a, &mut b, Duration::from_secs(3), |m| {
        matches!(m, ServerWorldMsg::EnemySpawn { mob_name, .. } if mob_name == "Near Dummy")
    })
    .await
    .expect("the near dummy spawns beside A");
    let near_id: u64 = match near_evt {
        ServerWorldMsg::EnemySpawn { id, .. } => id,
        _ => unreachable!(),
    };
    a.send_cast_start("Charm", 2.0);
    pump_both_for(&mut a, &mut b, Duration::from_millis(2150)).await;
    a.send_cast_spell("Charm", Some(near_id));
    wait_on_a_pumping_b(&mut a, &mut b, Duration::from_secs(5), |m| {
        matches!(m, ServerWorldMsg::PetSpawn { owner, .. } if *owner == a_char_id as u64)
    })
    .await
    .expect("the refused cast burned no cooldown: the next Charm lands");
}

/// The case that matters in play: charming a spawner-owned camp mob that is
/// aggroed and mid-fight. The dummy test above proves the conversion without
/// timing risk; this one keeps a live target, a threat table and a running
/// swing timer under the charm. The skeleton fights B, so nothing swings at
/// the caster and no interrupt can roll: the old single-client shape raced
/// the mob's own swing timer.
#[tokio::test]
async fn charm_converts_a_camp_mob_that_is_fighting_someone_else() {
    let h = start_both().await;
    let (a_session, a_char_id, a_token) =
        provision_client(&h.auth_url, "campench", "Campench", "Human", "Enchanter").await;
    set_char_level(&h.db_url, a_char_id, 20).await;
    let (b_session, b_char_id, b_token) =
        provision_client(&h.auth_url, "camptank", "Camptank", "Human", "Warrior").await;

    let mut a = WorldClient::start(a_token, &a_session, a_char_id).await;
    let mut b = WorldClient::start(b_token, &b_session, b_char_id).await;
    pump_both_for(&mut a, &mut b, Duration::from_millis(300)).await;

    // B walks 18 m toward the Bonepile's isolated [-16, -14] spawn (21 m from
    // the starter spawn, aggro 8) and stops: inside its aggro circle, and the
    // fight then happens under 20 m from A, who stays at spawn, outside that
    // circle and inside charm range (25 m).
    let len: f32 = (16.0_f32 * 16.0 + 14.0_f32 * 14.0).sqrt();
    let dir = Vec3 { x: -16.0 / len, y: 0.0, z: -14.0 / len };
    let mut seq: u32 = 1;
    let moves = (18.0 / (world::RUN_SPEED * 0.05)).ceil() as u32;
    walk_pumping(&mut b, &mut a, &mut seq, dir, moves).await;
    b.send_move(seq, Vec3 { x: 0.0, y: 0.0, z: 0.0 });

    let hit_evt = wait_on_a_pumping_b(&mut b, &mut a, Duration::from_secs(35), |m| {
        matches!(m, ServerWorldMsg::Hit { target, .. } if *target == b_char_id as u64)
    })
    .await
    .expect("the skeleton engages B");
    let enemy_id: u64 = match hit_evt {
        ServerWorldMsg::Hit { attacker, .. } => attacker,
        _ => unreachable!(),
    };

    a.send_cast_start("Charm", 2.0);
    pump_both_for(&mut a, &mut b, Duration::from_millis(2150)).await;
    a.send_cast_spell("Charm", Some(enemy_id));

    wait_on_a_pumping_b(&mut a, &mut b, Duration::from_secs(5), |m| {
        matches!(m, ServerWorldMsg::EntityDespawn { id } if *id == enemy_id)
    })
    .await
    .expect("the fighting mob's enemy id despawns on charm");
    let pet_spawn = wait_on_a_pumping_b(&mut a, &mut b, Duration::from_secs(5), |m| {
        matches!(m, ServerWorldMsg::PetSpawn { owner, .. } if *owner == a_char_id as u64)
    })
    .await
    .expect("it comes back as the caster's pet");
    if let ServerWorldMsg::PetSpawn { id, pet_name, .. } = pet_spawn {
        assert!(id >= PET_ID_BASE, "charmed pet id must be in the pet partition");
        assert_eq!(pet_name, "Decrepit Skeleton");
    }
}

/// User call D4 (2026-10-05): the pet comes along on Gate and STAYS. The
/// review of the first cut found the port arm cleared the pet's target but
/// not the owner's `last_attacked_enemy`, so the pet pre-pass re-inherited
/// the mob within 10 s and the pet walked straight back out of town to it,
/// fighting unsupervised (and a pet kill credits the owner). Necromancer 8
/// (Summon Skeleton at 6, Gate at 8), born bound at the spawn, starts 15 m
/// out; the dummy cannot move or hit, so only the pet engages.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_pet_stays_with_its_owner_after_gate() {
    let h = start_both().await;
    let (a_session, a_char_id, _stale) =
        provision_client(&h.auth_url, "petgate", "Morrow", "Human", "Necromancer").await;
    set_char_level(&h.db_url, a_char_id, 8).await;
    set_char_pos(&h.db_url, a_char_id, 15.0, 0.0).await;
    let pool = db::open(&h.db_url).await.expect("open pool");
    db::set_account_gm(&pool, "petgate", true).await.expect("set is_gm");
    let a_token = request_world_token(&h.auth_url, &a_session, a_char_id).await;
    let mut a = WorldClient::start(a_token, &a_session, a_char_id).await;
    a.pump_for(Duration::from_millis(300)).await;

    a.send_cast_start("Summon Skeleton", 3.0);
    a.pump_for(Duration::from_millis(3_000) + CAST_RELEASE_MARGIN).await;
    a.send_cast_spell("Summon Skeleton", None);
    let pet_spawn = a
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
            matches!(m, ServerWorldMsg::PetSpawn { owner, .. } if *owner == a_char_id as u64)
        })
        .await
        .expect("the skeleton is summoned");
    let pet_id: u64 = match pet_spawn {
        ServerWorldMsg::PetSpawn { id, .. } => id,
        _ => unreachable!(),
    };

    // A dummy 3 m ahead; one swing from the owner and the pet inherits it.
    a.send_dev_spawn("Charm Dummy", 1, 500.0, 0, 0.0, 1.0);
    let spawn = a
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
            matches!(m, ServerWorldMsg::EnemySpawn { mob_name, .. } if mob_name == "Charm Dummy")
        })
        .await
        .expect("the dummy spawns");
    let (dummy, dummy_pos) = match spawn {
        ServerWorldMsg::EnemySpawn { id, pos, .. } => (id, pos),
        _ => unreachable!(),
    };
    // Bare-handed reach is 2.7 m and the dummy lands 3 m off: step in, on
    // the server's word for where the owner stands. A wall-clock walk
    // (`walk_dir`, 1.5 m asked) ran the owner 2.25 m PAST the dummy with
    // one tick of reach to spare, and a stretched sleep under the full
    // suite ran it out of reach: the swing missed and the pet had nothing
    // to inherit.
    let mut seq: u32 = 1;
    let start = Vec3 { x: 15.0, y: 0.0, z: 0.0 };
    walk_until_within(&mut a, &mut seq, a_char_id, start, dummy_pos, 1.5).await;
    a.send_attack(dummy, "", false, DamageType::Physical);
    let engaged = a
        .wait_for(CHANNEL_POSITION, Duration::from_secs(4), |m| {
            matches!(m, ServerWorldMsg::Position { id, pos, .. }
                if *id == pet_id && pos.x.is_finite()
                    && ((pos.x - dummy_pos.x).powi(2) + (pos.z - dummy_pos.z).powi(2)).sqrt() < 3.0)
        })
        .await;
    assert!(engaged.is_some(), "the pet inherits the owner's target and closes on the dummy");

    // Gate home. The dummy cannot interrupt (no damage, no legs). The summon
    // started the global cooldown; let it pass first, with the same margin,
    // since the server starts that clock when IT accepts the summon.
    a.pump_for(Duration::from_millis(2_000) + CAST_RELEASE_MARGIN).await;
    a.send_cast_start("Gate", 5.0);
    a.pump_for(Duration::from_millis(5_000) + CAST_RELEASE_MARGIN).await;
    a.send_cast_spell("Gate", None);
    let tp = a
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| matches!(m, ServerWorldMsg::Teleport { .. }))
        .await;
    let Some(ServerWorldMsg::Teleport { pos: dest }) = tp else {
        panic!("Gate teleports the owner");
    };
    assert!(dest.x.abs() < 1.0 && dest.z.abs() < 1.0, "to the birth bind at the spawn, got ({}, {})", dest.x, dest.z);

    // For the next three seconds every Position of the pet stays beside the
    // owner. On the previous code it is walking back to the dummy at 3 m/s,
    // past 6 m inside two seconds. The position channel is unreliable and
    // unordered against the system channel, so a Position fanned just
    // before the port can be read after the Teleport: the first 400 ms are
    // let through.
    let ported_at = Instant::now();
    let end = ported_at + Duration::from_secs(3);
    let mut pet_positions = 0;
    while Instant::now() < end {
        let left = end.saturating_duration_since(Instant::now());
        let Some(ServerWorldMsg::Position { pos, .. }) = a
            .wait_for(CHANNEL_POSITION, left, |m| {
                matches!(m, ServerWorldMsg::Position { id, .. } if *id == pet_id)
            })
            .await
        else {
            break;
        };
        if ported_at.elapsed() < Duration::from_millis(400) {
            continue;
        }
        pet_positions += 1;
        let off = ((pos.x - dest.x).powi(2) + (pos.z - dest.z).powi(2)).sqrt();
        assert!(off < 6.0, "the pet stays with its owner after Gate; it was {off:.1} m away");
    }
    assert!(pet_positions > 0, "the pet's position is still fanned to its owner after the port");
}

/// The review's other find: a CHARMED mob rode Gate into the town square and,
/// when the charm ended, stood there hostile beside the spawn (a named mob,
/// even). A charmed mob is not carried: it is released where it stands and
/// walks home, as when its charmer logs out. Bard 14 (Siren's Song, Gate at
/// 8) charms a dummy 15 m out and Gates to the spawn.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_charmed_mob_is_released_in_place_when_its_charmer_gates() {
    let h = start_both().await;
    let (a_session, a_char_id, _stale) =
        provision_client(&h.auth_url, "charmgate", "Lirael", "Human", "Bard").await;
    set_char_level(&h.db_url, a_char_id, 14).await;
    set_char_pos(&h.db_url, a_char_id, 15.0, 0.0).await;
    let pool = db::open(&h.db_url).await.expect("open pool");
    db::set_account_gm(&pool, "charmgate", true).await.expect("set is_gm");
    let a_token = request_world_token(&h.auth_url, &a_session, a_char_id).await;
    let mut a = WorldClient::start(a_token, &a_session, a_char_id).await;
    a.pump_for(Duration::from_millis(300)).await;

    // Aggro 1 m so it never notices the bard before the charm lands.
    a.send_dev_spawn("Charm Dummy", 1, 500.0, 5, 2.0, 1.0);
    let spawn = a
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
            matches!(m, ServerWorldMsg::EnemySpawn { mob_name, .. } if mob_name == "Charm Dummy")
        })
        .await
        .expect("the dummy spawns");
    let dummy: u64 = match spawn {
        ServerWorldMsg::EnemySpawn { id, .. } => id,
        _ => unreachable!(),
    };
    a.send_cast_start("Siren's Song", 1.5);
    a.pump_for(Duration::from_millis(1_700)).await;
    a.send_cast_spell("Siren's Song", Some(dummy));
    let pet_spawn = a
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(5), |m| {
            matches!(m, ServerWorldMsg::PetSpawn { owner, .. } if *owner == a_char_id as u64)
        })
        .await
        .expect("the charm lands");
    let pet_id: u64 = match pet_spawn {
        ServerWorldMsg::PetSpawn { id, .. } => id,
        _ => unreachable!(),
    };

    // The charm started the global cooldown; let it pass before the Gate.
    a.pump_for(Duration::from_millis(2_100)).await;
    a.send_cast_start("Gate", 5.0);
    a.pump_for(Duration::from_millis(5_200)).await;
    a.send_cast_spell("Gate", None);
    // The Teleport, the charmed pet's despawn and the mob's return land in
    // the same tick; take them in whatever order they come.
    let end = Instant::now() + Duration::from_secs(3);
    let (mut teleported, mut pet_gone, mut mob_back) = (false, false, false);
    while Instant::now() < end && !(teleported && pet_gone && mob_back) {
        let left = end.saturating_duration_since(Instant::now());
        match a
            .wait_for(CHANNEL_SYSTEM, left, |m| {
                matches!(m, ServerWorldMsg::Teleport { .. })
                    || matches!(m, ServerWorldMsg::EntityDespawn { id } if *id == pet_id)
                    || matches!(m, ServerWorldMsg::EnemySpawn { mob_name, .. } if mob_name == "Charm Dummy")
            })
            .await
        {
            Some(ServerWorldMsg::Teleport { pos }) => {
                assert!(pos.x.abs() < 1.0 && pos.z.abs() < 1.0, "Gate lands at the spawn");
                teleported = true;
            }
            Some(ServerWorldMsg::EntityDespawn { .. }) => pet_gone = true,
            Some(ServerWorldMsg::EnemySpawn { pos, .. }) => {
                assert!(
                    (pos.x - 15.0).abs() < 4.0 && pos.z > -8.0 && pos.z < 2.0,
                    "the mob comes back where it stood, 15 m out, not in the square: got ({}, {})",
                    pos.x, pos.z
                );
                mob_back = true;
            }
            _ => break,
        }
    }
    assert!(teleported, "Gate teleports the charmer");
    assert!(pet_gone, "the charmed pet is despawned for its charmer");
    assert!(mob_back, "the mob is back as a hostile where it stood");
}

/// Warder's Fury: the hit, then the warder is sent at the target.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn warders_fury_sends_the_warder_at_the_target() {
    let h = start_both().await;
    let (session, a_id, _stale) = provision_client(&h.auth_url, "furious", "Furiousa", "Human", "Beast Master").await;
    let pool = db::open(&h.db_url).await.expect("open pool");
    db::set_account_gm(&pool, "furious", true).await.expect("set is_gm");
    master_weapon_skill(&h.db_url, a_id, "hand_to_hand").await;
    let token = request_world_token(&h.auth_url, &session, a_id).await;
    let mut a = WorldClient::start(token, &session, a_id).await;
    let pet = a
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(4), |m| matches!(m, ServerWorldMsg::PetSpawn { owner, .. } if *owner == a_id as u64))
        .await;
    let Some(ServerWorldMsg::PetSpawn { id: warder, .. }) = pet else { panic!("the warder comes") };
    let (dummy, dummy_pos) = spawn_dummy(&mut a, "Fury Dummy", 0, 0.0).await;
    let mut seq: u32 = 1;
    walk_until_within(&mut a, &mut seq, a_id, Vec3 { x: 0.0, y: 0.0, z: 0.0 }, dummy_pos, 1.0).await;
    // Guard stance: a guarding warder never inherits the owner's target on
    // its own (the follow-stance assist), so a swing from it here is the
    // skill's command and nothing else.
    a.send_pet_command(protocol::world::pet_command::GUARD, None);
    a.pump_for(Duration::from_millis(500)).await;
    a.send_use_skill("Warder's Fury", Some(dummy));
    let msgs = a.collect_for(Duration::from_millis(8_000)).await;
    assert!(msgs.iter().any(|m| matches!(m, ServerWorldMsg::Hit { attacker, target, .. } | ServerWorldMsg::Miss { attacker, target } if *attacker == a_id as u64 && *target == dummy)),
        "the user's own blow");
    assert!(msgs.iter().any(|m| matches!(m, ServerWorldMsg::Hit { attacker, target, .. } if *attacker == warder && *target == dummy)),
        "and the guarding warder, sent at the target, strikes it");
}
