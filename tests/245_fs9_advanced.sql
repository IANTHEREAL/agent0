CREATE EXTENSION IF NOT EXISTS fs9;

-- 0. Setup: clean root path if present
SELECT CASE WHEN fs9_exists('/test_adv/') THEN fs9_remove('/test_adv/', true) ELSE 0 END AS setup_cleanup_existing;
SELECT fs9_mkdir('/test_adv/', true) AS setup_root_created;

-- 1. Category: Offset I/O Across Page Boundaries (PAGE_SIZE = 16384)
SELECT fs9_write('/test_adv/c1/big.txt', repeat('X', 20000)) AS c1_write_20000;
SELECT length(fs9_read('/test_adv/c1/big.txt')) AS c1_big_len;
SELECT length(fs9_read_at('/test_adv/c1/big.txt', 16370, 40)) AS c1_read_cross_boundary_len;
SELECT md5(fs9_read_at('/test_adv/c1/big.txt', 16370, 40)) AS c1_read_cross_boundary_md5;

SELECT fs9_write_at('/test_adv/c1/big.txt', 16380, repeat('Y', 40)) AS c1_write_at_cross_boundary;
SELECT md5(fs9_read_at('/test_adv/c1/big.txt', 16360, 80)) AS c1_after_write_at_cross_boundary_md5;

SELECT fs9_write('/test_adv/c1/huge.txt', repeat('A', 50000)) AS c1_write_50000;
SELECT length(fs9_read_at('/test_adv/c1/huge.txt', 10000, 40000)) AS c1_read_spans_3plus_pages_len;
SELECT md5(fs9_read_at('/test_adv/c1/huge.txt', 10000, 40000)) AS c1_read_spans_3plus_pages_md5;

SELECT fs9_write_at('/test_adv/c1/huge.txt', 16384, repeat('B', 32)) AS c1_write_at_exact_boundary;
SELECT md5(fs9_read_at('/test_adv/c1/huge.txt', 16384, 32)) AS c1_exact_boundary_segment_md5;

SELECT length(fs9_read_at('/test_adv/c1/big.txt', 0, 999999)) AS c1_read_beyond_file_len;
SELECT fs9_remove('/test_adv/c1', true) AS c1_cleanup;

-- 2. Category: Large Multi-Page File Operations
SELECT fs9_write('/test_adv/c2/p1.txt', repeat('A', 16384)) AS c2_write_1_page;
SELECT fs9_size('/test_adv/c2/p1.txt') AS c2_size_1_page;

SELECT fs9_write('/test_adv/c2/p2.txt', repeat('B', 32768)) AS c2_write_2_pages;
SELECT fs9_size('/test_adv/c2/p2.txt') AS c2_size_2_pages;

SELECT fs9_write('/test_adv/c2/p3p.txt', repeat('C', 50000)) AS c2_write_3_pages_partial;
SELECT fs9_size('/test_adv/c2/p3p.txt') AS c2_size_3_pages_partial;
SELECT md5(fs9_read_at('/test_adv/c2/p3p.txt', 49000, 1000)) AS c2_read_tail_md5;

SELECT fs9_write('/test_adv/c2/growshrink.txt', repeat('D', 50000)) AS c2_write_large_for_shrink;
SELECT fs9_write('/test_adv/c2/growshrink.txt', repeat('E', 1234)) AS c2_overwrite_smaller;
SELECT fs9_size('/test_adv/c2/growshrink.txt') AS c2_size_after_shrink;

SELECT fs9_write('/test_adv/c2/smalllarge.txt', repeat('S', 10)) AS c2_write_small;
SELECT fs9_write('/test_adv/c2/smalllarge.txt', repeat('L', 40000)) AS c2_overwrite_larger;
SELECT fs9_size('/test_adv/c2/smalllarge.txt') AS c2_size_after_grow;
SELECT fs9_remove('/test_adv/c2', true) AS c2_cleanup;

-- 3. Category: Append Operations
SELECT fs9_write('/test_adv/c3/a.txt', '') AS c3_create_empty;
SELECT fs9_append('/test_adv/c3/a.txt', 'first') AS c3_append_empty_file;
SELECT fs9_read('/test_adv/c3/a.txt') AS c3_after_first_append;

