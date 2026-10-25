# Smoke steps in their own files (local #281)

A new headless-smoke step goes in a new `*.mjs` file here, not in `scripts/smoke.mjs`. `smoke.mjs` lists this directory and
appends every file's steps after the built-in ones, in file-name order, so two slices that each add a step add two different
files and never conflict. Files starting with `_` are ignored.

```js
// scripts/smoke-steps/attach-thing.mjs
export default function steps({ live, bridgeLog, pass, fail, sleep, closeAll, command, palette, DIALOG, cursorIndex, daemonLacks }) {
  return [
    {
      id: 'attach-thing',          // unique across all steps
      group: 'core',               // exactly one group of scripts/smoke-groups.mjs (core, settings, manager, keyboard, a11y)
      refuses: ['some_write_cmd'], // optional: write commands this step tries, which the read-only bridge refuses and logs
      title: 'what the step checks',
      async run(p) { /* p is the page: p.key, p.keys, p.eval, p.waitFor, p.element */ return pass('what it saw'); },
      after: closeAll,             // optional: runs after the screenshot
    },
  ];
}
```

A step with no `group`, two groups, an unknown group or a repeated id fails `tests/smoke.test.mjs` ("every smoke step runs
in exactly one group") and `node scripts/smoke.mjs`, naming the step. Pick the group with the most room: each test group
has a 90 s budget, and the slowest steps are printed when one runs out.
