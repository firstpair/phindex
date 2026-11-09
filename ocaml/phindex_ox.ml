(* phindex-ox: the OCaml/OxCaml implementation of phindex, on phidentify's OCaml
   package (an OxCaml Domain.Safe.spawn pool under an OxCaml switch, Domain.spawn
   under stock OCaml). Same database and revision stamp as the Rust phindex.

     phindex-ox --db INDEX.sqlite --root DIR [--only SUBDIR]... [--full-stat] [--workers N] [--progress-secs S] [--batch B]

   Each file read records the phidentify identities and the tiffgraph identity: dngsem_profile (strictest of dng-xmp/0, /1, /2 accepting
   the DNG) and preserved (Semantic Unison's SDE/0-preserved digest of every byte
   outside its XMP packet), and image_data (the stored image bytes of a camera
   raw, TIFF or JPEG that is not a DNG). Each file read records SHA-256, DNG raw blocks,
   Lightroom's fingerprint (supported DNGs), the metadata digest and compacted
   record, and the tiffgraph identity. A failed or inapplicable identity leaves
   its value NULL and its reason in the _err column. Files are only read. Rows
   whose ids_rev differs from this build's revision, new files and changed files
   are read; files that vanished are marked gone. Results commit every 500 files,
   so an interrupted run resumes where it stopped. A stored SHA-256 that
   disagrees with a re-read of an unchanged file is reported as an error.

   Adding files later: rerun the same command, or with --only SUBDIR (a folder
   under root) to walk just the new folder; files outside it are left as they
   are, and only files inside it can be marked gone. A file that was moved or
   renamed within root keeps its inode on HFS+/APFS: a new path whose device,
   inode, size and mtime equal a vanished, fully identified row takes over that
   row's identities without being read. Only one indexer, phindex or phindex-ox, may write an index
   at a time.

   Quick walk (default): every directory's inode and mtime are kept in `dirs`.
   Adding, removing or renaming an entry changes its directory's mtime, so a
   directory whose inode and mtime are unchanged is not listed and its files
   are not stat'ed: they are taken from the index. Only changed directories,
   such as the parent of a new folder, are listed. A file rewritten in place
   under the same name leaves its directory's mtime alone; --full-stat stats
   every file to catch that. The first walk of a tree, before `dirs` knows it,
   is always full. Existing rows keep their identities either way. *)

external lstat : string -> (bool * int64 * int64 * int64 * int64) option = "phindex_lstat"
external deflate : string -> string = "phindex_deflate"

let schema = {|
CREATE TABLE IF NOT EXISTS files (
  path TEXT PRIMARY KEY, size INTEGER NOT NULL, mtime_ns INTEGER NOT NULL, ino INTEGER NOT NULL,
  dev INTEGER NOT NULL, sha256 TEXT, hashed_unix INTEGER, seen_unix INTEGER NOT NULL, gone INTEGER NOT NULL DEFAULT 0);
CREATE INDEX IF NOT EXISTS files_sha ON files(sha256);
CREATE INDEX IF NOT EXISTS files_size ON files(size);
CREATE TABLE IF NOT EXISTS runs (root TEXT, started_unix INTEGER, finished_unix INTEGER, walked INTEGER,
  hashed INTEGER, bytes_hashed INTEGER, gone INTEGER, errors INTEGER);
CREATE TABLE IF NOT EXISTS metadata (path TEXT PRIMARY KEY, engine TEXT NOT NULL, record_deflate BLOB NOT NULL);
CREATE TABLE IF NOT EXISTS dirs (path TEXT PRIMARY KEY, ino INTEGER NOT NULL, mtime_ns INTEGER NOT NULL,
  seen_unix INTEGER NOT NULL, gone INTEGER NOT NULL DEFAULT 0);
|}

let identity_columns =
  [ "dng_blocks"; "dng_blocks_err"; "lr_digest"; "lr_err"; "md_digest"; "md_err"; "ids_rev";
    "dngsem_profile"; "dngsem_err"; "preserved"; "preserved_err"; "image_data"; "image_err" ]

(* The identity definitions rows are stamped with. A row whose ids_rev differs
   is read again, so change this only when an identity's definition changes, not
   for a phidentify release that leaves them alone. Identical in the Rust phindex. *)
let revision = "phidentify 0.1.0 +lightroom +metadata; tiffgraph dng-xmp/0,1,2 max_xmp=1MiB; SDE/0-preserved; image-data/1"

