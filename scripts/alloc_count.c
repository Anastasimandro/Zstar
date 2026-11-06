/* LD_PRELOAD shim: counts calls to the C allocator and prints them to stderr at exit.
 * Used by scripts/meta-bench.sh to compare allocation counts before and after a change.
 *   gcc -O2 -shared -fPIC -o alloc_count.so alloc_count.c
 *   LD_PRELOAD=./alloc_count.so some-binary
 * Counts calls, not bytes; calls made before main and by the loader are included. */
#include <stddef.h>
#include <stdio.h>

extern void *__libc_malloc(size_t);
extern void *__libc_calloc(size_t, size_t);
extern void *__libc_realloc(void *, size_t);
extern void *__libc_memalign(size_t, size_t);
extern void __libc_free(void *);

static unsigned long n_malloc, n_calloc, n_realloc, n_aligned, n_free;
#define BUMP(c) __atomic_fetch_add(&(c), 1UL, __ATOMIC_RELAXED)

void *malloc(size_t n) { BUMP(n_malloc); return __libc_malloc(n); }
void *calloc(size_t a, size_t b) { BUMP(n_calloc); return __libc_calloc(a, b); }
void *realloc(void *p, size_t n) { BUMP(n_realloc); return __libc_realloc(p, n); }
void free(void *p) { if (p) BUMP(n_free); __libc_free(p); }
int posix_memalign(void **out, size_t align, size_t n) {
  BUMP(n_aligned);
  void *p = __libc_memalign(align, n);
  if (!p) return 12;
  *out = p;
  return 0;
}
void *aligned_alloc(size_t align, size_t n) { BUMP(n_aligned); return __libc_memalign(align, n); }

__attribute__((destructor)) static void report(void) {
  unsigned long total = n_malloc + n_calloc + n_realloc + n_aligned;
  fprintf(stderr, "alloc_calls=%lu (malloc=%lu calloc=%lu realloc=%lu aligned=%lu) free_calls=%lu\n",
          total, n_malloc, n_calloc, n_realloc, n_aligned, n_free);
}
