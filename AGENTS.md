# Notes for coding agents

Capta records the microphone in pure Rust. README.md says what it does and `docs/` how it works.

- On Linux the crate uses only the standard library, and stays that way. cpal serves macOS and Windows as a dependency for those targets only.
- Capta takes no locks. One thread per capture owns the connection, and the `Capture` handle shares only a stop flag and a clone of the socket with it. Keep it that way. If a lock becomes unavoidable, don't hold it across a blocking call or an await, and take locks in one fixed order.
- Everything read from the socket is checked before it's used, and a malformed reply is an error, never a panic or an allocation of what the server claims.
- A change in behaviour comes with a test. `src/pulse/tests.rs` runs the client against a fake server that answers with replies recorded from pipewire-pulse (`tests/fixtures/`), and `cargo test -- --ignored` records from the machine's sound server. A new field or event gets a fixture from a real server where one can be recorded.
- Commit subjects are plain sentences saying the behaviour, as `git log` shows. Messages are public, so they say what changed and why for any user of the project.
- Everything a user reads (error details, docs, comments) uses the fewest plain words that keep the meaning, as a person would explain it to a colleague. Join clauses with a period or a comma, not a colon or a semicolon. Error details are lowercase phrases that callers put into sentences of their own.