(* The stamp before image_data. Its rows keep their other identities and are
   upgraded: a DNG without a read, any other file by its first bytes, and only a
   file that can have image data (JPEG, TIFF-family, RAF) is read again. *)
let previous_revision = "phidentify 0.1.0 +lightroom +metadata; tiffgraph dng-xmp/0,1,2 max_xmp=1MiB; SDE/0-preserved"

(* Whether the file's first bytes allow image data; an unreadable file is read in
   full, so the error is reported. *)
let image_candidate path =
  match In_channel.with_open_bin path (fun ic -> In_channel.really_input_string ic Phidentify.image_data_prefix_len) with
  | Some prefix -> Phidentify.image_data_candidate prefix
  | None -> (try Phidentify.image_data_candidate (In_channel.with_open_bin path In_channel.input_all) with Sys_error _ -> true)
  | exception Sys_error _ -> true

(* Refuse a phidentify build without an identity the revision promises. *)
let check_features () =
  let v = Phidentify.version () in
  let has f = let n = String.length f in
    let rec at i = i + n <= String.length v && (String.sub v i n = f || at (i + 1)) in at 0 in
  if not (has "+lightroom" && has "+metadata") then
    failwith ("phidentify lacks identities the revision promises: " ^ v)

let engine = "exiftool-rs (via phidentify)"

module D = Sqlite3.Data

let check db rc = if not (Sqlite3.Rc.is_success rc) then failwith (Sqlite3.Rc.to_string rc ^ ": " ^ Sqlite3.errmsg db)
let exec db sql = check db (Sqlite3.exec db sql)

let with_stmt db sql f =
  let st = Sqlite3.prepare db sql in
  Fun.protect ~finally:(fun () -> ignore (Sqlite3.finalize st)) (fun () -> f st)

let run_stmt db st values =
  ignore (Sqlite3.reset st);
  List.iteri (fun i v -> check db (Sqlite3.bind st (i + 1) v)) values;
  match Sqlite3.step st with
  | Sqlite3.Rc.DONE | Sqlite3.Rc.ROW -> ()
  | rc -> check db rc

let migrate db =
  exec db schema;
  let have = Hashtbl.create 32 in
  with_stmt db "SELECT name FROM pragma_table_info('files')" (fun st ->
    while Sqlite3.step st = Sqlite3.Rc.ROW do Hashtbl.replace have (Sqlite3.column_text st 0) () done);
  List.iter (fun c -> if not (Hashtbl.mem have c) then exec db (Printf.sprintf "ALTER TABLE files ADD COLUMN %s TEXT" c))
    identity_columns;
  exec db "CREATE INDEX IF NOT EXISTS files_dng_blocks ON files(dng_blocks);
           CREATE INDEX IF NOT EXISTS files_lr ON files(lr_digest);
           CREATE INDEX IF NOT EXISTS files_md ON files(md_digest);
           CREATE INDEX IF NOT EXISTS files_preserved ON files(preserved);
           CREATE INDEX IF NOT EXISTS files_image ON files(image_data);"

type ident = { size : int64; mtime_ns : int64; ino : int64 }
type entry = { path : string; ident : ident; dev : int64 }

let now () = Int64.of_float (Unix.time ())

(* Every regular file under root; symlinks are not followed. Unreadable entries
   and non-UTF-8 names are counted as errors. *)
(* Every regular file under root, and every directory visited with its inode and
   mtime (taken before listing it). [unchanged d ino mtime] says whether d may be
   reused from the index; then [files_of d] and [subdirs_of d] stand in for
   listing it. *)
let walk ~unchanged ~files_of ~subdirs_of root errors =
  let out = ref [] and dirs = ref [] and listed = ref 0 and reused = ref 0 in
  let rec dir d ino mtime =
    dirs := (d, ino, mtime) :: !dirs;
    if unchanged d ino mtime then begin
      incr reused;
      List.iter (fun e -> out := e :: !out) (files_of d);
      List.iter (fun s -> match lstat s with
        | Some (true, _, m, i, _) -> dir s i m
        | _ -> ()) (subdirs_of d)
    end else begin
      incr listed;
      match Sys.readdir d with
      | exception Sys_error _ -> incr errors
      | names ->
        Array.iter (fun n ->
          let p = Filename.concat d n in
          match lstat p with
          | None -> ()
          | Some (true, _, m, i, _) -> dir p i m
          | Some (false, size, mtime_ns, ino, dev) ->
            if String.is_valid_utf_8 p then out := { path = p; ident = { size; mtime_ns; ino }; dev } :: !out
            else incr errors) names
    end in
  (match lstat root with
   | Some (true, _, m, i, _) -> dir root i m
   | _ -> failwith ("not a directory: " ^ root));
  Array.of_list !out, !dirs, !listed, !reused

