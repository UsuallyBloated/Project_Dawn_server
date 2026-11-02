//! The /camp countdown (step 5c): each camping player's timer, and the logout at its end.

use super::*;

pub(super) fn camp_sweep(world: &mut WorldState, _q: &mut Intents, now: Instant) {
    let WorldState { server, connections, .. } = world;
    // 5c. Camp sweep (slice B). Advance each in-progress /camp. Runs after
    //     movement integration (which clears is_sitting on a move) and after
    //     all damage application this tick, so both cancel conditions are
    //     current. A camp cancels if the player is no longer seated (covers
    //     an explicit stand AND movement) or has taken damage since it began;
    //     on cancel we fan a CampUpdate so the client hides its countdown.
    //
    //     Completion is CLIENT-DRIVEN: at CAMP_SECS the client runs the same
    //     clean logout as Quit Game (a clean Disconnect), which reaps the body
    //     and frees the account. The server does NOT force the disconnect here
    //     because a mid-world server kick has no client-side return-to-lobby
    //     path yet (it would strand the player in a frozen world). The server's
    //     job is the vulnerability window + the cancel-on-damage/move rule (the
    //     security-relevant parts); once the window elapses it just stops
    //     tracking. The client backstops itself, and if it never disconnects it
    //     simply stays in-world (it gained nothing — it sat vulnerable the whole
    //     time). The `>= start` compare catches damage applied on the very tick
    //     the camp began (same `now`); prior-tick damage has a strictly smaller
    //     Instant, so this never spuriously cancels a fresh camp.
    for (camp_cid, conn) in connections.iter_mut() {
        let Some(start) = conn.camp_since else {
            continue;
        };
        let cancelled =
            !conn.is_sitting || conn.last_damaged_at.is_some_and(|t| t >= start);
        if cancelled {
            conn.camp_since = None;
            handlers::send_camp_update(&mut *server, *camp_cid, 0, false);
        } else if now.duration_since(start) >= CAMP_SECS {
            // Window elapsed; stop tracking. The client logs itself out.
            conn.camp_since = None;
            tracing::info!(char_id = conn.char_id, "camp window elapsed — client logs out");
        }
    }
}
