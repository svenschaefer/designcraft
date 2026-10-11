# Quality checks

CI runs cargo xtask ci for every pull request and push to main on Linux and Windows:
formatting, Clippy, workspace tests, asset attribution, dependency layering and
WebAssembly compilation. Linux uses pinned craft-fonts; Windows checks the fallback build.
The aggregate check is **quality gate**; requiring it for merges is a separate branch rule.

## IDML import budgets

Default imports allow at most 512 MiB of compressed input, 10,000 ZIP entries, 256 MiB per
decompressed part and 512 MiB total decompressed data. Limits apply to actual bytes read
and include unreferenced parts. Native IDML open, place and revert paths also bound the
initial file read. Library callers can use ImportLimits and import_idml_with_limits for
explicit budgets. Linked files supplied by the caller are outside the ZIP budgets.

ZIP and ZIP64 footer counts are checked before the entry index is allocated. The reader
accepts single-disk packages and rejects recovery through an older footer after malformed
directory metadata. Classic and ZIP64 counts must agree, and ZIP64 record lengths must
reach their locator. Archive comments cannot substitute an unchecked count. The footer
layout follows [PKWARE APPNOTE 6.3.10](https://pkware.cachefly.net/webdocs/casestudies/APPNOTE.TXT).
Offsets are interpreted from the start of the input. Bytes beyond the declared ZIP
archive comment are rejected.

## Command rollback

A failed command restores document states, undo and redo history, saved state, the active
document and document-switch bookkeeping. Ordinary errors and caught panics both roll
back nested edits. Filesystem writes are separate side effects; stage them before
publishing a successful command.

```sh
cargo test -p designcraft-idml --lib
cargo test -p designcraft-engine --lib guard::tests
```
