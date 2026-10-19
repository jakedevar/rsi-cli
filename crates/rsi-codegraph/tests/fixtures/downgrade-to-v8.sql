-- Test fixture: turn a populated v9 codegraph database back into a genuine v8
-- database. The v8 fact tables are rebuilt from the v9 compatibility views, the
-- shared block store is dropped, and user_version is stamped 8. Migration tests
-- then edit the result further (dropping later-version tables, lowering
-- user_version) exactly as they did before v9 existed.
PRAGMA foreign_keys = OFF;
CREATE TABLE legacy_files AS SELECT * FROM cg_files;
CREATE TABLE legacy_file_diagnostics AS SELECT * FROM cg_file_diagnostics;
CREATE TABLE legacy_nodes AS SELECT * FROM cg_nodes;
CREATE TABLE legacy_relations AS SELECT * FROM cg_relations;
CREATE TABLE legacy_evidence AS SELECT * FROM cg_evidence;
CREATE TABLE legacy_unresolved AS SELECT * FROM cg_unresolved_references;
DROP VIEW cg_fts_nodes;
DROP VIEW cg_unresolved_references;
DROP VIEW cg_evidence;
DROP VIEW cg_relations;
DROP VIEW cg_nodes;
DROP VIEW cg_file_diagnostics;
DROP VIEW cg_files;
DROP TABLE cg_fts_blocks;
DROP TABLE cg_block_unresolved;
DROP TABLE cg_block_evidence;
DROP TABLE cg_block_relations;
DROP TABLE cg_block_nodes;
DROP TABLE cg_manifest;
DROP TABLE cg_blocks;
ALTER TABLE legacy_files RENAME TO cg_files;
ALTER TABLE legacy_file_diagnostics RENAME TO cg_file_diagnostics;
ALTER TABLE legacy_nodes RENAME TO cg_nodes;
ALTER TABLE legacy_relations RENAME TO cg_relations;
ALTER TABLE legacy_evidence RENAME TO cg_evidence;
ALTER TABLE legacy_unresolved RENAME TO cg_unresolved_references;
CREATE VIRTUAL TABLE cg_fts_nodes USING fts5(
    name, path,
    workspace_id UNINDEXED, generation UNINDEXED,
    node_id UNINDEXED, owner_path UNINDEXED,
    tokenize='unicode61'
);
INSERT INTO cg_fts_nodes(name,path,workspace_id,generation,node_id,owner_path)
    SELECT name,path,workspace_id,generation,node_id,path FROM cg_nodes;
CREATE INDEX cg_relations_target ON cg_relations(workspace_id, generation, target_id, kind);
PRAGMA user_version = 8;
