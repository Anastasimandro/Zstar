/* libarchive links archive_random() into archive_util.c for temporary-file names, which a
 * read-only RAR reader never creates. The real implementation needs a different entropy source on
 * every platform, so this build supplies one that always fails instead: if it were ever reached,
 * the caller gets an error and no predictable value. */
#include <stddef.h>

int archive_random(void *buf, size_t nbytes)
{
	(void)buf;
	(void)nbytes;
	return -1;
}
