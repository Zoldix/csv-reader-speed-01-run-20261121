// csv-reader-speed-01: a fast drop-in for make.fun's csv-reader 1.0.0.
// run(input) returns exactly csv-reader's answer for {text, header}, computed
// by a small Rust/WebAssembly module (src/lib.rs, built from source in this
// repo) that scans UTF-8 bytes with v128 SIMD. transduceCases(buf) is the
// fast path main.ts uses: it parses a whole cases.json buffer in WASM and
// returns answers.json bytes, skipping JS JSON.parse / JSON.stringify.

import { readFileSync } from 'node:fs';

const { instance } = await WebAssembly.instantiate(
  readFileSync(new URL('./csv.wasm', import.meta.url)),
  {},
);
const wasm = instance.exports;

const enc = new TextEncoder();
const dec = new TextDecoder();

function copyIn(bytes) {
  const ptr = wasm.alloc(bytes.length);
  new Uint8Array(wasm.memory.buffer).set(bytes, ptr);
  return ptr;
}

function outBytes(n) {
  const ptr = wasm.out_ptr();
  return new Uint8Array(wasm.memory.buffer).slice(ptr, ptr + n);
}

/** csv-reader 1.0.0's answer for one case's input. */
export function run(input) {
  const bytes = enc.encode(String(input.text));
  const ptr = copyIn(bytes);
  const n = wasm.run_one(ptr, bytes.length, input.header === true ? 1 : 0);
  return JSON.parse(dec.decode(outBytes(n)));
}

/** Whole cases.json buffer (Uint8Array/Buffer) -> answers.json bytes. */
export function transduceCases(buf) {
  const bytes = Buffer.isBuffer(buf) ? buf : Buffer.from(buf);
  const ptr = copyIn(bytes);
  const n = wasm.run_cases(ptr, bytes.length);
  return Buffer.from(outBytes(n));
}
