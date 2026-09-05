// Two charts covering the same resources, plus a chart with a bad timestamp.
// Both shapes come from review findings that were verified in a browser.
import { writeFileSync } from 'node:fs';
const rows = Array.from({ length: 5 }, (_, i) => ({
  ts: `2026-09-04T10:0${i}:00Z`, 'web-a': 10 + i, 'web-b': 20 + i,
}));
writeFileSync(process.argv[2], JSON.stringify({
  title: 'Cross-chart consistency',
  charts: [
    { title: 'CPU', unit: '%', series: ['web-a', 'web-b'], rows },
    { title: 'Network', unit: 'B/s', series: ['web-a', 'web-b'], rows },
    { title: 'Broken timestamps', series: ['x'], rows: [
        { ts: '2026-09-04T10:00:00Z', x: 1 },
        { ts: 'not-a-timestamp', x: 2 },
        { ts: '2026-09-04T10:02:00Z', x: 3 } ] },
    { title: 'Queried but empty', series: ['y'], rows: [] },
  ],
  events: [{ ts: '2026-09-04T10:01:00Z', label: 'deploy 4f21a' },
           { ts: '2026-09-04T10:03:00Z', label: 'rollback' }],
  clusters: [],
}));
console.log('probe fixture written:', process.argv[2]);
