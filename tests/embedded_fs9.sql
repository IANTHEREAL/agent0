-- Embedded fs9 filesystem tests (TiKV backend)
-- Tests fs9_write, fs9_read, fs9_exists, fs9_size, fs9_mtime, fs9_remove, fs9_mkdir
-- when running against TiKV with embedded PageFS backend.

-- 1. Basic write + read cycle
SELECT fs9_write('/test_embedded/hello.txt', 'Hello, embedded world!');
SELECT fs9_read('/test_embedded/hello.txt');

-- 2. File existence check
SELECT fs9_exists('/test_embedded/hello.txt');
SELECT fs9_exists('/test_embedded/nonexistent.txt');

-- 3. File size
SELECT fs9_size('/test_embedded/hello.txt');

-- 4. File modification time (non-deterministic, just check not null)
SELECT fs9_mtime('/test_embedded/hello.txt') IS NOT NULL AS has_mtime;

-- 5. Overwrite existing file
SELECT fs9_write('/test_embedded/hello.txt', 'Updated content');
SELECT fs9_read('/test_embedded/hello.txt');
SELECT fs9_size('/test_embedded/hello.txt');

-- 6. Create directory
SELECT fs9_mkdir('/test_embedded/subdir');
SELECT fs9_exists('/test_embedded/subdir');

-- 7. Write into subdirectory
SELECT fs9_write('/test_embedded/subdir/nested.txt', 'Nested file content');
SELECT fs9_read('/test_embedded/subdir/nested.txt');

-- 8. Remove file
SELECT fs9_remove('/test_embedded/hello.txt');
SELECT fs9_exists('/test_embedded/hello.txt');

-- 9. Remove nested file then directory
SELECT fs9_remove('/test_embedded/subdir/nested.txt');
SELECT fs9_remove('/test_embedded/subdir');

-- 10. Cleanup root test directory
SELECT fs9_remove('/test_embedded');

-- 11. Null handling
SELECT fs9_read(NULL) AS null_read;
SELECT fs9_write(NULL, 'data') AS null_write_path;
SELECT fs9_write('/test_embedded_null', NULL) AS null_write_content;
