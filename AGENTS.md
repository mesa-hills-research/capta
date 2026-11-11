# Notes for coding agents

Capta records the microphone in pure Rust. README.md says what it does and `docs/` how it works.

- The crate depends on the standard library alone, and on Windows and macOS on the system's own libraries, which it declares by hand. Keep it that way.
- `unsafe` code only calls the system. It lives in `src/wasapi/com.rs` and `src/coreaudio/ffi.rs`, holds one operation per block with a `// SAFETY:` comment above it, and is listed in `docs/why-this-is-safe.md`, which a change to it updates. Linux forbids `unsafe`. The callback Core Audio calls takes no lock, allocates nothing and calls nothing but the queue.
- Capta takes no locks. One thread per capture owns the connection or the device, and the `Capture` handle shares only a stop flag and a way to wake that thread with it, a clone of the socket, a duplicate of an event handle or the thread's own handle. Keep it that way. If a lock becomes unavoidable, don't hold it across a blocking call or an await, and take locks in one fixed order.
- Everything read from the socket or the system is checked before it's used, and a malformed reply is an error, never a panic or an allocation of what the server claims.
- A change in behaviour comes with a test. `src/pulse/tests.rs` runs the client against a fake server that answers with replies recorded from pipewire-pulse (`tests/fixtures/`). `cargo test -- --ignored` records from the machine's audio system, and CI runs it on Linux, macOS and Windows against a loopback device that plays a tone. A new field or event gets a fixture from a real server where one can be recorded.
- Tests, fixtures, examples and docs use made-up paths and names, never a real machine's or person's, so a recorded fixture has its user and host names replaced (`fixture-user`, `fixture-host`).
- Commit subjects are plain sentences saying the behaviour, as `git log` shows. Messages are public, so they say what changed and why for any user of the project.
- Everything a user reads (error details, docs, comments) uses the fewest plain words that keep the meaning, as a person would explain it to a colleague. Join clauses with a period or a comma, not a colon or a semicolon. Error details are lowercase phrases that callers put into sentences of their own.
