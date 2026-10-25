// Session actions from the detail pane.
import { api } from './api.js';
import { deliver } from './operator-messages.js';
import { refreshEvents } from './conversation.js';
import { $, toast } from './dom.js';
import { refreshSessions } from './sessions.js';
import { isReadOnly, selected, state } from './state.js';

export async function sendMessage(): Promise<void> {
  const id = state.selectedId;
  const box = $<HTMLTextAreaElement>('message');
  const text = box.value;
  const s = selected();
  if (!id || !s || !text.trim() || isReadOnly(s)) return;
  box.disabled = true;
  try {
    const how = await deliver(s, text);
    box.value = '';
    box.dispatchEvent(new Event('input', { bubbles: true }));
    if (how === 'sent') toast('Sent', true);
    void refreshSessions();
    void refreshEvents();
  } catch (e) {
    toast(`Send failed: ${String(e)}`);
  } finally {
    box.disabled = false;
    box.focus();
  }
}

export async function interrupt(): Promise<void> {
  const id = state.selectedId;
  if (!id || isReadOnly(selected())) return;
  try {
    await api.interrupt(id);
    toast('Stopping this turn', true);
    void refreshSessions();
  } catch (e) {
    toast(`Could not stop the turn: ${String(e)}`);
  }
}
