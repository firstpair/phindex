/* Exact lstat (nanosecond mtime) and raw deflate for the index. */
#include <string.h>
#include <stdlib.h>
#include <sys/stat.h>
#include <zlib.h>
#include <caml/mlvalues.h>
#include <caml/memory.h>
#include <caml/alloc.h>
#include <caml/fail.h>

/* None for anything but a directory or regular file; Some (is_dir, size, mtime_ns, ino, dev). */
CAMLprim value phindex_lstat(value path) {
  CAMLparam1(path);
  CAMLlocal2(t, some);
  struct stat st;
  if (lstat(String_val(path), &st) != 0 || !(S_ISDIR(st.st_mode) || S_ISREG(st.st_mode))) CAMLreturn(Val_none);
  t = caml_alloc_tuple(5);
  Store_field(t, 0, Val_bool(S_ISDIR(st.st_mode)));
  Store_field(t, 1, caml_copy_int64((int64_t)st.st_size));
  Store_field(t, 2, caml_copy_int64((int64_t)st.st_mtimespec.tv_sec * 1000000000LL + st.st_mtimespec.tv_nsec));
  Store_field(t, 3, caml_copy_int64((int64_t)st.st_ino));
  Store_field(t, 4, caml_copy_int64((int64_t)st.st_dev));
  some = caml_alloc_some(t);
  CAMLreturn(some);
}

/* Raw deflate (no zlib header), as Rust flate2's DeflateEncoder writes. */
CAMLprim value phindex_deflate(value input) {
  CAMLparam1(input);
  CAMLlocal1(out);
  z_stream z; memset(&z, 0, sizeof z);
  if (deflateInit2(&z, Z_DEFAULT_COMPRESSION, Z_DEFLATED, -15, 8, Z_DEFAULT_STRATEGY) != Z_OK) caml_failwith("deflateInit2");
  uLong cap = deflateBound(&z, caml_string_length(input));
  unsigned char *buf = malloc(cap ? cap : 1);
  if (!buf) { deflateEnd(&z); caml_raise_out_of_memory(); }
  z.next_in = (Bytef *)String_val(input); z.avail_in = caml_string_length(input);
  z.next_out = buf; z.avail_out = cap;
  int r = deflate(&z, Z_FINISH);
  size_t n = cap - z.avail_out;
  deflateEnd(&z);
  if (r != Z_STREAM_END) { free(buf); caml_failwith("deflate"); }
  out = caml_alloc_initialized_string(n, (const char *)buf);
  free(buf);
  CAMLreturn(out);
}
