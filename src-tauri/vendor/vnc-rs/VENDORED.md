# vnc-rs, vendored

A copy of [vnc-rs](https://github.com/HsuJv/vnc-rs) 0.6.0 as published on crates.io, the VNC client library
Submarine's VNC tab uses (`src/vnc_client.rs`). MIT OR Apache-2.0 (`LICENSE-MIT`, `LICENSE-APACHE`).

## The changes

1. One request more: `X11Event::RefreshRect(Rect)` asks the server for a full (non-incremental) update of
   one part of the screen, clipped to it (`src/event.rs`, `src/client/connection.rs`, `input_message`). The
   library only asked for the whole screen, incrementally (`Refresh`) or not (`FullRefresh`). The VNC tab
   asks for an 8×8 corner every 2 seconds and times the answer, for its connection mark: a server must
   answer such a request at once, and the answer is a few hundred bytes.
2. A resize that timed out no longer stops all later ones for good (`src/client/resize.rs`, `observe`).
   After a timeout the library refuses to resize (`ResizeError::Uncertain`) until the outcome is known;
   it now counts the server's late answer to that request (reason "this client", nothing pending) as
   knowing it. Before, the session could never resize again. Tested in `resize.rs`
   (`the_late_answer_to_a_timed_out_resize_lets_the_next_one_go`).

Not copied: the crate's `tests/` folder and examples (its `[[test]]` and dev-dependencies are left out of
`Cargo.toml`, which is written out by hand from the published one). The unit tests inside `src/` are kept.

## Leaving it

Once a vnc-rs release can ask for an update of a part of the screen, delete this folder and depend on
`vnc-rs` from crates.io again.
