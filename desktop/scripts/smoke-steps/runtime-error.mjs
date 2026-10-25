// Issue #295: an error AFTER boot is a runtime error. The startup banner and html[data-boot] stay as they were, the user gets a
// dismissible notice in plain words (the technical text only under Details), the composer stays on screen at 760x480, and the same
// error twice is one notice with a count.
export default function steps({ pass, fail, sleep, closeAll }) {
  return [
    {
      id: 'runtime-error', group: 'core',
      title: 'an unhandled rejection after boot leaves data-boot and the startup banner alone and shows one dismissible plain-words notice that keeps Send on screen at 760x480',
      async run(p) {
        const resize = (w, h) => p.send('Emulation.setDeviceMetricsOverride', { width: w, height: h, deviceScaleFactor: 1, mobile: false });
        await closeAll(p);
        await p.click(`document.querySelector('#session-list .session-item')`);
        await sleep(400);
        await resize(760, 480);
        await sleep(300);
        const read = () => p.eval(`(() => {
          const n = document.getElementById('runtime-notice');
          const b = document.getElementById('boot-banner');
          const send = document.querySelector('#composer button[type=submit], #composer .btn.primary');
          const sr = send ? send.getBoundingClientRect() : null;
          return {
            boot: document.documentElement.dataset.boot,
            bannerHidden: b.hidden, banner: b.textContent,
            notice: n && !n.hidden ? n.querySelector('.err-text').textContent + (n.querySelector('.runtime-notice-repeat')?.textContent ?? '') : null,
            details: n ? (n.querySelector('.err-raw')?.textContent ?? null) : null,
            count: document.querySelectorAll('#runtime-notice').length,
            sendInView: sr ? sr.bottom <= window.innerHeight && sr.top >= 0 : null,
            docHeight: document.documentElement.scrollHeight, winHeight: window.innerHeight,
          };
        })()`);
        const before = await read();
        if (before.boot !== 'ready') return pass(`skipped: the page booted as ${before.boot}, not ready`);
        await p.eval(`(() => { for (let i = 0; i < 3; i++) Promise.reject(new TypeError("Cannot read properties of null (reading 'entries')")); return true; })()`);
        await sleep(400);
        const after = await read();
        const problems = [];
        if (after.boot !== 'ready') problems.push(`data-boot became ${after.boot}`);
        if (!after.bannerHidden || /did not start/.test(after.banner)) problems.push(`the startup banner showed: ${after.banner}`);
        if (!after.notice) problems.push('no runtime notice');
        else {
          if (/TypeError|Cannot read/.test(after.notice)) problems.push(`the notice text holds the raw error: ${after.notice}`);
          if (!/The app could not read rsid's answer/.test(after.notice)) problems.push(`the notice is not in plain words: ${after.notice}`);
          if (!/Happened 3 times/.test(after.notice)) problems.push(`three repeats were not folded into one count: ${after.notice}`);
        }
        if (!/TypeError: Cannot read properties of null/.test(after.details ?? '')) problems.push('the raw error is not under Details');
        if (after.count !== 1) problems.push(`${after.count} notices in the page`);
        if (after.sendInView === false) problems.push('Send is off screen at 760x480');
        if (after.docHeight > after.winHeight + 1) problems.push(`the document grew (${after.docHeight} in a ${after.winHeight} window)`);
        const dismissed = await p.eval(`(() => {
          const b = [...document.querySelectorAll('#runtime-notice button')].find((x) => x.textContent === 'Dismiss');
          if (!b) return null;
          b.click();
          return document.getElementById('runtime-notice').hidden;
        })()`);
        if (dismissed !== true) problems.push(`Dismiss did not hide the notice (${dismissed})`);
        if (problems.length) return fail(problems.join('; '));
        return pass(`data-boot stayed ready, the banner stayed hidden, one notice with the plain sentence and a count of 3, the raw error under Details, Send in view, Dismiss closes it`);
      },
      async after(p) {
        await p.send('Emulation.clearDeviceMetricsOverride');
        await p.eval(`document.getElementById('runtime-notice')?.remove()`);
      },
    },
  ];
}
