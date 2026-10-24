// Usage:
//   node lock.js hold <lockdir> <ms> [staleMs] [updateMs]  -> prints "held", holds, releases, prints "released"
//   node lock.js try  <lockdir> [staleMs]                   -> prints "acquired" or "ELOCKED"
// Options mirror Claude Code's OAuth refresh lock (realpath: false, stale 60000, update 5000).
const lockfile = require('proper-lockfile');
const path = require('path');

const [mode, lockdir, a, b, c] = process.argv.slice(2);
const target = path.join(path.dirname(lockdir), 'target');
const opts = (stale, update) => ({
  realpath: false,
  lockfilePath: lockdir,
  stale: Number(stale || 60000),
  update: Number(update || 5000),
  onCompromised: (err) => { console.log('compromised ' + err.message); process.exit(3); },
});

(async () => {
  if (mode === 'hold') {
    const release = await lockfile.lock(target, opts(b, c));
    console.log('held');
    await new Promise((r) => setTimeout(r, Number(a)));
    await release();
    console.log('released');
  } else if (mode === 'try') {
    try {
      const release = await lockfile.lock(target, opts(a));
      console.log('acquired');
      await release();
    } catch (e) {
      console.log(e.code || e.message);
    }
  }
})().catch((e) => { console.error(e); process.exit(2); });
