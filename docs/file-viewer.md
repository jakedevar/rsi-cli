# File Viewer

The file viewer is a vim-modal editor for inspecting and editing files within
the TUI. It supports syntax highlighting, code folding, search, and markdown
preview.

## External Edit Detection

The file viewer tracks the disk content snapshot (`disk_content`) from the
last load or save. When a file is reopened from the cache, the viewer
compares the fresh disk content to this snapshot:

- **No external change**: the cached viewer is reused, preserving cursor
  position, folds, undo/redo history, and unsaved edits.
- **External change, no unsaved edits**: the stale cache is discarded and the
  file is reloaded from disk.
- **External change, unsaved edits present**: the user's draft is preserved
  and `external_conflict` is set. A warning indicator appears at the bottom
  of the viewer. The user must resolve the conflict explicitly.

The recent-file, file-explorer, and right-click open paths perform this detection.

## Conflict Resolution

When an external conflict is detected:

| Command | Action |
|---------|--------|
| `:e!`   | Discard the draft and reload from disk. Clears the conflict. |
| `:w!`  | Force-save the draft, overwriting the external content. Clears the conflict. |
| `:w`   | Refused while a conflict is active. |
| `:q`   | Refused if the buffer is dirty (use `:q!` to discard). |

The conflict indicator (`⚠ External change — :e! reload | :w! overwrite`)
appears at the bottom of the viewer when no search or command prompt is
active. The bottom row gives live search input first priority, then live
command input, then the conflict indicator, then a finished search's match
count. This keeps `:w!` and `:e!` visible while they are being typed.

## Save Fencing

The `:w` command and `Ctrl+S` fence against external disk changes:

1. Read current disk content.
2. Compare to the `disk_content` snapshot.
3. If they differ and the buffer also differs from disk: refuse the save,
   set `external_conflict`, and notify the user.
4. If they differ but the buffer matches disk: treat as a no-op (the
   content is already current), update the snapshot, and clear dirty.
5. If they match: write the buffer to disk, update the snapshot, and clear
   dirty/conflict.

A same-content metadata-only change (e.g. `touch`) does not trigger a
conflict because the comparison is content-based, not timestamp-based. A
deleted file allows the write to proceed (creating a new file). A
deleted-then-recreated file with different content is detected as an
external change.

The read-compare-write sequence has an inherent TOCTOU window. Full
atomicity would require OS-level file locking, which is out of scope. The
fence catches the common case where an external edit occurred before the
save was initiated.

`:wq` closes the viewer only after a successful save. A failed write leaves the
buffer open even when it had no unsaved edits.
