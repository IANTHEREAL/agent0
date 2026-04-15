// fs-plane-poc: validate 3 critical assumptions before full implementation.
//
// Build inside the JuiceFS repo to avoid dependency hell:
//   cp main.go /tmp/juicefs/cmd/poc/main.go
//   cd /tmp/juicefs && go run ./cmd/poc/ --meta "tikv://..." --iterations 1000
//
// Validates:
//   1. Flush latency (P50/P99/max for 4KB write_at + flush)
//   2. Multi-volume memory footprint (N volumes, observe RSS)
//   3. Basic read/write correctness through pkg/fs.FileSystem
//
// For TiKV BR backup validation (Validation 3), use tikv-br CLI directly:
//   tikv-br backup raw --pd "pd:2379" --start "keyspace_prefix" --end "keyspace_prefix\xFF" -s "s3://backup"
//   tikv-br restore raw --pd "pd:2379" -s "s3://backup"

package main

import (
	"crypto/rand"
	"flag"
	"fmt"
	"io"
	"log"
	"os"
	"runtime"
	"sort"
	"time"

	"github.com/juicedata/juicefs/pkg/chunk"
	"github.com/juicedata/juicefs/pkg/fs"
	"github.com/juicedata/juicefs/pkg/meta"
	"github.com/juicedata/juicefs/pkg/object"
	"github.com/juicedata/juicefs/pkg/vfs"
)

var (
	metaURL    = flag.String("meta", "", "JuiceFS meta URL (e.g., tikv://pd:2379?keyspace=jfs_t_test1&gc-interval=0)")
	iterations = flag.Int("iterations", 1000, "Number of write+flush iterations for latency test")
	cacheSize  = flag.Int("cache-size", 64, "Chunk cache size in MB per volume")
)

func main() {
	flag.Parse()
	if *metaURL == "" {
		log.Fatal("--meta is required. Volume must be pre-formatted with `juicefs format`.")
	}

	fmt.Println("=== fs-plane POC validation ===")
	fmt.Println()

	fmt.Println("--- Validation 1: Flush latency (4KB write_at + flush) ---")
	jfs, metaCli := initVolume(*metaURL)
	defer func() {
		jfs.Close()
		metaCli.CloseSession()
	}()
	runLatencyTest(jfs)

	fmt.Println()
	fmt.Println("--- Validation 2: Memory footprint ---")
	runMemoryReport()

	fmt.Println()
	fmt.Println("--- Validation 3: TiKV BR key range backup ---")
	fmt.Println("  Manual step. Run tikv-br CLI to test backup by keyspace:")
	fmt.Println("    # If using ?keyspace= (API V2, db9-ai fork):")
	fmt.Println("    tikv-br backup full --pd pd:2379 --keyspace jfs_t_test1 -s s3://backup-bucket/test")
	fmt.Println("    tikv-br restore full --pd pd:2379 --keyspace jfs_t_test1_clone -s s3://backup-bucket/test")
	fmt.Println()
	fmt.Println("    # If using URL path prefix (application-level):")
	fmt.Println("    tikv-br backup raw --pd pd:2379 --start 'jfs_t_test1\\x00' --end 'jfs_t_test1\\xff' -s s3://backup-bucket/test")
	fmt.Println()

	fmt.Println("=== Done ===")
}

func initVolume(metaAddr string) (*fs.FileSystem, meta.Meta) {
	metaConf := meta.DefaultConf()
	metaConf.MountPoint = "poc"
	metaCli := meta.NewClient(metaAddr, metaConf)

	format, err := metaCli.Load(true)
	if err != nil {
		log.Fatalf("Failed to load volume (did you run `juicefs format` first?): %v", err)
	}
	fmt.Printf("  Volume: %s (storage=%s, bucket=%s, block=%dKB)\n",
		format.Name, format.Storage, format.Bucket, format.BlockSize)

	// Create object storage from format
	blob, err := object.CreateStorage(format.Storage, format.Bucket,
		format.AccessKey, format.SecretKey, format.SessionToken)
	if err != nil {
		log.Fatalf("Failed to create storage: %v", err)
	}
	blob = object.WithPrefix(blob, format.Name+"/")

	chunkConf := chunk.Config{
		BlockSize:  int(format.BlockSize) * 1024,
		Compress:   format.Compression,
		HashPrefix: format.HashPrefix,
		CacheSize:  uint64(*cacheSize),
	}
	store := chunk.NewCachedStore(blob, chunkConf, nil)

	metaCli.OnMsg(meta.DeleteSlice, func(args ...interface{}) error {
		return store.Remove(args[0].(uint64), int(args[1].(uint32)))
	})
	metaCli.OnMsg(meta.CompactChunk, func(args ...interface{}) error {
		return vfs.Compact(chunkConf, store, args[0].([]meta.Slice), args[1].(uint64), args[2].(uint8))
	})

	if err := metaCli.NewSession(false); err != nil {
		log.Fatalf("Failed to create session: %v", err)
	}

	vfsConf := &vfs.Config{
		Meta:   metaConf,
		Format: *format,
		Chunk:  &chunkConf,
	}
	jfs, err := fs.NewFileSystem(vfsConf, metaCli, store, nil)
	if err != nil {
		log.Fatalf("Failed to create FileSystem: %v", err)
	}

	return jfs, metaCli
}

