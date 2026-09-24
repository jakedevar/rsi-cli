#!/usr/bin/env node
import fs from "fs";
import { spawnSync } from "child_process";

// Function to read JSON from stdin
function readStdinJSON() {
  try {
    if (process.stdin.isTTY) return {};
    const s = fs.readFileSync(0, "utf8");
    return s.trim() ? JSON.parse(s) : {};
  } catch {
    return {};
  }
}

// Read data and set up notification content
const data = readStdinJSON();
const cliMsg = process.argv.slice(2).join(" ");
const title = data.title || "Claude Code";
const msg = String(data.message || cliMsg || "Claude Code needs input");

let ok = false;
try {
  // This script is specifically for Arch Linux running Dunst.
  // It uses 'notify-send' to send notifications, which Dunst
  // will then display.
  ok = spawnSync("notify-send", [title, msg]).status === 0;

} catch (e) {
  // Fallback to console log if the notification attempt fails entirely
  console.log(`[Notification] ${title}: ${msg}`);
  ok = true; 
}

// Fallback to console log if notify-send wasn't successful
if (!ok) {
    console.log(`[Notification] ${title}: ${msg}`);
    ok = true; 
}

if (!ok) console.error("Notification failed");
process.exit(ok ? 0 : 2);