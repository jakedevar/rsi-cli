// Local #294: Settings › Workspace › Session List has the two notification switches (needs me on, finished off) and a test
// button. The switches flip with a click on their value and are remembered; the test button asks the Rust side for one
// notification (the smoke bridge's notify_session stub answers; nothing reaches the desktop).
export default function steps({ pass, fail, sleep, closeAll, command, bridgeLog }) {
  const rowValue = (label) => `(() => { const li = [...document.querySelectorAll('#settings-view .settings-row')].find((r) => r.querySelector('.sv-label')?.textContent === ${JSON.stringify(label)}); return li ? li.querySelector('.sv-value').textContent : null; })()`;
  const clickValue = (label) => `(() => { const li = [...document.querySelectorAll('#settings-view .settings-row')].find((r) => r.querySelector('.sv-label')?.textContent === ${JSON.stringify(label)}); li.querySelector('.sv-value').click(); })()`;
  return [
    {
      id: 'settings-notifications', group: 'settings',
      title: 'Settings has "Notify me when a session needs me" (on) and "… finishes" (off); a click flips and remembers each; the test button posts one notification',
      async run(p) {
        await p.eval(`localStorage.removeItem('rsi.notify.v1')`);
        await command(p, 'set');
        if (!(await p.waitFor(`!document.getElementById('settings-view').hidden`, 5000))) return fail('Settings did not open');
        await p.eval(`document.querySelector('.settings-rail [data-group="workspace"]').click()`);
        await sleep(200);
        await p.eval(`document.querySelector('.settings-tabs [data-section="session_list"]').click()`);
        await sleep(300);
        const needs = 'Notify me when a session needs me';
        const done = 'Notify me when a session finishes';
        const first = { needs: await p.eval(rowValue(needs)), done: await p.eval(rowValue(done)) };
        if (first.needs !== '● On' || first.done !== '○ Off') return fail(`defaults are wrong: ${JSON.stringify(first)}`);
        await p.eval(clickValue(needs)); await sleep(150);
        await p.eval(clickValue(done)); await sleep(150);
        const flipped = { needs: await p.eval(rowValue(needs)), done: await p.eval(rowValue(done)) };
        if (flipped.needs !== '○ Off' || flipped.done !== '● On') return fail(`a click did not flip the switches: ${JSON.stringify(flipped)}`);
        const stored = await p.eval(`localStorage.getItem('rsi.notify.v1')`);
        if (stored !== JSON.stringify({ needsMe: false, finished: true })) return fail(`the choice was not remembered: ${stored}`);
        const posts = () => bridgeLog.filter((e) => e.cmd === 'notify_session').length;
        const before = posts();
        await p.eval(clickValue('Send a test notification')); await sleep(300);
        if (posts() !== before + 1) return fail(`the test button posted ${posts() - before} notifications, not 1`);
        return pass('defaults on/off, both flip by click and are remembered, and Send a test notification posted exactly one');
      },
      async after(p) {
        await p.eval(`(() => { localStorage.removeItem('rsi.notify.v1'); localStorage.removeItem('rsi.desktop.settings.nav.v1'); })()`);
        await closeAll(p);
      },
    },
  ];
}
