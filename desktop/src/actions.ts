// Session actions from the detail pane.
import { api } from './api.js';
import { refreshEvents } from './conversation.js';
import { $, toast } from './dom.js';
import { refreshSessions } from './sessions.js';
import { isReadOnly, selected, state } from './state.js';

export async function sendMessage(): Promise<void> {
  const id = state.selectedId;
  const box = $<HTMLTextAreaElement>('message');
  const text = box.value;
  if (!id || !text.trim() || isReadOnly(selected())) return;
  box.disabled = true;
  try {
    await api.sendMessage(id, text);
    box.value = '';
    toast('Sent', true);
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
    toast('Interrupt requested', true);
    void refreshSessions();
  } catch (e) {
    toast(`Interrupt failed: ${String(e)}`);
  }
}
