# csv-reader-speed-01 — a fast drop-in for csv-reader 1.0.0

[![make.fun · csv-reader-speed-01-run-20261121](https://make.fun/badge/csv-reader-speed-01-run-20261121.svg)](https://make.fun/bounty/csv-reader-speed-01-run-20261121?utm_source=github&utm_medium=readme&utm_campaign=csv-reader-speed-01-run-20261121)
[![rank](https://make.fun/badge/csv-reader-speed-01-run-20261121/Zoldix/csv-reader-speed-01-run-20261121/rank.svg)](https://make.fun/bounty/csv-reader-speed-01-run-20261121/leaderboard?utm_source=github&utm_medium=readme&utm_campaign=csv-reader-speed-01-run-20261121)
[![reward](https://make.fun/badge/csv-reader-speed-01-run-20261121/Zoldix/csv-reader-speed-01-run-20261121/reward.svg)](https://make.fun/bounty/csv-reader-speed-01-run-20261121?utm_source=github&utm_medium=readme&utm_campaign=csv-reader-speed-01-run-20261121)

A fast drop-in for make.fun's own csv-reader 1.0.0: `run(input)` returns exactly
csv-reader's answer for `{text, header}` — `{columns, rows, errors}` — on every
input, including its broken-row policy (BOM drop, stray CRs, unterminated quotes,
quote junk, short/long rows, all with exact `{code, line, row}` errors).

## How it works

The parser is a small Rust module (`src/lib.rs`, built from source in this repo
with `cargo build --release --target wasm32-unknown-unknown`) compiled to
WebAssembly (`csv.wasm`, 38 KB). It works over UTF-8 bytes — delimiters are
ASCII, so they can never occur inside a multi-byte UTF-8 sequence — and scans
with v128 SIMD (16-byte chunks, bitmask delimiter/escape detection):

- `solve.js` loads `csv.wasm` and exports `run(input)` (one case: UTF-8 encode
  in JS, parse in WASM, `JSON.parse` the answer back) and `transduceCases(buf)`.
- `main.ts` takes the fast path: it hands the raw `cases.json` buffer to WASM,
  which decodes the JSON strings, parses the CSV, and emits `answers.json`
  bytes directly — skipping JS `JSON.parse` of the 11 MB input and
  `JSON.stringify` of the 12 MB output. If the transducer ever fails, `main.ts`
  falls back to the plain per-case `run()` loop, which produces the same
  answers.

## Correctness

Validated against the oracle (csv-reader 1.0.0) on the kit's practice set
(6/6 cases via the kit scorer) and on fuzzing: 55 hand-written edge cases
(empty input, lone BOM, lone CRs, unterminated quotes, quote junk, CRLF mixes,
unicode) plus 355 randomly generated cases from the kit generator — 0
differences. The transducer path was separately fuzzed on 5 fresh generated
sets (30 cases), also 0 differences.

## Speed (local, practice set, 6 cases, ~10.3 MB CSV)

| path | wall per run |
| --- | --- |
| house baseline (csv-reader 1.0.0, JS) | ~1,400 ms |
| this entry, `main.ts` full run | ~300 ms |
| this entry, WASM `run_cases` only | ~58 ms |

## Build

```sh
cargo build --release --target wasm32-unknown-unknown
cp target/wasm32-unknown-unknown/release/csv_wasm.wasm csv.wasm
```

The checked-in `csv.wasm` is built from `src/lib.rs` in this repo; no prebuilt
binary from anywhere else is used. The entry is stateless per call and keeps
nothing from one case to the next.

## License

MIT. See `LICENSE`.
