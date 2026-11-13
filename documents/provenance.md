# Chess960 port provenance

**Planner and worker agents must not read this document or anything else in this repository.**

`src/engine.rs` was copied from the user's neighboring project:

`../chess960_rule_based_evaluator/chess960/src/engine.rs`

The engine remains self-contained and retains its all-960-starts, perft, overlapping-castling, en-passant, promotion and draw tests. This project does not depend on the neighboring directory at build or runtime. The Elo formula was reviewed there and reimplemented independently with this service's records and explicit double-forfeit handling.

The old project's rule-author restrictions do not apply to these benchmark entrants: the new preset prompt specifies only the input/output/runtime contract and evaluation rules, leaving implementation choices open.

Chess library: [shakmaty 0.30.2](https://docs.rs/shakmaty/0.30.2/shakmaty/). [Chess960 castling mode](https://docs.rs/shakmaty/0.30.2/shakmaty/enum.CastlingMode.html) uses king-to-rook UCI notation. HTTP transport: [tiny_http](https://docs.rs/tiny_http/0.12.0/tiny_http/struct.Server.html). Cargo.lock pins dependencies. Shakmaty is GPL-3.0-or-later; account for dependency licensing when distributing this project.
