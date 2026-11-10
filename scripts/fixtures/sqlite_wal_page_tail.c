/*
 * Differential full-engine regression for the SQLite WAL short-page backport.
 * Synthetic files and initialized allocator poison only. Never open a profile.
 * Run in a fresh process; this changes SQLite's process-global allocator.
 */
#include "sqlite3.h"
#include <stdint.h>
#include <stddef.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/stat.h>

enum { DB_PAGE = 4096, WAL_PAGE = 512, BLOB_BYTES = 2048, CELL = 128 };
static const char *case_name = "unknown";

typedef union Allocation {
  max_align_t alignment;
  size_t bytes;
} Allocation;

static void *poison_malloc(int n) {
  Allocation *h;
  if (n <= 0 || n > 16777216) return NULL;
  h = (Allocation *)malloc(sizeof(*h) + (size_t)n);
  if (!h) return NULL;
  h->bytes = (size_t)n;
  memset(h + 1, 0xa5, (size_t)n);
  return h + 1;
}
static void poison_free(void *p) {
  if (p) free(((Allocation *)p) - 1);
}
static int poison_size(void *p) {
  return p ? (int)((((Allocation *)p) - 1)->bytes) : 0;
}
static void *poison_realloc(void *p, int n) {
  void *q;
  size_t old;
  if (!p) return poison_malloc(n);
  if (n <= 0) { poison_free(p); return NULL; }
  old = (((Allocation *)p) - 1)->bytes;
  q = poison_malloc(n);
  if (!q) return NULL;
  memcpy(q, p, old < (size_t)n ? old : (size_t)n);
  poison_free(p);
  return q;
}
static int poison_roundup(int n) {
  return n > 0 && n <= 16777208 ? (n + 7) & ~7 : 0;
}
static int poison_init(void *unused) { (void)unused; return SQLITE_OK; }
static void poison_shutdown(void *unused) { (void)unused; }

