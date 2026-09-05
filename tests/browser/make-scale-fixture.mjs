// Generates a findings document at realistic incident volume.
//
// A report that is pleasant with one cluster and unusable with three hundred
// has not been tested, and three hundred is an ordinary morning.
import { writeFileSync } from 'node:fs';

const CLUSTERS = 300, POINTS = 3000, SERIES = 8;
let seed = 7;
const rnd = () => (seed = (seed * 1103515245 + 12345) & 0x7fffffff) / 0x7fffffff;

const t0 = Date.UTC(2026, 8, 4, 6, 0, 0);
const iso = ms => new Date(ms).toISOString().replace(/\.\d{3}Z$/, 'Z');
const series = Array.from({ length: SERIES }, (_, i) => `host-${String(i).padStart(2, '0')}`);

const rows = Array.from({ length: POINTS }, (_, i) => {
  const r = { ts: iso(t0 + i * 60_000) };
  // Real series have gaps; a dense synthetic one would not exercise them.
  for (const s of series) r[s] = rnd() < 0.15 ? null : Math.round(rnd() * 10000) / 100;
  return r;
});

const clusters = Array.from({ length: CLUSTERS }, (_, i) => ({
  cluster_id: i.toString(16).padStart(12, '0'),
  status: ['new', 'spiking', 'steady'][i % 3],
  count: 1 + Math.floor(rnd() * 90000),
  first_seen: iso(t0),
  last_seen: iso(t0 + 3600_000),
  template: `Handler <*> failed for tenant <*> after <*>ms (variant ${i})`,
  histogram: Array.from({ length: 60 }, () => Math.floor(rnd() * 400)),
  by_stream: Array.from({ length: 10 }, (_, j) => ({ stream: `stream-${j}`, count: 1 + Math.floor(rnd() * 900) })),
  other_streams: Math.floor(rnd() * 40),
  exemplar: { ts: iso(t0), stream: 'stream-0', message: 'Handler abc failed for tenant xyz after 1234ms' },
}));

writeFileSync(process.argv[2], JSON.stringify({
  title: 'Scale check',
  subtitle: `${CLUSTERS} clusters, ${POINTS} points x ${SERIES} series`,
  window: { since: iso(t0), until: iso(t0 + POINTS * 60_000) },
  summary: 'Synthetic document at realistic incident volume.',
  sections: [],
  events: [{ ts: iso(t0 + 1800_000), label: 'deploy' }],
  charts: [{ title: 'CPU', unit: '%', series, rows }],
  clusters,
  appendix: [],
}));
console.log('scale fixture written:', process.argv[2]);
