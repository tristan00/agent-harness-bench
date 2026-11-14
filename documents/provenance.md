# Chess960 port provenance

**Planner and worker agents must not read this document or anything else in this repository.**

The reusable Chess960 tooling is maintained outside this benchmark in:

[tristan00/bench_tooling](https://github.com/tristan00/bench_tooling), checked out locally at `D:\harness_work\bench_tooling`

The benchmark depends on that standalone Rust crate as a git dependency; Cargo.lock pins the commit. It retains the all-960-starts, perft, overlapping-castling, en-passant, promotion and draw tests. The Elo formula is implemented independently in this service with explicit double-forfeit handling.

The old project's rule-author restrictions do not apply to these benchmark entrants: the new preset prompt specifies only the input/output/runtime contract and evaluation rules, leaving implementation choices open.

Chess library: [shakmaty 0.30.2](https://docs.rs/shakmaty/0.30.2/shakmaty/). [Chess960 castling mode](https://docs.rs/shakmaty/0.30.2/shakmaty/enum.CastlingMode.html) uses king-to-rook UCI notation. HTTP transport: [tiny_http](https://docs.rs/tiny_http/0.12.0/tiny_http/struct.Server.html). Cargo.lock pins dependencies. Shakmaty is GPL-3.0-or-later; account for dependency licensing when distributing this project.