let split = function
  | Phidentify.Not_attempted -> D.NULL, D.NULL
  | Ok v -> D.TEXT v, D.NULL
  | Failed e -> D.NULL, D.TEXT e

let progress_line ~t0 ~n ~todo ~bytes ~total ~errors ~counts:(b, l, m, s, i, u) =
  let el = Unix.gettimeofday () -. t0 in
  let rate = Int64.to_float bytes /. Float.max el 1e-9 in
  let eta = if rate > 0. then Int64.to_float (Int64.sub total bytes) /. rate else 0. in
  let e = int_of_float el in
  Printf.eprintf "+%dh%02dm %d/%d files, %.1f/%.1f GB (%.1f%%), %.0f MB/s, %d errors, ETA %dh%02dm; raw blocks %d, Lightroom %d, metadata %d, preserved %d, image data %d, upgraded unread %d\n%!"
    (e / 3600) (e mod 3600 / 60) n todo (Int64.to_float bytes /. 1e9) (Int64.to_float total /. 1e9)
    (100. *. Int64.to_float bytes /. Float.max 1. (Int64.to_float total)) (rate /. 1e6) errors
    (int_of_float eta / 3600) (int_of_float eta mod 3600 / 60) b l m s i u

(* The metadata identity applies only to files whose extension the engine
   reads (Phidentify.reads_metadata). Rows written before that rule, for other
   types (a Capture One .cop cache proxy, .ini), lose their metadata digest,
   error and record, as if never attempted; nothing is read again, and the
   revision stamp stays. Same as the Rust clear_unread_metadata. *)
let clear_unread_metadata db lo hi =
  let paths = ref [] in
  with_stmt db "SELECT path FROM files WHERE path >= ?1 AND path < ?2 AND (md_digest IS NOT NULL OR md_err IS NOT NULL)"
    (fun st ->
      List.iteri (fun i v -> check db (Sqlite3.bind st (i + 1) v)) [ D.TEXT lo; D.TEXT hi ];
      while Sqlite3.step st = Sqlite3.Rc.ROW do
        let p = Sqlite3.column_text st 0 in
        if not (Phidentify.reads_metadata p) then paths := p :: !paths
      done);
  exec db "BEGIN";
  with_stmt db "UPDATE files SET md_digest=NULL, md_err=NULL WHERE path=?1" (fun clear ->
    with_stmt db "DELETE FROM metadata WHERE path=?1" (fun record ->
      List.iter (fun p -> run_stmt db clear [ D.TEXT p ]; run_stmt db record [ D.TEXT p ]) !paths));
  exec db "COMMIT";
  List.length !paths

