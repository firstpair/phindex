---
title: "An index you can afford to rerun"
date: 2026-11-02
author: "Alexy Khrabrov"
slug: "phindex"
canonical_url: "https://firstpair.press/phindex/"
tags:
  - photography
  - dng
  - lightroom
  - sqlite
  - rust
  - ocaml
  - oxcaml
  - open-source
---

![Two Victorian detectives study fingerprints on cased photographs at the rim of a layered canyon, while a camel caravan crosses the desert above.](../../../cover/phindex-headboard.png)

An index of a photo archive is only useful if it is current. The day it falls behind, every answer it gives comes with an asterisk: this file has a copy on the backup, *as of the last time we looked*. And on an archive measured in terabytes, on spinning disks, "looking" means days of reading. If keeping the index current costs as much as building it, it will not be kept current, and then it is not an index but a historical document.

[phindex](https://github.com/firstpair/phindex) is our attempt at an index that costs almost nothing to rerun. It walks a directory tree and stores, in one SQLite file, every identity that [phidentify](https://github.com/firstpair/phidentify) can compute for each file: the whole-file SHA-256, the DNG raw blocks, Lightroom Classic's fingerprint, the metadata record, and the digest of everything outside the XMP packet. Then, on every later run, it does as little work as it can prove is enough. The [manual](https://firstpair.org/books/phindex/) is now a preview on the First Pair Photography and Unix shelves.

## What the index is for

phidentify, whose [manual](https://firstpair.org/books/phidentify/) is on the same shelves, is about naming the identities: "the same photo" is five questions, and an archive gets into trouble when one answer is taken for another. An index is where those answers pay off. With each identity in its own column, archive questions become joins.

Is everything under the photo library also on the backup, byte for byte? That is a `NOT EXISTS` on `sha256`. Which DNGs differ from their backup copy only inside the XMP packet, because a rating or a keyword was written into the file? That is a self-join on the preserved digest under the same grammar. Which file on disk is the original a Lightroom catalog entry recorded? That is a join of `lr_digest` against the catalog's own fingerprint table, read from a copy of the catalog.

Each of those queries takes seconds. None of them reads a photo. What they need is an index that is right.

## Doing less, safely

The obvious incremental rule is: read a file again only if it is new, or its size, mtime or inode changed. phindex follows it. A row also carries a revision stamp naming the identity definitions that produced it, so when a definition changes, say a new tiffgraph grammar, every row from the old definitions becomes stale and is read again. A release that leaves the definitions alone keeps the stamp, because changing it means reading the whole archive.

But that rule alone still costs one `stat` per file, and on a tree of more than a million files spread over hard disks, a full stat pass takes a while. The more interesting saving comes from the directories.

## What a directory's mtime knows

A directory's modification time changes when an entry is added to it, removed from it or renamed inside it. Copy a new card into the archive and the parent folder's mtime moves. Delete a reject and its folder's mtime moves. Leave a folder alone and its mtime stays put, however many years go by.

So phindex stores every directory's inode and mtime, and on a rerun it compares them first. A directory whose inode and mtime are unchanged is not listed at all: its files are taken from the index, its subdirectories from the stored tree, and each subdirectory gets the same test in turn. Only directories whose contents changed are listed. On a tree of 1.6 million files in 3,100 directories, that walk took 16 seconds. The work of a rerun becomes proportional to what changed, not to the size of the archive.

The mtime is taken before the directory is listed, so a file added while phindex is reading the listing leaves the stored mtime stale and the folder is listed again next time. The idea is not new. What makes it safe here is knowing exactly where it stops.

## Where the trick stops

A directory's mtime knows about its entries, not their contents. A file rewritten in place under the same name, by a tool appending to it or an editor saving straight into it, changes its own mtime and size, but not its folder's. The quick walk never looks at that file, so it never notices.

phindex does not pretend otherwise. `--full-stat` lists every directory and stats every file, which catches in-place rewrites at the cost of one stat per file and still no reads. The manual says when to use it: after anything that may have written into files, such as a tool that writes metadata into DNGs, and as a periodic sweep. Programs that save by writing a new file and renaming it over the old one do change the folder's mtime, and the quick walk finds them.

## Moves without reads

Archives get reorganized. A folder is renamed from a date to a place; a year is split into months. A naive index sees every file under the new name as new and reads it again.

A move within one file system keeps the file's inode, size and mtime. So when phindex finds a new path, it looks for a row elsewhere in the index with the same device, inode, size and mtime, whose path no longer exists, and lets the new path inherit its identities without a read. The old path is marked gone, not deleted, because it is evidence of what used to be there. Renaming a folder of three thousand files becomes a run that reads nothing. Two live hard links, which share an inode but both still exist, are never mistaken for a move; each is read on its own.

The same care goes into interruption. phindex commits its plan, every new and changed row and every directory it visited, before reading anything, and then commits results every 500 files. Stop it halfway through a three-day first run and the next run picks up the unread files exactly where it stopped, because they are the rows that still have no SHA-256.

## Two implementations, compared

phindex comes twice. `phindex` is a Rust binary: a rayon pool identifies files and streams the results to a single SQLite writer. `phindex-ox` is OCaml, written on phidentify's OCaml package. Under OxCaml it uses a `Domain.Safe.spawn` pool, whose closures must be portable, so the workers share nothing but an immutable array of paths and an atomic counter; under stock OCaml 5 the same source gets a `Domain.spawn` pool. In both, the tiffgraph walk that finds a DNG's XMP packet runs inside the workers, each on its own file descriptor, and work is handed to the pool in batches.

Both write the same tables with the same revision stamp, so either can continue the other's index. That makes a sharp test. We ran both, with 16 workers and a warm cache, on the same folder of 1,061 files and 4.6 GB, phindex-ox built in an OxCaml switch, and compared the two databases row by row and column by column. They were identical, error reasons included. The only difference anywhere was inside the stored metadata records: the file access time, which changes because the file was read and which the metadata digest leaves out by design.

The Rust binary finished in 13 seconds using 2.3 GB of memory; phindex-ox took 25 seconds and 3.5 GB. We did not take a stock OCaml timing, so this says nothing about stock OCaml against OxCaml, and the comparison measured totals without attributing them. The structure differs in an obvious way: the Rust pool never stops, while phindex-ox spawns its domains for each batch and waits for the batch's slowest file. For production runs we use `phindex`. `phindex-ox` earns its place as an independent second implementation in a different runtime that, so far, agrees on every byte it writes.

## Where the time actually goes

On a first run, the cost is not the walk. Most identities cost about one pass over the bytes. The exception is Lightroom's fingerprint, which needs the raw image fully decoded, about 0.85 seconds of CPU per DNG. On an archive of DNGs that makes the run CPU-bound, and more workers help up to the number of cores. Among JPEGs and sidecars there is no decode, and the disks set the pace.

On hard disks the signs to watch are the read rate and the request size. When the rate stops rising with more workers and the requests get smaller, the readers are interleaving and the heads are seeking, and fewer workers will do as well. Small sidecar files, such as Capture One's `.cop` and `.cof` files beside every image, make the MB/s figure collapse while the files per second hold steady. That looks like a slowdown and is not one.

Memory had its own lesson. Under the macOS system allocator, the metadata engine's allocation pattern fragmented the heap and kept about half a megabyte per file, without bound, in long multi-threaded runs. With mimalloc it stays flat, so phidentify's library and the phindex binary both use it.

## The point of being cheap

None of the individual tricks is new. Directory mtimes, inode matching for moves, periodic commits: each is a decades-old idea. What phindex adds is the discipline of saying exactly when each one is sound, and falling back to reading when it is not. The result is an index that can be rerun after every import, every reorganization, every backup, in seconds, and so is current when you ask it something.

The [manual](https://firstpair.org/books/phindex/) covers the schema, every incremental rule and its limits, the two implementations, tuning on hard disks, the queries, and aligning two archives. Its [PDF](https://firstpair.org/phindex/pdf/), [EPUB](https://firstpair.org/phindex/epub/) and [HTML](https://firstpair.org/read/phindex/) editions are on the shelf, and the code is MIT-licensed at [github.com/firstpair/phindex](https://github.com/firstpair/phindex). It builds on [phidentify](https://github.com/firstpair/phidentify), [tiffgraph](https://github.com/firstpair/tiffgraph), [Semantic Unison](https://github.com/querygraph/semantic-unison) and [dng-compare](https://github.com/firstpair/dng-compare).

## Update, 2026-11-02: phindex 0.2.0

The first version of this post went out with phindex 0.1.0. Within the day, running it on real archives had produced four changes worth telling, and the [manual](https://firstpair.org/books/phindex/) now covers them as version 0.2.0.

### An identity that survives a time shift

Every photographer has shot a trip with the camera clock still on home time, or an hour off after daylight saving, and then fixed it in ExifTool or Lightroom. The fix rewrites a few bytes of EXIF in every file. The pictures are untouched, but every file's SHA-256 is new, and a backup check by checksum now says that none of those photos is backed up.

For DNGs, phindex already had an answer: the raw-block identity ignores metadata. For everything else it had none. So phidentify gained a sixth identity, `image_data`, and phindex a column for it. It hashes only the bytes that hold the picture: the image strips and tiles of a TIFF or a TIFF-based raw such as NEF, CR2, ARW or ORF, the raw data of a Panasonic RW2, the sensor data of a Fujifilm RAF, and a JPEG's compressed image without its EXIF, XMP and thumbnail segments. We tested it on real files: shifting the capture time of a TIFF, an ARW, an RW2 and a JPEG with ExifTool changed every whole-file checksum and left every `image_data` equal. DNGs are left to the raw-block identity, because a DNG's strips include previews that Lightroom regenerates when it saves metadata.

### Adding it without reading the archive again

A new identity is supposed to mean a new revision stamp, and a new stamp means reading every file again. On our archives that is days of disk time, to compute one value that most files cannot have: a DNG never has `image_data`, and neither does a video, a sidecar or a Capture One settings file.

So 0.2.0 upgrades the rows of the previous revision instead. DNG rows are recognized in the index itself and settled by one SQL update, without opening a file. Every other unchanged file is opened and its first 16 bytes read: if they are not the start of a JPEG, a TIFF or a RAF, the row is settled there. Only the files that can have image data are read in full. On an archive of about 459,000 files, that left 94,455 files, 3.8 TB, to read, and settled 72,072 DNGs without touching them. On one of 1.6 million files, 83,003 files and 1.1 TB were read; the rest were not. The Rust and OCaml indexers, run side by side on real folders, again wrote identical rows, both fresh and upgraded.

### Sixteen workers, sixteen folders

Watching a run on a two-disk hard-disk RAID 0, `iostat` showed about 368 transfers a second of half a megabyte each, about 180 MB/s: nearly every read was a seek. The cause was one line. The Rust pool handed out the file list with rayon's `into_par_iter`, which splits a list into one range per thread, so sixteen workers were reading in sixteen distant parts of the archive at once, and the heads crossed the disk between almost every read. Handing the sorted list out one file at a time, in order, puts all sixteen workers in the same folder. The same array went to about 212 MB/s, and the process's CPU dropped from about 930% to about 480%, because the workers now spend their time waiting on a disk that is reading as fast as it can rather than on one that is seeking.

The manual now has a section on choosing `--workers`: compare the process's CPU with workers × 100% to see whether the CPU or the disks are the limit, read `iostat`'s transfers per second and kilobytes per transfer to see whether the disks are reading or seeking, start at 8 workers on a four-disk RAID 5, and keep the total across concurrent runs under the number of cores.

### Aligning two archives by identity

The point of all these identities is to answer one question across two archives: is everything here also there? `phindex align` answers it from two indexes, without reading a photo. It groups each folder's files into units, a raw with its JPEG and sidecars, matches the raw by its strongest shared identity, and lets the rest of the unit follow it. Each photo then gets a verdict: identical, metadata differs, image differs, partially there, missing, or not yet indexed; and flags saying whether it was moved, renamed, split across folders or duplicated. A file that shares nothing with its counterpart, like a JPEG re-rendered from its raw, is still paired, as the raw's partner. Files that exist only on the right are listed as new or as copies of something already on the left.

A trial on 1.7 million files against 1.6 million took 24 seconds and 1.6 GB of memory. The DNG pairs it finds can go straight to [dng-compare](https://github.com/firstpair/dng-compare), which compares their raw samples and metadata.

Smaller things: `--only` can be repeated to index several new folders in one command, and the first line every run prints is the commit the binary was built from, so a three-day log says which code wrote its rows.

*Colophon: written by Alexy Khrabrov with Claude as co-author. The ideas are Alexy's; Claude wrote the text and the code with him.*