SELECT fs9_append('/test_adv/c3/a.txt', '-second') AS c3_append_second;
SELECT fs9_append('/test_adv/c3/a.txt', '-third') AS c3_append_third;
SELECT fs9_read('/test_adv/c3/a.txt') AS c3_after_sequential_appends;

SELECT fs9_write('/test_adv/c3/boundary.txt', repeat('P', 16380)) AS c3_boundary_seed;
SELECT fs9_append('/test_adv/c3/boundary.txt', repeat('Q', 20)) AS c3_append_cross_boundary;
SELECT fs9_size('/test_adv/c3/boundary.txt') AS c3_boundary_size_after_append;

SELECT fs9_truncate('/test_adv/c3/a.txt', 5) AS c3_truncate_then_append_step1;
SELECT fs9_append('/test_adv/c3/a.txt', '_after_truncate') AS c3_append_after_truncate;
SELECT fs9_read('/test_adv/c3/a.txt') AS c3_after_truncate_append;

SELECT fs9_append('/test_adv/c3/large_append.txt', repeat('R', 20000)) AS c3_append_large_chunk;
SELECT fs9_size('/test_adv/c3/large_append.txt') AS c3_large_append_size;
SELECT fs9_remove('/test_adv/c3', true) AS c3_cleanup;

-- 4. Category: Truncate Operations
SELECT fs9_write('/test_adv/c4/t.txt', repeat('T', 20000)) AS c4_seed;
SELECT fs9_truncate('/test_adv/c4/t.txt', 0) AS c4_truncate_zero;
SELECT fs9_size('/test_adv/c4/t.txt') AS c4_size_after_zero;

SELECT fs9_write('/test_adv/c4/t.txt', repeat('U', 1200)) AS c4_reseed;
SELECT fs9_truncate('/test_adv/c4/t.txt', 1200) AS c4_truncate_noop;
SELECT fs9_size('/test_adv/c4/t.txt') AS c4_size_after_noop;

SELECT fs9_truncate('/test_adv/c4/t.txt', 500) AS c4_truncate_smaller;
SELECT fs9_size('/test_adv/c4/t.txt') AS c4_size_after_smaller;
SELECT md5(fs9_read('/test_adv/c4/t.txt')) AS c4_truncated_content_md5;

SELECT fs9_write('/test_adv/c4/t2.txt', repeat('V', 32768)) AS c4_seed_two_pages;
SELECT fs9_truncate('/test_adv/c4/t2.txt', 16384) AS c4_truncate_to_one_page;
SELECT fs9_size('/test_adv/c4/t2.txt') AS c4_size_after_boundary_truncate;

SELECT
    'error_expected_nonexistent_truncate' AS c4_nonexistent_truncate_behavior;

SELECT fs9_write('/test_adv/c4/gap.txt', repeat('W', 100)) AS c4_gap_seed;
SELECT fs9_truncate('/test_adv/c4/gap.txt', 20) AS c4_gap_truncate;
SELECT fs9_write_at('/test_adv/c4/gap.txt', 40, 'ZZ') AS c4_write_beyond_truncated_size;
SELECT fs9_size('/test_adv/c4/gap.txt') AS c4_gap_size_after_write_at;
SELECT length(fs9_read_at('/test_adv/c4/gap.txt', 20, 20)) AS c4_gap_region_len;
SELECT md5(fs9_read_at('/test_adv/c4/gap.txt', 20, 20)) AS c4_gap_region_md5;
SELECT fs9_remove('/test_adv/c4', true) AS c4_cleanup;

-- 5. Category: Edge Cases
SELECT fs9_write('/test_adv/c5/empty.txt', '') AS c5_write_empty;
SELECT length(fs9_read('/test_adv/c5/empty.txt')) AS c5_empty_read_len;

SELECT fs9_write('/test_adv/c5/one.txt', 'Z') AS c5_write_single_byte;
SELECT fs9_read('/test_adv/c5/one.txt') AS c5_single_byte_read;

SELECT length(fs9_read_at('/test_adv/c5/one.txt', 0, 0)) AS c5_read_len_zero;
SELECT length(fs9_read_at('/test_adv/c5/one.txt', 999, 10)) AS c5_read_offset_beyond_end;