static int failure(const char *phase, int code) {
  printf("{\"case\":\"%s\",\"ok\":false,\"phase\":\"%s\",\"code\":%d}\n",
         case_name, phase, code);
  return 1;
}
static void be32(unsigned char *p, uint32_t v) {
  p[0]=(unsigned char)(v>>24); p[1]=(unsigned char)(v>>16);
  p[2]=(unsigned char)(v>>8); p[3]=(unsigned char)v;
}
static uint32_t le32(const unsigned char *p) {
  return (uint32_t)p[0] | ((uint32_t)p[1]<<8) |
         ((uint32_t)p[2]<<16) | ((uint32_t)p[3]<<24);
}
static void checksum(const unsigned char *p, size_t n, uint32_t sum[2]) {
  size_t i;
  for (i=0; i<n; i+=8) {
    sum[0] += le32(p+i) + sum[1];
    sum[1] += le32(p+i+4) + sum[0];
  }
}
static int varint(unsigned char *p, unsigned n) {
  if (n < 128) { p[0]=(unsigned char)n; return 1; }
  if (n >= 16384) return 0;
  p[0]=(unsigned char)((n>>7)|0x80);
  p[1]=(unsigned char)(n&0x7f);
  return 2;
}
static int create_database(const char *path) {
  sqlite3 *db=NULL;
  int rc=sqlite3_open_v2(path, &db,
      SQLITE_OPEN_READWRITE|SQLITE_OPEN_CREATE|SQLITE_OPEN_PRIVATECACHE|
      SQLITE_OPEN_NOMUTEX, NULL);
  if (rc == SQLITE_OK) {
    rc=sqlite3_exec(db,
      "PRAGMA page_size=4096;PRAGMA journal_mode=WAL;"
      "CREATE TABLE t(x BLOB);PRAGMA wal_checkpoint(TRUNCATE);",
      NULL, NULL, NULL);
  }
  if (db) {
    int closed=sqlite3_close(db);
    if (rc == SQLITE_OK) rc=closed;
  }
  return rc;
}
static int write_wal(const char *path, int *prefix_bytes) {
  unsigned char header[32]={0}, frame[24]={0}, page[WAL_PAGE]={0};
  char wal[1024];
  uint32_t sum[2]={0,0};
  FILE *file;
  int n, pos=CELL;
  n=snprintf(wal, sizeof(wal), "%s-wal", path);
  if (n < 0 || (size_t)n >= sizeof(wal)) return 0;
  /* A valid table-leaf cell wholly fits a 4096-byte database page. Its BLOB
   * extends beyond this WAL's 512-byte frame, which triggers the real pager. */
  page[0]=0x0d; page[3]=0; page[4]=1;
  page[5]=(unsigned char)(CELL>>8); page[6]=(unsigned char)CELL;
  page[8]=(unsigned char)(CELL>>8); page[9]=(unsigned char)CELL;
  pos += varint(page+pos, BLOB_BYTES+3);
  pos += varint(page+pos, 1);
  page[pos++]=3;
  pos += varint(page+pos, 12+2*BLOB_BYTES);
  if (pos <= CELL || pos >= WAL_PAGE) return 0;
  *prefix_bytes=WAL_PAGE-pos;
  memset(page+pos, 0x31, (size_t)*prefix_bytes);
  /* 0x377f0682 selects the little-endian checksum word interpretation.
   * WAL headers/frame integers and stored checksum values are big-endian. */
  be32(header, 0x377f0682u); be32(header+4, 3007000u);
  be32(header+8, WAL_PAGE); be32(header+16, 0x12345678u);
  be32(header+20, 0x76543210u);
  checksum(header, 24, sum);
  be32(header+24,sum[0]); be32(header+28,sum[1]);
  be32(frame,2); be32(frame+4,2);
  memcpy(frame+8,header+16,8);
  checksum(frame,8,sum); checksum(page,sizeof(page),sum);
  be32(frame+16,sum[0]); be32(frame+20,sum[1]);
  file=fopen(wal,"wb");
  if (!file) return 0;
  n=(fwrite(header,1,sizeof(header),file)==sizeof(header) &&
     fwrite(frame,1,sizeof(frame),file)==sizeof(frame) &&
     fwrite(page,1,sizeof(page),file)==sizeof(page));
  if (fclose(file)!=0) n=0;
  return n;
}
int main(int argc, char **argv) {
  sqlite3_mem_methods methods = {poison_malloc,poison_free,poison_realloc,
      poison_size,poison_roundup,poison_init,poison_shutdown,NULL};
  sqlite3 *db=NULL;
  sqlite3_stmt *statement=NULL;
  const unsigned char *blob;
  int prefix=0, bytes, i, rc, expected;
  FILE *existing;
  if (argc!=3) return failure("arguments",0);
  if (strcmp(argv[1],"baseline")==0) {case_name="baseline"; expected=0xa5;}
  else if (strcmp(argv[1],"patched")==0) {case_name="patched"; expected=0;}
  else return failure("arguments",0);
  if (strlen(argv[2])>=900) return failure("path_length",0);
  umask(0077);
  existing=fopen(argv[2],"rb");
  if (existing) {fclose(existing); return failure("path_exists",0);}
  rc=sqlite3_config(SQLITE_CONFIG_MALLOC,&methods);
  if (rc!=SQLITE_OK) return failure("allocator_config",rc);
  rc=sqlite3_config(SQLITE_CONFIG_LOOKASIDE,0,0);
  if (rc!=SQLITE_OK) return failure("lookaside_config",rc);
  rc=sqlite3_config(SQLITE_CONFIG_PAGECACHE,NULL,0,0);
  if (rc!=SQLITE_OK) return failure("pagecache_config",rc);
  rc=create_database(argv[2]);
  if (rc!=SQLITE_OK) return failure("create_database",rc);
  if (!write_wal(argv[2],&prefix)) return failure("write_wal",0);
  if (prefix<=0 || prefix>=BLOB_BYTES) return failure("fixture_prefix",0);
  rc=sqlite3_open_v2(argv[2],&db,
      SQLITE_OPEN_READWRITE|SQLITE_OPEN_PRIVATECACHE|SQLITE_OPEN_NOMUTEX,NULL);
  if (rc!=SQLITE_OK) return failure("open_fixture",rc);
  rc=sqlite3_exec(db,"PRAGMA mmap_size=0;PRAGMA cache_size=32;",NULL,NULL,NULL);
  if (rc!=SQLITE_OK) return failure("query_config",rc);
  rc=sqlite3_prepare_v2(db,"SELECT x FROM t",-1,&statement,NULL);
  if (rc!=SQLITE_OK) return failure("prepare",rc);
  rc=sqlite3_step(statement);
  if (rc!=SQLITE_ROW) return failure("first_row",rc);
  if (sqlite3_column_type(statement,0)!=SQLITE_BLOB)
    return failure("column_type",0);
  bytes=sqlite3_column_bytes(statement,0);
  blob=(const unsigned char *)sqlite3_column_blob(statement,0);
  if (!blob || bytes!=BLOB_BYTES) return failure("column_size",bytes);
  for (i=0;i<prefix;i++) if (blob[i]!=0x31) return failure("prefix_mismatch",0);
  for (i=prefix;i<bytes;i++) if (blob[i]!=expected)
    return failure("tail_mismatch",0);
  rc=sqlite3_step(statement);
  if (rc!=SQLITE_DONE) return failure("extra_row",rc);
  rc=sqlite3_finalize(statement);
  if (rc!=SQLITE_OK) return failure("finalize",rc);
  rc=sqlite3_close(db);
  if (rc!=SQLITE_OK) return failure("close",rc);
  rc=sqlite3_shutdown();
  if (rc!=SQLITE_OK) return failure("shutdown",rc);
  printf("{\"case\":\"%s\",\"ok\":true,\"prefix_bytes\":%d,"
         "\"tail_bytes\":%d,\"expected_tail\":%d}\n",
         case_name,prefix,bytes-prefix,expected);
  return 0;
}
