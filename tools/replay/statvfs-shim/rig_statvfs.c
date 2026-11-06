/*
 * Rig (Phase 5b WP5, item 4): a deterministic root filesystem for the
 * `system.disk_usage` reading of BOTH agents.
 *
 * The TS reads `df -kP /` (ts:src/hardware/system.ts:126-130 @09da38d) and the
 * Rust agent statvfs("/") (crates/xorgate-hal/src/system.rs read_disk), so on
 * the rig both read the shared Docker VM's disk, which other containers (and
 * the run's own recordings) move mid-replay (phase-3 gate D3 FINAL `.jsrs09`:
 * one untolerated disk_usage read). Preloaded into each agent (and inherited by
 * its `df` child), this answers statvfs()/statvfs64() for the path "/" EXACTLY
 * from $RIG_STATVFS_ROOT = "<f_blocks>:<f_bfree>:<f_bavail>:<f_frsize>" (the
 * player writes the fixture, play.rs RIG_STATVFS_ROOT) — and for every path ON
 * the root filesystem (same st_dev as "/"), because `df -kP <path>` reports
 * the MOUNT that holds <path> (its statvfs of the mount point "/"), while the
 * Rust agent statvfs()es <path> itself: the recording disk manager's probe of
 * its recordings root (ts:src/recording/disk.ts:73-78 `readDisk(root)` ↔
 * rs recording/disk.rs default_free_bytes) must see the SAME filesystem on
 * both sides. A path on another filesystem (/dev/shm, a tmpfs) and every call
 * while the variable is unset or malformed go to the real libc call. It never
 * touches statfs() (libuv's fs.statfs).
 */
#define _GNU_SOURCE
#include <dlfcn.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/stat.h>
#include <sys/statvfs.h>

static int on_root_fs(const char *path) {
  if (strcmp(path, "/") == 0) return 1;
  struct stat a, r;
  return stat(path, &a) == 0 && stat("/", &r) == 0 && a.st_dev == r.st_dev;
}

static int root_fixture(const char *path, unsigned long long v[4]) {
  const char *s = getenv("RIG_STATVFS_ROOT");
  if (path == NULL || s == NULL) return 0;
  char tail;
  if (sscanf(s, "%llu:%llu:%llu:%llu%c", &v[0], &v[1], &v[2], &v[3], &tail) != 4 || v[3] == 0) return 0;
  return on_root_fs(path);
}

#define FILL(b, v)                                     \
  do {                                                 \
    memset((b), 0, sizeof *(b));                       \
    (b)->f_bsize = (v)[3];                             \
    (b)->f_frsize = (v)[3];                            \
    (b)->f_blocks = (v)[0];                            \
    (b)->f_bfree = (v)[1];                             \
    (b)->f_bavail = (v)[2];                            \
    (b)->f_files = 1048576;                            \
    (b)->f_ffree = 917504;                             \
    (b)->f_favail = 917504;                            \
    (b)->f_namemax = 255;                              \
  } while (0)

int statvfs(const char *path, struct statvfs *buf) {
  static int (*real)(const char *, struct statvfs *);
  unsigned long long v[4];
  if (root_fixture(path, v)) {
    FILL(buf, v);
    return 0;
  }
  if (real == NULL) real = (int (*)(const char *, struct statvfs *))dlsym(RTLD_NEXT, "statvfs");
  return real(path, buf);
}

int statvfs64(const char *path, struct statvfs64 *buf) {
  static int (*real)(const char *, struct statvfs64 *);
  unsigned long long v[4];
  if (root_fixture(path, v)) {
    FILL(buf, v);
    return 0;
  }
  if (real == NULL) real = (int (*)(const char *, struct statvfs64 *))dlsym(RTLD_NEXT, "statvfs64");
  return real(path, buf);
}
