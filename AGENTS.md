# phindex: agent notes

Shared rules for every repository under `~/src` are in `~/src/firstpair/AGENTS.md`
(toolchains; distinguish stock OCaml from OxCaml). Local constraints:

- `phindex` (Rust, `rust/`) and `phindex-ox` (OCaml/OxCaml, `ocaml/`) must write the
  same schema and the same `REVISION` stamp, and agree row for row on the same tree,
  reasons included. Change both together, and recheck them against each other.
- Change `REVISION` only when an identity's definition changes; a new stamp makes
  every existing index read every file again.
- The indexed tree is only ever read. Tests use disposable synthetic trees, never a
  real archive, catalog or personal path.
- Identities come from phidentify; don't reimplement them here.