let update ~db_path ~root ~only ~full_stat ~workers ~progress ~batch =
  check_features ();
  let rev = revision in
  let root = Unix.realpath root in
  let scope = match only with
    | None -> root
    | Some d ->
      let d = Unix.realpath d in
      if not (String.starts_with ~prefix:(root ^ "/") d) then failwith ("--only must be under --root: " ^ d);
      d in
  let db = Sqlite3.db_open db_path in
  exec db "PRAGMA journal_mode=WAL;";
  migrate db;
  let started = now () in
  let lo = scope ^ "/" and hi = scope ^ "0" in
  let upgraded_dngs =
    with_stmt db "UPDATE files SET image_data=NULL, image_err=?1, ids_rev=?2 WHERE ids_rev IS ?3
                  AND sha256 IS NOT NULL AND dng_blocks IS NOT NULL AND path >= ?4 AND path < ?5" (fun st ->
      run_stmt db st [ D.TEXT Phidentify.image_data_dng; D.TEXT rev; D.TEXT previous_revision; D.TEXT lo; D.TEXT hi ];
      Sqlite3.changes db) in
  if upgraded_dngs > 0 then Printf.eprintf "upgrade: %d DNG rows given image_data without a read\n%!" upgraded_dngs;
  let cleared = clear_unread_metadata db lo hi in
  if cleared > 0 then
    Printf.eprintf "upgrade: %d rows of file types the engine does not read: metadata cleared, not attempted\n%!" cleared;
  let known = Hashtbl.create 500_000 and was_gone = Hashtbl.create 1024 in
  let upgradable = Hashtbl.create 1024 in
  let files_by_dir = Hashtbl.create 50_000 in
  with_stmt db "SELECT path, size, mtime_ns, ino, sha256 IS NOT NULL AND ids_rev IS ?3, sha256, gone, dev,
                sha256 IS NOT NULL AND ids_rev IS ?4 FROM files WHERE path >= ?1 AND path < ?2"
    (fun st ->
      List.iteri (fun i v -> check db (Sqlite3.bind st (i + 1) v)) [ D.TEXT lo; D.TEXT hi; D.TEXT rev; D.TEXT previous_revision ];
      while Sqlite3.step st = Sqlite3.Rc.ROW do
        let i64 k = match Sqlite3.column st k with D.INT v -> v | _ -> -1L in
        Hashtbl.replace known (Sqlite3.column_text st 0)
          ({ size = i64 1; mtime_ns = i64 2; ino = i64 3 }, Sqlite3.column_bool st 4,
           (match Sqlite3.column st 5 with D.TEXT s -> Some s | _ -> None));
        let path = Sqlite3.column_text st 0 in
        if Sqlite3.column_bool st 8 then Hashtbl.replace upgradable path ();
        if Sqlite3.column_bool st 6 then Hashtbl.replace was_gone path ()
        else Hashtbl.add files_by_dir (Filename.dirname path)
            { path; ident = { size = i64 1; mtime_ns = i64 2; ino = i64 3 }; dev = i64 7 }
      done);
  let dirs_known = Hashtbl.create 50_000 and subdirs = Hashtbl.create 50_000 in
  with_stmt db "SELECT path, ino, mtime_ns FROM dirs WHERE gone=0 AND (path=?1 OR (path >= ?2 AND path < ?3))" (fun st ->
    List.iteri (fun i v -> check db (Sqlite3.bind st (i + 1) v)) [ D.TEXT scope; D.TEXT lo; D.TEXT hi ];
    while Sqlite3.step st = Sqlite3.Rc.ROW do
      let i64 k = match Sqlite3.column st k with D.INT v -> v | _ -> -1L in
      let path = Sqlite3.column_text st 0 in
      Hashtbl.replace dirs_known path (i64 1, i64 2);
      if path <> scope then Hashtbl.add subdirs (Filename.dirname path) path
    done);
  let errors = ref 0 in
  let t_walk = Unix.gettimeofday () in
  let entries, visited, listed, reused =
    walk scope errors
      ~unchanged:(fun d ino mtime -> (not full_stat) && Hashtbl.find_opt dirs_known d = Some (ino, mtime))
      ~files_of:(Hashtbl.find_all files_by_dir) ~subdirs_of:(Hashtbl.find_all subdirs) in
  Printf.eprintf "walk: %d directories, %d listed, %d unchanged and taken from the index%s, %.0f s\n%!"
    (List.length visited) listed reused (if full_stat then " (--full-stat)" else "") (Unix.gettimeofday () -. t_walk);
  let seen = now () in
  let present = Hashtbl.create (Array.length entries) in
  Array.iter (fun e -> Hashtbl.replace present e.path ()) entries;
  let changed = List.filter (fun e -> match Hashtbl.find_opt known e.path with
    | None -> true | Some (id, is_done, _) -> id <> e.ident || not is_done) (Array.to_list entries) in
  let fresh = List.filter (fun e -> match Hashtbl.find_opt known e.path with
    | None -> true | Some (id, _, _) -> id <> e.ident) changed in
  let gone = Hashtbl.fold (fun p _ acc -> if Hashtbl.mem present p || Hashtbl.mem was_gone p then acc else p :: acc) known [] in
  (* A path marked gone that is back, unchanged, is present again (a changed one is reset as fresh). *)
  let returned = List.filter (fun e -> Hashtbl.mem was_gone e.path &&
    (match Hashtbl.find_opt known e.path with Some (id, _, _) -> id = e.ident | None -> false)) (Array.to_list entries) in
  (* Moves and renames: a new path with the device, inode, size and mtime of a
     fully identified row that is gone, here or in an earlier run, inherits it. *)
  (* Every vanished row with the same identity is marked gone (a file moved twice
     leaves two), including sources outside --only, which this walk didn't cover. *)
  let moved = Hashtbl.create 64 and moved_from = ref [] in
  let fresh_new = List.filter (fun e -> not (Hashtbl.mem known e.path)) fresh in
  if fresh_new <> [] then
    with_stmt db "SELECT path, gone FROM files WHERE dev=?1 AND ino=?2 AND size=?3 AND mtime_ns=?4 AND ids_rev IS ?5
                  AND sha256 IS NOT NULL AND path <> ?6" (fun st ->
      List.iter (fun e ->
        ignore (Sqlite3.reset st);
        List.iteri (fun i v -> check db (Sqlite3.bind st (i + 1) v))
          [ D.INT e.dev; D.INT e.ident.ino; D.INT e.ident.size; D.INT e.ident.mtime_ns; D.TEXT rev; D.TEXT e.path ];
        let rows = ref [] in
        while Sqlite3.step st = Sqlite3.Rc.ROW do
          rows := (Sqlite3.column_text st 0, Sqlite3.column_bool st 1) :: !rows
        done;
        (* Only from paths that no longer exist: two live hard links are read separately. *)
        let vanished = List.filter (fun (p, _) -> not (Hashtbl.mem present p) && not (Sys.file_exists p)) (List.rev !rows) in
        match vanished with
        | (old, _) :: _ ->
          Hashtbl.replace moved e.path old;
          List.iter (fun (p, g) -> if not g then moved_from := p :: !moved_from) vanished
        | [] -> ()) fresh_new);
  exec db "BEGIN";
  with_stmt db "INSERT INTO files (path,size,mtime_ns,ino,dev,sha256,hashed_unix,seen_unix,gone) VALUES (?1,?2,?3,?4,?5,NULL,NULL,?6,0)
     ON CONFLICT(path) DO UPDATE SET size=excluded.size, mtime_ns=excluded.mtime_ns, ino=excluded.ino, dev=excluded.dev,
     sha256=NULL, hashed_unix=NULL, seen_unix=excluded.seen_unix, gone=0, dng_blocks=NULL, dng_blocks_err=NULL,
     lr_digest=NULL, lr_err=NULL, md_digest=NULL, md_err=NULL, ids_rev=NULL, dngsem_profile=NULL, dngsem_err=NULL,
     preserved=NULL, preserved_err=NULL, image_data=NULL, image_err=NULL" (fun st ->
    List.iter (fun e -> run_stmt db st [ D.TEXT e.path; D.INT e.ident.size; D.INT e.ident.mtime_ns; D.INT e.ident.ino;
                                         D.INT e.dev; D.INT seen ]) fresh);
  with_stmt db "DELETE FROM metadata WHERE path=?1" (fun st -> List.iter (fun e -> run_stmt db st [ D.TEXT e.path ]) fresh);
  with_stmt db "UPDATE files SET gone=1 WHERE path=?1" (fun st -> List.iter (fun p -> run_stmt db st [ D.TEXT p ]) gone);
  with_stmt db "UPDATE files SET gone=0 WHERE path=?1" (fun st -> List.iter (fun e -> run_stmt db st [ D.TEXT e.path ]) returned);
  let seen_dirs = Hashtbl.create (List.length visited) in
  with_stmt db "INSERT INTO dirs (path, ino, mtime_ns, seen_unix, gone) VALUES (?1,?2,?3,?4,0)
                ON CONFLICT(path) DO UPDATE SET ino=excluded.ino, mtime_ns=excluded.mtime_ns, seen_unix=excluded.seen_unix, gone=0"
    (fun st -> List.iter (fun (d, ino, mtime) -> Hashtbl.replace seen_dirs d ();
                  run_stmt db st [ D.TEXT d; D.INT ino; D.INT mtime; D.INT seen ]) visited);
  with_stmt db "UPDATE dirs SET gone=1 WHERE path=?1" (fun st ->
    Hashtbl.iter (fun d _ -> if not (Hashtbl.mem seen_dirs d) then run_stmt db st [ D.TEXT d ]) dirs_known);
  with_stmt db "UPDATE files SET (sha256, hashed_unix, dng_blocks, dng_blocks_err, lr_digest, lr_err, md_digest, md_err, ids_rev,
                  dngsem_profile, dngsem_err, preserved, preserved_err, image_data, image_err) =
                  (SELECT sha256, hashed_unix, dng_blocks, dng_blocks_err, lr_digest, lr_err, md_digest, md_err, ids_rev,
                          dngsem_profile, dngsem_err, preserved, preserved_err, image_data, image_err FROM files WHERE path=?2)
                  WHERE path=?1" (fun st ->
    Hashtbl.iter (fun p old -> run_stmt db st [ D.TEXT p; D.TEXT old ]) moved);
  with_stmt db "INSERT OR REPLACE INTO metadata SELECT ?1, engine, record_deflate FROM metadata WHERE path=?2" (fun st ->
    Hashtbl.iter (fun p old -> run_stmt db st [ D.TEXT p; D.TEXT old ]) moved);
  with_stmt db "UPDATE files SET gone=1 WHERE path=?1" (fun st ->
    List.iter (fun old -> run_stmt db st [ D.TEXT old ]) !moved_from);
  exec db "COMMIT";
  let changed = List.filter (fun e -> not (Hashtbl.mem moved e.path)) changed in
  let todo = Array.of_list changed in
  let total = ref (Array.fold_left (fun a e -> Int64.add a e.ident.size) 0L todo) in
  (* Unchanged rows of the previous revision: only image_data is missing. *)
  let upgrade = Hashtbl.create 1024 in
  Array.iter (fun e -> match Hashtbl.find_opt known e.path with
    | Some (id, _, _) when id = e.ident && Hashtbl.mem upgradable e.path -> Hashtbl.replace upgrade e.path ()
    | _ -> ()) todo;
  Printf.eprintf "walked %d files under %s, %d to read (%.1f GB; %d of them only to add image_data, by their first bytes unless they can have it), %d moved or renamed (identities kept), %d gone, %d errors, %d workers (%s), identities: %s\n%!"
    (Array.length entries) scope (Array.length todo) (Int64.to_float !total /. 1e9) (Hashtbl.length upgrade) (Hashtbl.length moved)
    (List.length gone) !errors workers Phidentify.backend rev;
  let stored_sha = Hashtbl.create 1024 in
  Array.iter (fun e -> match Hashtbl.find_opt known e.path with
    | Some (id, _, Some sha) when id = e.ident -> Hashtbl.replace stored_sha e.path sha | _ -> ()) todo;
  let request = { Phidentify.all with compact_record = true } in
  let upd = Sqlite3.prepare db
    "UPDATE files SET sha256=?1, hashed_unix=?2, dng_blocks=?3, dng_blocks_err=?4, lr_digest=?5, lr_err=?6,
     md_digest=?7, md_err=?8, ids_rev=?9, dngsem_profile=?10, dngsem_err=?11, preserved=?12, preserved_err=?13,
     image_data=?14, image_err=?15 WHERE path=?16" in
  let not_image = Sqlite3.prepare db "UPDATE files SET image_data=NULL, image_err=?1, ids_rev=?2 WHERE path=?3" in
  let meta = Sqlite3.prepare db "INSERT OR REPLACE INTO metadata VALUES (?1,?2,?3)" in
  let t0 = Unix.gettimeofday () and last = ref (Unix.gettimeofday ()) in
  let n = ref 0 and hashed = ref 0 and bytes = ref 0L and sha_changed = ref 0 in
  let cb = ref 0 and cl = ref 0 and cm = ref 0 and cs = ref 0 and ci = ref 0 and cu = ref 0 in
  let since_commit = ref 0 in
  exec db "BEGIN";
  let count r = function Phidentify.Ok _ -> incr r | _ -> () in
  let len = Array.length todo in
  let i = ref 0 in
  while !i < len do
    let chunk = Array.sub todo !i (min batch (len - !i)) in
    i := !i + Array.length chunk;
    (* Upgraded rows whose first bytes rule out image data are settled without a read. *)
    let chunk = Array.of_list (List.filter (fun e ->
      if Hashtbl.mem upgrade e.path && not (image_candidate e.path) then begin
        run_stmt db not_image [ D.TEXT Phidentify.image_data_not_applicable; D.TEXT rev; D.TEXT e.path ];
        incr n; incr since_commit; incr cu; total := Int64.sub !total e.ident.size;
        false
      end else true) (Array.to_list chunk)) in
    let results = Phidentify.identify_all ~request ~workers (Array.to_list (Array.map (fun e -> e.path) chunk)) in
    List.iteri (fun k r ->
      let e = chunk.(k) in
      incr n; incr since_commit;
      let unchanged () = match lstat e.path with
        | Some (false, size, mtime_ns, ino, _) -> { size; mtime_ns; ino } = e.ident | _ -> false in
      match r with
      | Ok (id : Phidentify.identity) when unchanged () ->
        (match Hashtbl.find_opt stored_sha e.path with
         | Some old when old <> id.sha256 ->
           incr sha_changed; incr errors;
           Printf.eprintf "SHA256 CHANGED with unchanged size/mtime/inode: %s\n%!" e.path
         | _ -> ());
        let b, be = split id.dng_raw_blocks and l, le = split id.lightroom and m, me = split id.metadata in
        let s, se = split id.dng_semantic and p, pe = split id.preserved and im, ie = split id.image_data in
        count cb id.dng_raw_blocks; count cl id.lightroom; count cm id.metadata; count cs id.preserved; count ci id.image_data;
        run_stmt db upd [ D.TEXT id.sha256; D.INT (now ()); b; be; l; le; m; me; D.TEXT rev; s; se; p; pe; im; ie; D.TEXT e.path ];
        Option.iter (fun json -> run_stmt db meta [ D.TEXT e.path; D.TEXT engine; D.BLOB (deflate json) ]) id.metadata_record;
        incr hashed; bytes := Int64.add !bytes e.ident.size
      | Ok _ ->
        incr errors; if !errors <= 50 then Printf.eprintf "ERROR %s: changed since the walk\n%!" e.path
      | Error reason ->
        incr errors; if !errors <= 50 then Printf.eprintf "ERROR %s: %s\n%!" e.path reason) results;
    if !since_commit >= 500 then begin exec db "COMMIT"; exec db "BEGIN"; since_commit := 0 end;
    if Unix.gettimeofday () -. !last >= progress then begin
      last := Unix.gettimeofday ();
      progress_line ~t0 ~n:!n ~todo:len ~bytes:!bytes ~total:!total ~errors:!errors ~counts:(!cb, !cl, !cm, !cs, !ci, !cu)
    end
  done;
  exec db "COMMIT";
  ignore (Sqlite3.finalize upd); ignore (Sqlite3.finalize meta); ignore (Sqlite3.finalize not_image);
  with_stmt db "INSERT INTO runs VALUES (?1,?2,?3,?4,?5,?6,?7,?8)" (fun st ->
    run_stmt db st [ D.TEXT scope; D.INT started; D.INT (now ()); D.INT (Int64.of_int (Array.length entries));
                     D.INT (Int64.of_int !hashed); D.INT !bytes; D.INT (Int64.of_int (List.length gone));
                     D.INT (Int64.of_int !errors) ]);
  Printf.eprintf "done: read %d files, %.1f GB, %d errors (%d stored SHA-256 mismatches); raw blocks %d, Lightroom %d, metadata %d, preserved %d, image data %d; upgraded without a read %d\n%!"
    !hashed (Int64.to_float !bytes /. 1e9) !errors !sha_changed !cb !cl !cm !cs !ci !cu;
  ignore (Sqlite3.db_close db)

let () =
  Printf.eprintf "phindex-ox %s built %s (%s)\n%!" Build_info.commit Build_info.built Phidentify.backend;
  let args = Array.to_list Sys.argv in
  let rec get flag = function a :: v :: _ when a = flag -> Some v | _ :: rest -> get flag rest | [] -> None in
  let need flag = match get flag args with Some v -> v | None -> prerr_endline (flag ^ " required"); exit 2 in
  let int flag default = Option.fold ~none:default ~some:int_of_string (get flag args) in
  let workers = int "--workers" 8 in
  if workers < 1 then (prerr_endline "--workers must be at least 1"; exit 2);
  (* --only may be repeated: each folder is indexed in turn, in the order given. *)
  let rec onlys = function "--only" :: v :: rest -> v :: onlys rest | _ :: rest -> onlys rest | [] -> [] in
  let run only = update ~db_path:(need "--db") ~root:(need "--root") ~only ~full_stat:(List.mem "--full-stat" args) ~workers
      ~progress:(Float.of_int (int "--progress-secs" 60)) ~batch:(int "--batch" (workers * 8)) in
  match onlys args with [] -> run None | dirs -> List.iter (fun d -> run (Some d)) dirs
