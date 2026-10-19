// Typed adapter over the project-file Tauri commands (src-tauri/src/files.rs).
// Each call is one rsid method (the reads, and the operator-only changes of
// #113: WriteProjectFile, CreateProjectEntry, MoveProjectEntry,
// CopyProjectEntry, TrashProjectEntry, UndoProjectFileOperation); the Rust
// side checks the shape again and rsid owns the root scope, the version fence
// and the undo log. The webview never touches a file.
import { invoke } from './api.js';
import type { DirListing, FileList, FileRead, FileRoot, Gutter, OperationResult, UndoResult, WriteResult } from './files-model.js';

export const filesApi = {
  listDirectory: (root: FileRoot, path: string, maxEntries?: number) =>
    invoke<DirListing>('list_project_directory', { request: { ...root, path, ...(maxEntries ? { max_entries: maxEntries } : {}) } }),
  listFiles: (root: FileRoot, maxPaths?: number) =>
    invoke<FileList>('list_project_files', { request: { ...root, path: '', ...(maxPaths ? { max_paths: maxPaths } : {}) } }),
  readFile: (root: FileRoot, path: string, maxBytes?: number) =>
    invoke<FileRead>('read_project_file', { request: { ...root, path, ...(maxBytes ? { max_bytes: maxBytes } : {}) } }),
  gitGutter: (root: FileRoot, path: string) =>
    invoke<Gutter>('get_project_file_git_gutter', { request: { ...root, path } }),
  /** Save `request` (files-edit-model.ts `saveRequest`): the buffer fenced by the version it started from. */
  writeFile: (request: Record<string, unknown>) => invoke<WriteResult>('write_project_file', { request }),
  createEntry: (root: FileRoot, path: string, kind: 'file' | 'dir') =>
    invoke<OperationResult>('create_project_entry', { request: { ...root, path, kind } }),
  moveEntry: (root: FileRoot, from: string, to: string) =>
    invoke<OperationResult>('move_project_entry', { request: { ...root, from, to } }),
  copyEntry: (root: FileRoot, from: string, to: string) =>
    invoke<OperationResult>('copy_project_entry', { request: { ...root, from, to } }),
  trashEntry: (root: FileRoot, path: string) =>
    invoke<OperationResult>('trash_project_entry', { request: { ...root, path } }),
  undo: (root: FileRoot) => invoke<UndoResult>('undo_project_file_operation', { request: { ...root } }),
};