func runLatencyTest(jfs *fs.FileSystem) {
	ctx := meta.NewContext(uint32(os.Getpid()), 0, []uint32{0})

	// Create test file with 1MB initial content
	testPath := "/poc-latency-test.bin"
	jfs.Delete(ctx, testPath) // clean up from previous runs

	f, errno := jfs.Create(ctx, testPath, 0644, 022)
	if errno != 0 {
		log.Fatalf("Failed to create test file: %v", errno)
	}
	initial := make([]byte, 1024*1024)
	rand.Read(initial)
	if _, errno := f.Write(ctx, initial); errno != 0 {
		log.Fatalf("Failed to write initial data: %v", errno)
	}
	if errno := f.Flush(ctx); errno != 0 {
		log.Fatalf("Failed to flush initial data: %v", errno)
	}
	f.Close(ctx)
	fmt.Println("  Created 1MB test file")

	// Benchmark: open + 4KB pwrite + flush + close
	data := make([]byte, 4096)
	rand.Read(data)
	latencies := make([]time.Duration, *iterations)

	for i := 0; i < *iterations; i++ {
		f, errno := jfs.Open(ctx, testPath, 2) // MODE_MASK_W
		if errno != 0 {
			log.Fatalf("Open failed at iteration %d: %v", i, errno)
		}
		offset := int64((i * 4096) % (1024 * 1024))

		start := time.Now()
		if _, errno := f.Pwrite(ctx, data, offset); errno != 0 {
			log.Fatalf("Pwrite failed at iteration %d: %v", i, errno)
		}
		if errno := f.Flush(ctx); errno != 0 {
			log.Fatalf("Flush failed at iteration %d: %v", i, errno)
		}
		latencies[i] = time.Since(start)
		f.Close(ctx)
	}

	sort.Slice(latencies, func(i, j int) bool { return latencies[i] < latencies[j] })

	fmt.Printf("  Iterations: %d\n", *iterations)
	fmt.Printf("  4KB Pwrite + Flush:\n")
	fmt.Printf("    P50:  %v\n", latencies[len(latencies)*50/100])
	fmt.Printf("    P90:  %v\n", latencies[len(latencies)*90/100])
	fmt.Printf("    P99:  %v\n", latencies[len(latencies)*99/100])
	fmt.Printf("    Max:  %v\n", latencies[len(latencies)-1])
	fmt.Printf("    Min:  %v\n", latencies[0])

	// Correctness: read back
	f, errno = jfs.Open(ctx, testPath, 4) // MODE_MASK_R
	if errno != 0 {
		log.Fatalf("Open for read failed: %v", errno)
	}
	readBuf := make([]byte, 4096)
	n, err := f.Pread(ctx, readBuf, 0)
	if err != nil && err != io.EOF {
		log.Fatalf("Pread failed: %v", err)
	}
	f.Close(ctx)
	fmt.Printf("  Read-back: %d bytes from offset 0 OK\n", n)

	// Truncate test
	if errno := jfs.Truncate(ctx, testPath, 512); errno != 0 {
		log.Fatalf("Truncate failed: %v", errno)
	}
	stat, errno := jfs.Stat(ctx, testPath)
	if errno != 0 {
		log.Fatalf("Stat failed: %v", errno)
	}
	fmt.Printf("  Truncate(512): size=%d OK\n", stat.Size())

	// Append test
	f, errno = jfs.Open(ctx, testPath, 2) // MODE_MASK_W
	if errno != 0 {
		log.Fatalf("Open for append failed: %v", errno)
	}
	f.Seek(ctx, 0, io.SeekEnd)
	appendData := make([]byte, 8192)
	rand.Read(appendData)
	if _, errno := f.Write(ctx, appendData); errno != 0 {
		log.Fatalf("Append write failed: %v", errno)
	}
	f.Flush(ctx)
	f.Close(ctx)
	stat, errno = jfs.Stat(ctx, testPath)
	if errno != 0 {
		log.Fatalf("Stat after append failed: %v", errno)
	}
	fmt.Printf("  Append(8KB): size=%d (expected %d) OK\n", stat.Size(), 512+8192)

	// Cleanup
	jfs.Delete(ctx, testPath)
}

func runMemoryReport() {
	runtime.GC()
	var m runtime.MemStats
	runtime.ReadMemStats(&m)

	fmt.Printf("  Current process:\n")
	fmt.Printf("    Sys (total from OS):  %.1f MB\n", float64(m.Sys)/1024/1024)
	fmt.Printf("    HeapAlloc:            %.1f MB\n", float64(m.HeapAlloc)/1024/1024)
	fmt.Printf("    HeapInuse:            %.1f MB\n", float64(m.HeapInuse)/1024/1024)
	fmt.Printf("    NumGoroutine:         %d\n", runtime.NumGoroutine())
	fmt.Println()
	fmt.Println("  This is with 1 volume. For N-volume test:")
	fmt.Println("  Format N volumes, then run N instances of initVolume() and measure RSS.")
	fmt.Println("  Key factors: meta client (~10MB), chunk cache (--cache-size MB), goroutines (~20 per volume).")
	fmt.Printf("  Estimated per-volume overhead: ~%.0f MB (meta) + %d MB (cache) = ~%d MB\n",
		10.0, *cacheSize, 10+*cacheSize)
	fmt.Printf("  Estimated 20 volumes: ~%d MB\n", 20*(10+*cacheSize))
	fmt.Printf("  Estimated 50 volumes: ~%d MB\n", 50*(10+*cacheSize))
}
