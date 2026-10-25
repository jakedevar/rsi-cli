// An example audit flow (local #277): run it with `npm run audit -- flow scripts/audit-flow-example.mjs`, copy it for your own.
// A flow is a module whose default export is `async ({ lib, out })`. `lib` is scripts/audit-lib.mjs. It sends a message with the
// write stubbed, then again while rsid is "down", and reports what the page did; screenshots land in `out`.
import { join } from 'node:path';

export default async function flow({ lib, out }) {
  const { open, sleep, clickSel, clickText, openSessionByTitle } = lib;
  const a = await open({ width: 1280, height: 800, stubs: { send_message: { queued: true } } });
  try {
    await openSessionByTitle(a.page, 'Fake worker');
    await clickSel(a.page, '#message');
    await a.page.keys('hello');
    await clickText(a.page, 'Send');
    await sleep(500);
    await a.page.shot(join(out, 'flow-send.png'));
    const sent = a.writes();
    console.log(`writes after Send: ${JSON.stringify(sent.map((w) => ({ cmd: w.cmd, outcome: w.outcome })))}`);

    a.bridge.setDown(true);
    await clickSel(a.page, '#message');
    await a.page.keys('again');
    await clickText(a.page, 'Send');
    await sleep(1500);
    await a.page.shot(join(out, 'flow-send-down.png'));
    console.log(`composer keeps the text while rsid is down: ${JSON.stringify(await a.page.eval(`document.getElementById('message').value`))}`);
    console.log(`banner: ${JSON.stringify(await a.page.eval(`document.body.innerText.split('\\n').find((l) => /did not (start|answer)|cannot reach/i.test(l)) ?? ''`))}`);
    console.log(`connection: ${JSON.stringify(await a.page.eval(`document.getElementById('conn')?.innerText ?? ''`))}`);
    // setDown answers like the real backend: daemon_info says { connected: false }, so the page shows the daemon-down card.
    const card = await a.page.waitFor(`document.body.innerText.includes('rsid is not running')`, 20000);
    const buttons = await a.page.eval(lib.visibleButtons);
    console.log(`daemon-down card: ${card}; Start rsid ${buttons.includes('Start rsid')}; Retry now ${buttons.includes('Retry now')}`);
    await a.page.shot(join(out, 'flow-daemon-down-card.png'));
    console.log(`texts under AA: ${(await lib.contrastAudit(a.page)).failed}`);
  } finally { await a.close(); }
}
