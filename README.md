# phindex

phindex builds and keeps up to date an index of every file under a directory
tree, for example a photo archive or a backup volume, in one SQLite file. Each
file is read once, and five separate identities from
[phidentify](https://github.com/firstpair/phidentify) are recorded for it:

| Column | Identity | Equal values mean |
| --- | --- | --- |
| `sha256` | whole-file SHA-256 | every byte is equal |
| `dng_blocks` | DNG encoded raw blocks | same encoded raw data; metadata may differ |
| `lr_digest` | Lightroom Classic's original fingerprint (supported DNGs) | Lightroom's matcher pairs them |
| `md_digest` | exiftool-rs metadata record, location left out | same metadata |
| `dngsem_profile` + `preserved` | strictest tiffgraph grammar and Semantic Unison's `SDE/0-preserved` digest | only the XMP packet can differ (same profile) |

An identity that doesn't apply, or fails, leaves its column NULL and its reason
in the matching `_err` column. It's never a match. The compacted metadata record
is stored too, deflated, in the `metadata` table, so records can be compared
later without reading the files again. String members over 1 KiB are replaced by
their SHA-256 and length.

## Rerunning is cheap

- **Only changed files are read.** A file is read again only if it is new, or its
  size, mtime or inode changed, or the identity definitions changed. Each row
  carries a revision stamp, and that stamp changes only when an identity's
  definition does.
- **Quick walk.** Every directory's inode and mtime are stored. Adding, removing or
  renaming an entry changes its directory's mtime. So a rerun lists only changed
  directories (new folders, added or removed files) and takes the rest from the
  index. On a tree of 1.6 million files in 3,100 directories, that walk took
  16 seconds. The first run walks everything.
- **`--full-stat`.** A file rewritten in place under the same name doesn't change its
  directory's mtime. `--full-stat` stats every file to catch that.
- **Moves and renames.** A new path whose device, inode, size and mtime equal a
  vanished, fully identified row takes over its identities without a read. A
  file moved twice marks both old paths gone.
- **`--only SUBDIR`.** Walks just a folder you've added.
- **Interruptions.** Results are committed every 500 files, so a stopped run
  resumes where it stopped.

## Two implementations, one index

- **`phindex` (Rust, `rust/`):** one binary with N worker threads. This is the
  recommended one.
- **`phindex-ox` (OCaml, `ocaml/`):** built on phidentify's OCaml package. Under an
  OxCaml switch it uses a `Domain.Safe.spawn` pool with portable closures; under
  stock OCaml 5, a `Domain.spawn` pool. It runs the tiffgraph walk inside the
  workers.

Both write the same tables with the same revision stamp, so either can continue
the other's index. On the same folder (1,061 files, 4.6 GB, 16 workers, warm
cache) the two indexes were identical in every column, reasons included. Only
the metadata records' file access time differed, which is left out of the
digest. phindex took 13 s and 2.3 GB of memory; phindex-ox took 25 s and 3.5 GB.

## Build and run

```sh
cd rust && cargo build --release && cargo test --release
./target/release/phindex --db archive.sqlite --root /path/to/archive --workers 16
./target/release/phindex --db archive.sqlite --root /path/to/archive --only /path/to/archive/new-folder
```

Progress is printed every minute (`--progress-secs`) to stderr. With hard-disk
arrays, watch the read rate and the request size: more workers help while the
CPU limits, in particular for the Lightroom fingerprint's DNG decode, until the
disks do.

phindex-ox needs phidentify's OCaml package (opam 2.1 or later, for `--subpath`),
built against phidentify's Rust library:

```sh
git clone https://github.com/firstpair/phidentify && (cd phidentify/rust && cargo build --release -p phidentify-ffi --features metadata)
PHIDENTIFY_FFI_LIBDIR=$PWD/phidentify/rust/target/release opam pin add phidentify ./phidentify --subpath ocaml
cd ocaml && dune build && ./_build/default/phindex_ox.exe --db archive.sqlite --root /path/to/archive --workers 16
```

## Queries

```sql
-- files under one root with no byte-identical copy under another
SELECT a.path FROM files a WHERE a.path LIKE '/photos/%' AND a.gone = 0
  AND NOT EXISTS (SELECT 1 FROM files b WHERE b.sha256 = a.sha256 AND b.path LIKE '/backup/%' AND b.gone = 0);

-- DNGs whose bytes differ from a copy only in the XMP packet
SELECT a.path, b.path FROM files a JOIN files b
  ON a.preserved = b.preserved AND a.dngsem_profile = b.dngsem_profile AND a.sha256 <> b.sha256;
```

## Related projects

- [phidentify](https://github.com/firstpair/phidentify): the identities, as a Rust
  library with an OCaml and OxCaml package.
- [tiffgraph](https://github.com/firstpair/tiffgraph): the bounded TIFF/DNG graph
  walker, in OCaml and Rust, that finds a DNG's editable XMP packet.
- [Semantic Unison](https://github.com/querygraph/semantic-unison): a Unison fork that
  syncs DNG metadata apart from image bytes. It negotiates the tiffgraph grammars
  and defines `SDE/0-preserved`.
- [dng-compare](https://github.com/firstpair/dng-compare): DNG raw-sample comparison
  and metadata archives, and the exiftool-rs engine phidentify uses.

## Licences

The phindex source is MIT. The binaries link rawler (LGPL-2.1), for the Lightroom
fingerprint, and exiftool-rs (GPL-3.0-or-later), for metadata, so built binaries
are covered by the GPL. phindex only reads the indexed tree; it writes only its
SQLite file.
