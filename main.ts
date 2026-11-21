// csv-reader-speed-01 entry driver: reads every case from /input/cases.json,
// answers each one with solve.js, and writes /output/answers.json.
// Fast path: solve.js transduces the raw cases.json buffer in WebAssembly
// (JSON in, answers JSON out). Fallback: the plain per-case run() loop, which
// produces the same answers.
import { readFileSync, writeFileSync } from 'node:fs';
import { join } from 'node:path';
import { setFlagsFromString } from 'node:v8';

setFlagsFromString('--no-incremental-marking');

const solve = await import(new URL('./solve.js', import.meta.url).href);

const INPUT_DIR = process.env.MAKEFUN_INPUT_DIR ?? '/input';
const OUTPUT_DIR = process.env.MAKEFUN_OUTPUT_DIR ?? '/output';

const raw = readFileSync(join(INPUT_DIR, 'cases.json'));
let answers;
try {
  answers = solve.transduceCases(raw);
} catch {
  const cases = JSON.parse(raw.toString('utf8'));
  const out = [];
  for (const c of cases) out.push(solve.run(c.input));
  answers = JSON.stringify(out);
}
writeFileSync(join(OUTPUT_DIR, 'answers.json'), answers);