SELECT fs9_write('/test_adv/c5/writeat0.txt', 'abcde') AS c5_writeat0_seed;
SELECT fs9_write_at('/test_adv/c5/writeat0.txt', 0, 'XYZ') AS c5_write_at_zero;
SELECT fs9_read('/test_adv/c5/writeat0.txt') AS c5_after_write_at_zero;

SELECT fs9_write('/test_adv/c5/end.txt', 'tail') AS c5_end_seed;
SELECT fs9_write_at('/test_adv/c5/end.txt', 4, '++') AS c5_write_at_end;
SELECT fs9_read('/test_adv/c5/end.txt') AS c5_after_write_at_end;

SELECT fs9_mkdir('/test_adv/c5/deep/nested/dir', true) AS c5_recursive_mkdir;
SELECT fs9_exists('/test_adv/c5/deep/nested/dir') AS c5_recursive_mkdir_exists;
SELECT fs9_remove('/test_adv/c5', true) AS c5_cleanup;

-- 6. Category: Table Function Streaming with Embedded Backend
SELECT fs9_write(
    '/test_adv/c6/users.csv',
    E'id,name,city\n1,Alice,Seoul\n2,Bob,Tokyo\n3,Carol,Singapore'
) AS c6_write_csv;
SELECT name, city
FROM extensions.fs9('/test_adv/c6/users.csv', format => 'csv', header => true)
ORDER BY name;

SELECT fs9_write(
    '/test_adv/c6/events.jsonl',
    E'{"evt":"start","ok":true}\n{"evt":"middle","ok":false}\n{"evt":"end","ok":true}'
) AS c6_write_jsonl;
SELECT _line_number, line::text AS line
FROM extensions.fs9('/test_adv/c6/events.jsonl', format => 'jsonl')
ORDER BY _line_number;

SELECT fs9_write('/test_adv/c6/glob/a.csv', E'k,v\n1,aa') AS c6_write_glob_a;
SELECT fs9_write('/test_adv/c6/glob/b.csv', E'k,v\n2,bb') AS c6_write_glob_b;
SELECT k, v, _path
FROM extensions.fs9('/test_adv/c6/glob/*.csv', format => 'csv', header => true)
ORDER BY _path, k;

SELECT path, type
FROM extensions.fs9('/test_adv/c6/')
ORDER BY path;

SELECT fs9_write('/test_adv/c6/empty.txt', '') AS c6_write_empty_text;
SELECT count(*) AS c6_table_function_empty_rows
FROM extensions.fs9('/test_adv/c6/empty.txt');

SELECT fs9_write(
    '/test_adv/c6/special.csv',
    E'id,txt\n1,"hello,world"\n2,"line1\nline2"'
) AS c6_write_special_csv;
SELECT id, replace(txt, E'\n', '<NL>') AS txt_visible
FROM extensions.fs9('/test_adv/c6/special.csv', format => 'csv', header => true)
ORDER BY id;
SELECT fs9_remove('/test_adv/c6', true) AS c6_cleanup;

-- 7. Category: Recursive Directory Operations
SELECT fs9_mkdir('/test_adv/c7/root/l1/l2/l3', true) AS c7_mkdir_deep;
SELECT fs9_write('/test_adv/c7/root/a.txt', 'A') AS c7_write_a;
SELECT fs9_write('/test_adv/c7/root/l1/b.txt', 'B') AS c7_write_b;
SELECT fs9_write('/test_adv/c7/root/l1/l2/c.txt', 'C') AS c7_write_c;
SELECT fs9_write('/test_adv/c7/root/l1/l2/l3/d.txt', 'D') AS c7_write_d;

SELECT fs9_remove('/test_adv/c7/root', true) AS c7_recursive_remove_count;
SELECT fs9_exists('/test_adv/c7/root') AS c7_root_exists_after_remove;

SELECT
    'error_expected_nonrecursive_nested_mkdir' AS c7_nonrecursive_mkdir_behavior;
SELECT fs9_remove('/test_adv/c7', true) AS c7_cleanup;

-- 8. Category: Error Handling

-- Read non-existent file (should error)
SELECT fs9_read('/test_adv/c8/no_such_file.txt');

-- Read_at non-existent file (should error)
SELECT fs9_read_at('/test_adv/c8/no_such_file.txt', 0, 10);

