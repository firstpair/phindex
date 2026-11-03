#!/bin/bash
# Run phindex (Rust) or, with --ox, phindex-ox (OCaml/OxCaml) on one index, from
# this checkout's build, keeping the machine awake and appending to a log.
#
#   phindex-run.sh [--ox] [--log FILE] --db INDEX.sqlite --root DIR [phindex options...]
#
# Every other option (--only, --workers, --full-stat, --progress-secs, --batch) is
# passed through unchanged. The run refuses to start while another phindex or
# phindex-ox process writes the same index. Safe to stop with Ctrl-C and rerun.
set -euo pipefail
usage() { echo "usage: phindex-run.sh [--ox] [--log FILE] --db INDEX.sqlite --root DIR [phindex options...]" >&2; exit 2; }

here=$(cd "$(dirname "$0")/.." && pwd)
bin=$here/rust/target/release/phindex
build="cd $here/rust && cargo build --release"
db= root= log= args=()
while [ $# -gt 0 ]; do
  case $1 in
    --ox) bin=$here/ocaml/_build/default/phindex_ox.exe build="cd $here/ocaml && dune build"; shift ;;
    --log|--db|--root) [ $# -ge 2 ] || usage
      case $1 in --log) log=$2 ;; --db) db=$2 ;; --root) root=$2 ;; esac; shift 2 ;;
    -h|--help) usage ;;
    *) args+=("$1"); shift ;;
  esac
done
[ -n "$db" ] && [ -n "$root" ] || usage
[ -x "$bin" ] || { echo "no build at $bin; run: $build" >&2; exit 1; }
# An absolute index path, so that the check below sees every run of this index alike.
db=$(cd "$(dirname "$db")" && pwd)/$(basename "$db")

# A writer of this index is a phindex or phindex-ox binary whose arguments name it.
writers=$(ps -axo pid=,command= | grep -E '/phindex(_ox\.exe)? ' | grep -F -e "--db $db " || true)
if [ -n "$writers" ]; then
  echo "another run is writing $db:" >&2
  echo "$writers" >&2
  exit 1
fi

cmd=("$bin" --db "$db" --root "$root" ${args[@]+"${args[@]}"})
command -v caffeinate >/dev/null && cmd=(caffeinate -i "${cmd[@]}")
if [ -n "$log" ]; then
  "${cmd[@]}" 2>&1 | tee -a "$log"
else
  "${cmd[@]}"
fi
