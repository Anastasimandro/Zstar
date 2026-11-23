/* Hand-written replacement for libarchive's generated config.h: only what the RAR and RAR5
 * readers need. Everything optional (zlib, bzlib, lzma, zstd, lz4, openssl, ACL and xattr
 * libraries, iconv, locale conversion) is left undefined on purpose. */
#ifndef ZSTAR_LIBARCHIVE_CONFIG_H
#define ZSTAR_LIBARCHIVE_CONFIG_H

#define __LIBARCHIVE_CONFIG_H_INCLUDED 1
#define ARCHIVE_VERSION_ONLY_STRING "3.8.11"
#define ARCHIVE_VERSION_STRING "libarchive 3.8.11"

#define HAVE_STDINT_H 1
#define HAVE_INTTYPES_H 1
#define HAVE_STDLIB_H 1
#define HAVE_STRING_H 1
#define HAVE_ERRNO_H 1
#define HAVE_LIMITS_H 1
#define HAVE_WCHAR_H 1
#define HAVE_SYS_TYPES_H 1
#define HAVE_SYS_STAT_H 1
#define HAVE_FCNTL_H 1
#define HAVE_SIGNAL_H 1
#define HAVE_TIME_H 1

#if !defined(_WIN32)
#define HAVE_UNISTD_H 1
#define HAVE_STRINGS_H 1
#endif

#define HAVE_MEMMOVE 1
#define HAVE_STRERROR 1
#define HAVE_STRCHR 1
#define HAVE_STRRCHR 1
#define HAVE_WCSCPY 1
#define HAVE_WCSLEN 1
#define HAVE_WMEMCMP 1
#define HAVE_WMEMCPY 1
#define HAVE_WMEMMOVE 1

#define HAVE_DECL_INT32_MAX 1
#define HAVE_DECL_INT32_MIN 1
#define HAVE_DECL_INT64_MAX 1
#define HAVE_DECL_INT64_MIN 1
#define HAVE_DECL_UINT32_MAX 1
#define HAVE_DECL_UINT64_MAX 1
#define HAVE_DECL_SIZE_MAX 1
#define HAVE_DECL_SSIZE_MAX 1
#define HAVE_INTMAX_T 1
#define HAVE_UINTMAX_T 1
#define HAVE_LONG_LONG_INT 1
#define HAVE_UNSIGNED_LONG_LONG 1
#define HAVE_UNSIGNED_LONG_LONG_INT 1

#if defined(_WIN32)
#define SIZEOF_WCHAR_T 2
#else
#define SIZEOF_WCHAR_T 4
#endif

#endif
