// Issue #305: Cmd-N (Ctrl-N off macOS) opens New session with the Vim keys off, from the session list and from the message
// box, as Cmd-K and Cmd-, do. The native File menu shows the same accelerator; this checks the webview chord, which works in
// a browser too.
export default function steps({ pass, fail, sleep, closeAll }) {
  const vimKeys = (p, on) => p.eval(`(() => { const c = document.getElementById('vim-keys'); if (c.checked !== ${on}) { c.checked = ${on}; c.dispatchEvent(new Event('change')); } })()`);
  /** Cmd-N on macOS (Meta = 4), Ctrl-N elsewhere (Ctrl = 2), as the app reads the platform. */
  const chord = async (p) => {
    const mac = await p.eval(`/mac|iphone|ipad/i.test(navigator.userAgentData?.platform || navigator.platform || navigator.userAgent)`);
    const base = { key: 'n', code: 'KeyN', windowsVirtualKeyCode: 78, modifiers: mac ? 4 : 2 };
    await p.send('Input.dispatchKeyEvent', { type: 'keyDown', ...base });
    await p.send('Input.dispatchKeyEvent', { type: 'keyUp', ...base });
    return mac ? 'Cmd-N' : 'Ctrl-N';
  };
  const launchOpen = (p) => p.waitFor(`!!document.querySelector('#launch-dialog[open]')`, 3000);
  return [
    {
      id: 'new-session-chord', group: 'keyboard',
      title: 'With the Vim keys off, Cmd-N (Ctrl-N off macOS) opens New session from the session list and from the message box',
      async run(p) {
        await vimKeys(p, false);
        await closeAll(p);
        // From the list: focus a row (or the list itself).
        await p.eval(`(() => { const r = document.querySelector('#session-list .session-item') ?? document.getElementById('session-list'); r.focus(); return true; })()`);
        const name = await chord(p);
        if (!(await launchOpen(p))) return fail(`${name} on the session list did not open New session (Vim keys off)`);
        await closeAll(p);
        // From the message box of an open session.
        await p.click(`document.querySelector('#session-list .session-item')`);
        await sleep(400);
        const box = await p.eval(`(() => { const m = document.getElementById('message'); if (!m || m.offsetParent === null) return false; m.focus(); return document.activeElement === m; })()`);
        if (!box) return pass(`${name} opened New session from the list; skipped the message box: no open session shows one`);
        await chord(p);
        if (!(await launchOpen(p))) return fail(`${name} in the message box did not open New session (Vim keys off)`);
        const typed = await p.eval(`document.getElementById('message').value`);
        if (/n$/i.test(typed ?? '')) return fail(`${name} also typed "n" into the message box`);
        return pass(`${name} opened New session from the session list and from the message box with the Vim keys off`);
      },
      async after(p) {
        await closeAll(p);
        await vimKeys(p, true);
      },
    },
  ];
}
