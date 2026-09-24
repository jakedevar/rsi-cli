# Provisional migration declarations

Add one uniquely named JSON file here in the same commit as a new SQLite
migration. The rolling lander uses it to assign the next version after fetching
the current `rolling` tip. The declaration is bound to the accepted source
commit; it is retained as historical evidence after landing.

```json
{
  "schema_version": 1,
  "version": 130,
  "files": [
    {
      "path": "crates/rsid/src/store/mod.rs",
      "path_template": "crates/rsid/src/store/mod.rs",
      "source_blob": "sha256:<64 lowercase hex digits>",
      "sites": [
        {
          "anchor": "pub const LATEST_SCHEMA_VERSION: i32 = 130;",
          "replacement": "pub const LATEST_SCHEMA_VERSION: i32 = ${VERSION};",
          "scope": "head"
        },
        {
          "anchor": "if version < 130 {",
          "replacement": "if version < ${VERSION} {",
          "scope": "unit"
        }
      ]
    }
  ]
}
```

List every occurrence of the provisional number in each changed source or test
file. Each anchor must occur exactly once in its source file. Its replacement
must render byte for byte to the anchor when `${VERSION}` is set to the
provisional number. Use `scope: "unit"` for migration gates, helper names,
catalog markers, and fixture logic tied to that migration. Use `scope: "head"`
for the latest schema constant and tests that must track the final schema head
after all accepted units land. A versioned filename needs a matching
`path_template` containing `${VERSION}`. The source blob digest is SHA-256 of
the complete file bytes at the accepted source commit.

The lander validates the source inventory, immutable released prefix, unique
site anchors and complete version occurrence coverage before making a private
transform. It recomputes the manifest and refuses semantic conflicts. Existing
migration code and fingerprints are never edited after landing.
