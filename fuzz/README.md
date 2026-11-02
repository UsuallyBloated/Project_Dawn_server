# Fuzz targets

`cargo fuzz` runs a target for as long as you let it, feeding it millions of
generated inputs and keeping the ones that reach new code. Here each input is
a client message handled by the server's real message handler, the code the
2026-10-23 chat crash lived in. A panic is the finding: it is a bug any client
could trigger (the server drops that player since 2026-11-02; before that it
went down for everyone).

| Target | Input |
|---|---|
| `handle_arbitrary` | A well-formed message of any type, built from the bytes. The useful one. |
| `decode_and_handle` | Raw bytes as they arrive on the wire, decoded first. Mostly exercises the decoder. |

The entry points are `crates/projectdawn-server/src/world/fuzzing.rs`, built
only with the server's `fuzzing` feature, so none of it reaches the deployed
server. The same code runs as a short seeded soak in every `cargo test`
(`random_client_messages_never_panic_the_handler`); this folder is the long,
coverage-guided version.

## Running

Needs the nightly toolchain and `cargo-fuzz`:

```
rustup toolchain install nightly --profile minimal
cargo install cargo-fuzz --locked
```

Then, from the repository root (not this folder):

```
cargo +nightly fuzz run handle_arbitrary -- -max_total_time=600
```

`-max_total_time` is in seconds; leave it off to run until Ctrl-C. A crash
stops the run and saves its input under `fuzz/artifacts/handle_arbitrary/`;
`cargo +nightly fuzz run handle_arbitrary <that file>` replays it.

**Linux (the R720 or WSL) runs it as is.** On Windows the build needs the
64-bit AddressSanitizer runtime, which comes with Visual Studio's "C++
AddressSanitizer" component (the Visual Studio Installer, Individual
components). Without it the link fails on `clang_rt.asan_dynamic_runtime_thunk`;
`--sanitizer none` does not help on Windows (the coverage counters do not
link). The server forbids unsafe code, so the sanitizer itself finds nothing
extra here: the run is looking for panics.

`target/`, `corpus/` and `artifacts/` are not committed.
