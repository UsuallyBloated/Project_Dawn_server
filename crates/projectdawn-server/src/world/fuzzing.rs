//! Random client input through the real message handler, to find panics a
//! client could trigger before a player does. Two users: the `cargo fuzz`
//! targets (the `fuzz/` crate at the repository root, `fuzzing` feature) and
//! the soak test below, which runs in every `cargo test`. Compiled for those
//! only, so none of this reaches the server that is deployed. A panic is the
//! finding: the narrow containment (`tick::intake`) would drop the player who
//! sent it, and before that the whole server went down.

use super::{connection::PerConnection, handlers};
use protocol::world::Coins;
use std::time::Instant;

const CLIENT: renet::ClientId = 1;

/// Decode `data` as one client message, as the wire would, and handle it.
pub fn decode_and_handle(data: &[u8]) {
    if let Some(msg) = handlers::decode_client(data, CLIENT) {
        handle(msg);
    }
}

/// Build a well-formed client message from `data` (any variant, any field
/// values: odd strings, non-finite floats, extreme numbers) and handle it.
/// Reaches the handlers' own code far more often than raw bytes, which mostly
/// fail to decode.
pub fn handle_arbitrary(data: &[u8]) {
    let mut input = arbitrary::Unstructured::new(data);
    if let Ok(msg) =
        <protocol::world::ClientWorldMsg as arbitrary::Arbitrary>::arbitrary(&mut input)
    {
        handle(msg);
    }
}

/// Handle one message for a fresh character in the world, the way the tick's
/// intake does. The character is a GM, so the dev commands' arms are reached
/// too.
fn handle(msg: protocol::world::ClientWorldMsg) {
    let mut server = renet::RenetServer::new(super::connection_config());
    server.add_connection(CLIENT);
    let mut conn = PerConnection::from_spawn(spawn(), Instant::now());
    conn.in_world = true;
    conn.is_gm = true;
    let _ = handlers::handle_message(&mut server, &mut conn, CLIENT, msg, Instant::now());
}

/// A level 20 Human Cleric at the starter spawn with some coin, standing in
/// for a loaded character.
pub(crate) fn spawn() -> crate::db::CharacterSpawn {
    crate::db::CharacterSpawn {
        char_id: CLIENT as i64,
        account_id: CLIENT as i64,
        name: "Fuzzer".into(),
        race: "Human".into(),
        class: "Cleric".into(),
        level: 20,
        xp: 0,
        xp_to_next: 100,
        strength: 10,
        dexterity: 10,
        agility: 10,
        intelligence: 10,
        wisdom: 10,
        charisma: 10,
        constitution: 10,
        max_hp: 100.0,
        max_mp: 100.0,
        max_stamina: 100.0,
        hp: 100.0,
        mp: 100.0,
        stamina: 100.0,
        coins: Coins {
            platinum: 1,
            gold: 1,
            silver: 1,
            copper: 1,
        },
        bank_coins: Coins {
            platinum: 1,
            gold: 1,
            silver: 1,
            copper: 1,
        },
        zone: None,
        pos: (0.0, 0.0, 0.0),
        yaw: 0.0,
        bind: None,
        hunger: 14400.0,
        thirst: 14400.0,
        alignment: None,
        factions: Vec::new(),
        completed_quests: Vec::new(),
        active_quests: Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use rand::{RngCore, SeedableRng};

    /// Whole UTF-8 characters, one to four bytes each, for the byte streams
    /// below: `arbitrary` keeps only the valid UTF-8 at the start of a
    /// string's bytes, which for uniformly random bytes is about two ASCII
    /// characters, too short and too plain to reach a multibyte cut.
    const CHARS: [&str; 8] = ["a", "Z", " ", "/", "é", "ß", "€", "😀"];

    /// The soak: twenty thousand random, well-formed client messages through
    /// the handler, from a fixed seed so a failure reproduces. Half are built
    /// from random bytes, half from a stream of whole UTF-8 characters, so
    /// strings come out long and multibyte (the October chat crash was a byte
    /// slice through a two-byte letter). Any panic fails the test. A
    /// coverage-guided run of the same thing is
    /// `cargo +nightly fuzz run handle_arbitrary` (fuzz/README.md).
    #[test]
    fn random_client_messages_never_panic_the_handler() {
        let mut rng = rand::rngs::StdRng::seed_from_u64(0x5044_5f57);
        let mut buf: Vec<u8> = Vec::with_capacity(2048);
        for i in 0..20_000 {
            buf.clear();
            let len = (rng.next_u32() as usize) % 1024;
            if i % 2 == 0 {
                buf.resize(len, 0);
                rng.fill_bytes(&mut buf);
            } else {
                // Four random bytes first: they pick the message type, which
                // a stream of eight characters could only reach a few of.
                buf.extend_from_slice(&rng.next_u32().to_le_bytes());
                while buf.len() < len {
                    let c = CHARS[(rng.next_u32() as usize) % CHARS.len()];
                    buf.extend_from_slice(c.as_bytes());
                }
            }
            super::handle_arbitrary(&buf);
        }
    }

    /// The raw-bytes entry runs on an honest message and on garbage without
    /// panicking, so a fuzz run's first crash is a real one and not the harness.
    #[test]
    fn the_entry_handles_a_message_and_garbage() {
        let say = protocol::world::ClientWorldMsg::Chat {
            channel: protocol::world::ChatChannel::Say,
            text: "hello".into(),
            target_name: None,
        };
        let bytes =
            bincode::serde::encode_to_vec(&say, bincode::config::standard()).expect("encode");
        super::decode_and_handle(&bytes);
        super::decode_and_handle(&[0xff; 64]);
        super::decode_and_handle(&[]);
    }
}
