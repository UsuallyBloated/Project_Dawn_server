//! Raw bytes as a client would send them: decoded as one message, then
//! handled for a character in the world (`world::fuzzing::decode_and_handle`).
//! A panic is a bug any client could trigger.
#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    projectdawn_server::world::fuzzing::decode_and_handle(data);
});
