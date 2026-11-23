/* archive.h includes this on Android. Upstream ships it in contrib/ to map the large-file calls on
   32-bit targets. We build 64-bit Android only, where off_t is already 64 bits, so nothing is needed. */