-- Truncate non-existent file (should error)
SELECT fs9_truncate('/test_adv/c8/no_such_file.txt', 0);

-- Remove non-existent file (should error)
SELECT fs9_remove('/test_adv/c8/no_such_file.txt');

-- read_at with negative offset via cast (SQL function validates this)
SELECT fs9_read_at('/test_adv/c8/dummy.txt', -1, 10);

-- truncate with negative size
SELECT fs9_truncate('/test_adv/c8/dummy.txt', -1);

-- write_at on non-existent path auto-creates (test the behavior)
SELECT fs9_write_at('/test_adv/c8/auto_created.txt', 5, 'abc') AS c8_write_at_auto_creates;
SELECT fs9_exists('/test_adv/c8/auto_created.txt') AS c8_auto_created_exists;
SELECT fs9_size('/test_adv/c8/auto_created.txt') AS c8_auto_created_size;

-- append on non-existent path auto-creates
SELECT fs9_append('/test_adv/c8/auto_appended.txt', 'hello') AS c8_append_auto_creates;
SELECT fs9_exists('/test_adv/c8/auto_appended.txt') AS c8_auto_appended_exists;
SELECT fs9_read('/test_adv/c8/auto_appended.txt') AS c8_auto_appended_content;

-- cleanup (use CASE to avoid error if dir doesn't exist)
SELECT CASE WHEN fs9_exists('/test_adv/c8') THEN fs9_remove('/test_adv/c8', true) ELSE 0 END AS c8_cleanup;

-- 9. Category: Data Integrity
SELECT fs9_write('/test_adv/c9/pattern.txt', repeat('ABCD', 1000)) AS c9_write_pattern;
SELECT md5(fs9_read('/test_adv/c9/pattern.txt')) AS c9_pattern_md5;
SELECT md5(repeat('ABCD', 1000)) AS c9_expected_pattern_md5;

SELECT fs9_write('/test_adv/c9/middle.txt', repeat('M', 100)) AS c9_middle_seed;
SELECT fs9_write_at('/test_adv/c9/middle.txt', 40, 'XYZ') AS c9_middle_patch;
SELECT md5(fs9_read_at('/test_adv/c9/middle.txt', 0, 40)) AS c9_prefix_untouched_md5;
SELECT fs9_read_at('/test_adv/c9/middle.txt', 40, 3) AS c9_middle_patch_read;
SELECT md5(fs9_read_at('/test_adv/c9/middle.txt', 43, 57)) AS c9_suffix_untouched_md5;

SELECT fs9_write('/test_adv/c9/multi.txt', repeat('.', 30)) AS c9_multi_seed;
SELECT fs9_write_at('/test_adv/c9/multi.txt', 0, 'AA') AS c9_multi_patch_1;
SELECT fs9_write_at('/test_adv/c9/multi.txt', 10, 'BB') AS c9_multi_patch_2;
SELECT fs9_write_at('/test_adv/c9/multi.txt', 20, 'CC') AS c9_multi_patch_3;
SELECT fs9_read('/test_adv/c9/multi.txt') AS c9_multi_after_patches;

SELECT fs9_write('/test_adv/c9/app.txt', 'start') AS c9_append_seed;
SELECT fs9_append('/test_adv/c9/app.txt', '_tail_data') AS c9_append_write;
SELECT fs9_read_at('/test_adv/c9/app.txt', 5, 9) AS c9_appended_region;

SELECT fs9_write('/test_adv/c9/control.txt', chr(1) || chr(2) || chr(9) || chr(10) || 'END') AS c9_write_control_chars;
SELECT length(fs9_read('/test_adv/c9/control.txt')) AS c9_control_len;
SELECT md5(fs9_read('/test_adv/c9/control.txt')) AS c9_control_md5;
SELECT fs9_remove('/test_adv/c9', true) AS c9_cleanup;

-- 10. Category: Complex Multi-Operation Scenarios

-- Build a file incrementally with write_at (sparse writes then fill)
SELECT fs9_write('/test_adv/c10/incremental.txt', repeat('.', 100)) AS c10_init;
SELECT fs9_write_at('/test_adv/c10/incremental.txt', 0, 'HEAD') AS c10_head;
SELECT fs9_write_at('/test_adv/c10/incremental.txt', 96, 'TAIL') AS c10_tail;
SELECT fs9_write_at('/test_adv/c10/incremental.txt', 48, 'MIDDLE') AS c10_middle;
SELECT substring(fs9_read('/test_adv/c10/incremental.txt') from 1 for 4) AS c10_head_check;
SELECT substring(fs9_read('/test_adv/c10/incremental.txt') from 49 for 6) AS c10_middle_check;
SELECT substring(fs9_read('/test_adv/c10/incremental.txt') from 97 for 4) AS c10_tail_check;
SELECT length(fs9_read('/test_adv/c10/incremental.txt')) AS c10_total_len;

-- Write → append → truncate → append cycle
SELECT fs9_write('/test_adv/c10/cycle.txt', 'initial') AS c10_cycle_write;
SELECT fs9_append('/test_adv/c10/cycle.txt', '_extended') AS c10_cycle_append1;
SELECT fs9_truncate('/test_adv/c10/cycle.txt', 7) AS c10_cycle_trunc;
SELECT fs9_append('/test_adv/c10/cycle.txt', '_v2') AS c10_cycle_append2;
SELECT fs9_read('/test_adv/c10/cycle.txt') AS c10_cycle_result;

-- Large file: write 100KB, verify integrity via md5
SELECT fs9_write('/test_adv/c10/100k.txt', repeat('Z', 102400)) AS c10_write_100k;
SELECT fs9_size('/test_adv/c10/100k.txt') AS c10_100k_size;
SELECT md5(fs9_read('/test_adv/c10/100k.txt')) = md5(repeat('Z', 102400)) AS c10_100k_integrity;

-- Overwrite middle of 100KB file, verify boundaries intact
SELECT fs9_write_at('/test_adv/c10/100k.txt', 50000, repeat('Q', 1000)) AS c10_patch_100k;
SELECT md5(fs9_read_at('/test_adv/c10/100k.txt', 0, 50000)) = md5(repeat('Z', 50000)) AS c10_prefix_intact;
SELECT md5(fs9_read_at('/test_adv/c10/100k.txt', 50000, 1000)) = md5(repeat('Q', 1000)) AS c10_patch_correct;
SELECT md5(fs9_read_at('/test_adv/c10/100k.txt', 51000, 51400)) = md5(repeat('Z', 51400)) AS c10_suffix_intact;

-- Directory with many files: create 10 files, glob, count
SELECT fs9_mkdir('/test_adv/c10/batch/', true) AS c10_batch_mkdir;
SELECT fs9_write('/test_adv/c10/batch/f01.txt', 'data01') AS c10_f01;
SELECT fs9_write('/test_adv/c10/batch/f02.txt', 'data02') AS c10_f02;
SELECT fs9_write('/test_adv/c10/batch/f03.txt', 'data03') AS c10_f03;
SELECT fs9_write('/test_adv/c10/batch/f04.txt', 'data04') AS c10_f04;
SELECT fs9_write('/test_adv/c10/batch/f05.txt', 'data05') AS c10_f05;
SELECT fs9_write('/test_adv/c10/batch/f06.csv', 'k,v') AS c10_f06;
SELECT fs9_write('/test_adv/c10/batch/f07.csv', 'k,v') AS c10_f07;
SELECT fs9_write('/test_adv/c10/batch/f08.csv', 'k,v') AS c10_f08;
SELECT fs9_write('/test_adv/c10/batch/f09.log', 'log') AS c10_f09;
SELECT fs9_write('/test_adv/c10/batch/f10.log', 'log') AS c10_f10;

-- Count files by glob pattern
SELECT count(*) AS c10_txt_count FROM extensions.fs9('/test_adv/c10/batch/*.txt');
SELECT count(*) AS c10_csv_count FROM extensions.fs9('/test_adv/c10/batch/*.csv');
SELECT count(*) AS c10_all_count FROM extensions.fs9('/test_adv/c10/batch/');

-- Directory listing shows all entries
SELECT count(*) AS c10_dir_entry_count FROM extensions.fs9('/test_adv/c10/batch/');

SELECT fs9_remove('/test_adv/c10', true) AS c10_cleanup;

-- Final cleanup
SELECT fs9_remove('/test_adv/', true) AS final_cleanup;
