# Working rules for this repository

1. **THE SPINE IS FIXED.** `xtce-gs-core` is what every other crate agrees on. A type there
   changes only with a reason written down in `PROGRESS.md`, because five crates and an
   interface are built on it. Nothing in core knows about sockets and nothing in core draws.

2. **ONE PLACE ENDS THE BORROW.** `xtce-decode` hands back values that borrow the packet and
   the definition. They become owned exactly once, in `xtce_gs_engine::decode`. A borrowed
   value must never reach the store, a channel or the interface — see `ARCHITECTURE.md`.

3. **THE STANDARD DECIDES, AND IS CITED.** Where a Blue Book says what a field means, the doc
   comment names the document and the section — "CCSDS 132.0-B-3 §4.1.2", not "per the
   standard". A test that asserts what the code does rather than what the standard says is
   not a test; the randomiser-order bug in `PROGRESS.md` is what that costs.

4. **NO PANIC ON A LIVE DOWNLINK.** No `unwrap`, `expect`, `panic!`, `todo!` or
   `unimplemented!` in library code — the lints are denied at each crate root, tests are
   exempt. A hostile packet is a `Result`, never a process that stops.

5. **REFUSE, DO NOT GUESS.** A configuration that cannot be right is refused by name at
   startup. A frame past Reed-Solomon's capacity is dropped and counted, not corrected on
   spec. Every refusal moves a counter the operator can see.

6. **THE GATE.** `cargo fmt --all --check`, `cargo clippy --workspace --all-targets --
   -D warnings`, `cargo test --workspace`. All three, before every commit. Never commit code
   that does not compile.

7. **DEPENDENCIES.** `tokio`, `eframe`/`egui`/`egui_plot`, `clap`, `thiserror`,
   `serde`/`serde_json`, and the two `xtce-rs` crates. Anything else needs a justification in
   `PROGRESS.md` saying what it does that this project will not do by hand.

8. **A TODO NAMES ITS DECISION.** Stopping short is allowed; stopping short silently is not.
   A `// TODO(id)` says what is missing, what has to be decided before it can be written, and
   roughly what the change costs. Add it to `TODO.md` in the same commit.

9. **LOG.** Append to `PROGRESS.md`: what was done, what was measured, what broke and what
   found it. Terse. A number that came from a measurement says which command produced it.
