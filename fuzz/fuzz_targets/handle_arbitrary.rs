//! A well-formed client message built from the fuzzer's bytes (any variant,
//! any field values), handled for a character in the world
//! (`world::fuzzing::handle_arbitrary`). Reaches the handlers far more often
//! than raw bytes, which mostly fail to decode. A panic is a bug any client
//! could trigger.
#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    projectdawn_server::world::fuzzing::handle_arbitrary(data);
});
