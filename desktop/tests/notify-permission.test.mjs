// Local #300: Settings says once when macOS blocks notifications for RSI, and shows nothing about it otherwise.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { join } from 'node:path';
import { fileURLToPath } from 'node:url';
import {
  NOTIFY_DENIED_LABEL, NOTIFY_DENIED_SUMMARY, NOTIFY_OPEN_SETTINGS_BUTTON, NOTIFY_PERMISSION_HELP, parseNotifyPermission,
} from '../dist/notify-model.js';
import { sectionRows, valueTooltip } from '../dist/settings-model.js';
import { createStubBridge } from '../scripts/smoke-stub-bridge.mjs';

const root = join(fileURLToPath(new URL('.', import.meta.url)), '..');
const notifyRows = (permission) => sectionRows('session_list', {}, permission).filter((r) => r.id.startsWith('desktop_notify_'));

test('the Rust word is parsed to one of four states; anything else is unknown', () => {
  assert.equal(parseNotifyPermission('granted'), 'granted');
  assert.equal(parseNotifyPermission('denied'), 'denied');
  assert.equal(parseNotifyPermission('not_determined'), 'not_determined');
  assert.equal(parseNotifyPermission('unknown'), 'unknown');
  for (const bad of [undefined, null, '', 'Denied', 'authorized', 1, {}]) assert.equal(parseNotifyPermission(bad), 'unknown');
});

test('denied: one extra row names the problem, tells where to turn it on and opens the pane; the standing hint is gone', () => {
  const rows = notifyRows('denied');
  assert.deepEqual(rows.map((r) => r.id), ['desktop_notify_denied', 'desktop_notify_needs', 'desktop_notify_finished', 'desktop_notify_test']);
  const [denied, ...rest] = rows;
  assert.equal(denied.label, NOTIFY_DENIED_LABEL);
  assert.equal(denied.summary, NOTIFY_DENIED_SUMMARY);
  assert.match(denied.summary, /System Settings › Notifications, pick RSI and turn on Allow Notifications/);
  assert.deepEqual(denied.widget, { type: 'local', local: 'notify-open-settings' });
  assert.equal(valueTooltip(denied, false), 'Click to open System Settings › Notifications');
  assert.match(NOTIFY_OPEN_SETTINGS_BUTTON, /Notification settings/);
  for (const r of rest) assert.ok(!r.summary.includes(NOTIFY_PERMISSION_HELP), r.id);
});

test('granted and not yet asked show nothing about permission: no extra row and no standing hint', () => {
  for (const p of ['granted', 'not_determined']) {
    const rows = notifyRows(p);
    assert.deepEqual(rows.map((r) => r.id), ['desktop_notify_needs', 'desktop_notify_finished', 'desktop_notify_test'], p);
    for (const r of rows) assert.ok(!r.summary.includes('System Settings'), `${p} ${r.id}`);
  }
});

test('unknown (not macOS, a dev build, no answer, or before it is read) keeps today\'s text', () => {
  for (const rows of [notifyRows('unknown'), sectionRows('session_list', {}).filter((r) => r.id.startsWith('desktop_notify_'))]) {
    assert.deepEqual(rows.map((r) => r.id), ['desktop_notify_needs', 'desktop_notify_finished', 'desktop_notify_test']);
    assert.ok(rows[0].summary.endsWith(NOTIFY_PERMISSION_HELP));
    assert.ok(rows[2].summary.endsWith(NOTIFY_PERMISSION_HELP));
  }
});

test('through the stub bridge: the page asks notify_permission and open_notification_settings, with no arguments', async () => {
  const calls = [];
  const bridge = createStubBridge({
    call: async () => ({}),
    local: { notify_permission: (a) => { calls.push(['notify_permission', a]); return 'denied'; }, open_notification_settings: (a) => { calls.push(['open', a]); return null; } },
  });
  assert.equal(parseNotifyPermission(await bridge.invoke('notify_permission', {})), 'denied');
  await bridge.invoke('open_notification_settings', {});
  assert.deepEqual(calls, [['notify_permission', {}], ['open', {}]]);
});

test('the Rust side stays read only and the webview gets no new permission', () => {
  const caps = readFileSync(join(root, 'src-tauri/capabilities/default.json'), 'utf8');
  assert.doesNotMatch(caps, /notification|systempreferences/);
  const rs = readFileSync(join(root, 'src-tauri/src/notify.rs'), 'utf8');
  assert.match(rs, /pub async fn notify_permission\(\)/);
  assert.doesNotMatch(rs.split('#[cfg(test)]')[0], /requestAuthorization|request_permission|setNotificationCategories/);
  const nav = readFileSync(join(root, 'src-tauri/src/nav.rs'), 'utf8');
  assert.match(nav, /pub fn open_notification_settings\(\)/);
  // the pane address is fixed in Rust; the page cannot pass one
  assert.doesNotMatch(nav, /open_notification_settings\([a-z_]+:/);
});
