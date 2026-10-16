import fs from 'node:fs';
import path from 'node:path';

const root = process.argv[2];
if (!root) throw Error('Usage: bun summarize.mjs <results-directory>');
const read = name => fs.readFileSync(path.join(root, name), 'utf8');
const median = values => [...values].sort((a, b) => a - b)[Math.ceil(values.length / 2) - 1];
const rows = [];
for (const window of [0, 8, 64]) {
  for (const round of [1, 2, 3]) {
    const prefix = mode => `out/w${window}-r${round}-${mode}/`;
    for (const file of ['roots.csv', 'audit.csv']) {
      if (read(prefix('version-path') + file) !== read(prefix('node-id') + file)) {
        throw Error(`Mismatch: W=${window}, round=${round}, ${file}`);
      }
    }
    for (const mode of ['version-path', 'node-id']) {
      const [header, ...lines] = read(prefix(mode) + 'cycles.csv').trim().split('\n');
      const keys = header.split(',');
      const cycles = lines.map(line => Object.fromEntries(line.split(',').map((v, i) => [keys[i], Number(v)])));
      if (cycles.length !== 60 || cycles.some(r => r.profiled)) throw Error('Unexpected measured workload');
      const sum = field => cycles.reduce((total, r) => total + r[field], 0);
      rows.push({window, round, mode, ops_s: sum('ops') / sum('branch_ms') * 1000,
        seal_p50: median(cycles.map(r => r.seal_ms)), commit_p50: median(cycles.map(r => r.commit_ms))});
    }
  }
}
console.log('| W | Path ops/s | ID ops/s | ID change |');
console.log('|---:|---:|---:|---:|');
for (const window of [0, 8, 64]) {
  const rate = mode => median(rows.filter(r => r.window === window && r.mode === mode).map(r => r.ops_s));
  const a = rate('version-path'), b = rate('node-id');
  console.log(`| ${window} | ${a.toFixed(0)} | ${b.toFixed(0)} | ${((b / a - 1) * 100).toFixed(1)}% |`);
}
