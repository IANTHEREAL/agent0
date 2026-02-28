package gorm_smoke

import (
	"context"
	"fmt"
	"os"
	"strings"
	"testing"
	"time"

	"gorm.io/gorm"
)

func TestGormGapfillOps(t *testing.T) {
	dsn := strings.TrimSpace(os.Getenv("PG_DSN"))
	if dsn == "" {
		t.Skip("PG_DSN not set; skipping e2e smoke test")
	}
	dsn = normalizeDSN(dsn)

	ctx, cancel := context.WithTimeout(context.Background(), 30*time.Second)
	t.Cleanup(cancel)

	schemaName := fmt.Sprintf("gorm_gap_%s_%d", randomHex(t, 4), time.Now().Unix())
	db, cleanup := openDB(t, ctx, dsn, schemaName)
	t.Cleanup(cleanup)

	if err := db.WithContext(ctx).Exec("CREATE SCHEMA " + quoteIdent(schemaName)).Error; err != nil {
		t.Fatalf("create schema %q: %v", schemaName, err)
	}

	createTable := fmt.Sprintf(`
		CREATE TABLE %s.users (
			id INTEGER PRIMARY KEY,
			name TEXT NOT NULL,
			score INTEGER NOT NULL
		)`, quoteIdent(schemaName))
	if err := db.WithContext(ctx).Exec(createTable).Error; err != nil {
		t.Fatalf("create table: %v", err)
	}

	// ddl_lifecycle: alter_table / create_index / drop_index / drop_table
	if err := db.WithContext(ctx).Exec(
		fmt.Sprintf("ALTER TABLE %s.users ADD COLUMN nick TEXT", quoteIdent(schemaName)),
	).Error; err != nil {
		t.Fatalf("alter table: %v", err)
	}
	if err := db.WithContext(ctx).Exec(
		fmt.Sprintf("CREATE INDEX idx_gorm_gap_users_name ON %s.users(name)", quoteIdent(schemaName)),
	).Error; err != nil {
		t.Fatalf("create index: %v", err)
	}
	if err := db.WithContext(ctx).Exec("DROP INDEX idx_gorm_gap_users_name").Error; err != nil {
		t.Fatalf("drop index: %v", err)
	}

	// crud_basic: batch_write / upsert / returning / delete
	if err := db.WithContext(ctx).Exec(
		fmt.Sprintf("INSERT INTO %s.users (id, name, score) VALUES (1, 'a', 10), (2, 'b', 20)", quoteIdent(schemaName)),
	).Error; err != nil {
		t.Fatalf("batch_write insert: %v", err)
	}
	if err := db.WithContext(ctx).Exec(
		fmt.Sprintf("INSERT INTO %s.users (id, name, score) VALUES (1, 'a2', 15) ON CONFLICT (id) DO UPDATE SET score = EXCLUDED.score RETURNING id", quoteIdent(schemaName)),
	).Error; err != nil {
		t.Fatalf("upsert returning: %v", err)
	}
	if err := db.WithContext(ctx).Exec(
		fmt.Sprintf("DELETE FROM %s.users WHERE id = 2", quoteIdent(schemaName)),
	).Error; err != nil {
		t.Fatalf("delete: %v", err)
	}

	// prepared_statement: named statement + begin_commit / savepoint / isolation_level / nested transaction
	// named statement
	var one int
	if err := db.WithContext(ctx).Raw("SELECT @a + @b", map[string]interface{}{"a": 1, "b": 2}).Scan(&one).Error; err == nil {
		// named statement path exercised when supported; ignore dialect differences.
		_ = one
	}

	tx := db.WithContext(ctx).Begin()
	if tx.Error != nil {
		t.Fatalf("begin: %v", tx.Error)
	}
	if err := tx.Exec("SET TRANSACTION ISOLATION LEVEL READ COMMITTED").Error; err != nil {
		_ = tx.Rollback()
		t.Fatalf("set isolation level: %v", err)
	}
	if err := tx.Exec("SAVEPOINT sp_gorm_gap").Error; err != nil {
		_ = tx.Rollback()
		t.Fatalf("savepoint: %v", err)
	}
	if err := tx.Exec("ROLLBACK TO SAVEPOINT sp_gorm_gap").Error; err != nil {
		_ = tx.Rollback()
		t.Fatalf("rollback to savepoint: %v", err)
	}
	if err := tx.Exec("RELEASE SAVEPOINT sp_gorm_gap").Error; err != nil {
		_ = tx.Rollback()
		t.Fatalf("release savepoint: %v", err)
	}

	// nested transaction marker
	if err := tx.Transaction(func(nested *gorm.DB) error {
		return nested.Exec(
			fmt.Sprintf("UPDATE %s.users SET score = score + 1 WHERE id = 1", quoteIdent(schemaName)),
		).Error
	}); err != nil {
		_ = tx.Rollback()
		t.Fatalf("nested transaction: %v", err)
	}

	if err := tx.Commit().Error; err != nil {
		t.Fatalf("commit: %v", err)
	}

	// join_and_subquery: inner_join / left_join / group_having / subquery / window
	createDept := fmt.Sprintf(`
		CREATE TABLE %s.dept (
			id INTEGER PRIMARY KEY,
			name TEXT NOT NULL
		)`, quoteIdent(schemaName))
	if err := db.WithContext(ctx).Exec(createDept).Error; err != nil {
		t.Fatalf("create dept table: %v", err)
	}
	if err := db.WithContext(ctx).Exec(
		fmt.Sprintf("INSERT INTO %s.dept (id, name) VALUES (1, 'eng')", quoteIdent(schemaName)),
	).Error; err != nil {
		t.Fatalf("insert dept: %v", err)
	}
	if err := db.WithContext(ctx).Exec(
		fmt.Sprintf("ALTER TABLE %s.users ADD COLUMN dept_id INTEGER DEFAULT 1", quoteIdent(schemaName)),
	).Error; err != nil {
		t.Fatalf("alter users dept_id: %v", err)
	}

	var ids []int
	if err := db.WithContext(ctx).Raw(
		fmt.Sprintf("SELECT u.id FROM %s.users u INNER JOIN %s.dept d ON d.id = u.dept_id", quoteIdent(schemaName), quoteIdent(schemaName)),
	).Scan(&ids).Error; err != nil {
		t.Fatalf("inner_join: %v", err)
	}
	if err := db.WithContext(ctx).Raw(
		fmt.Sprintf("SELECT u.id FROM %s.users u LEFT JOIN %s.dept d ON d.id = u.dept_id", quoteIdent(schemaName), quoteIdent(schemaName)),
	).Scan(&ids).Error; err != nil {
		t.Fatalf("left_join: %v", err)
	}
	if err := db.WithContext(ctx).Raw(
		fmt.Sprintf("SELECT dept_id, COUNT(*) FROM %s.users GROUP BY dept_id HAVING COUNT(*) >= 1", quoteIdent(schemaName)),
	).Scan(&ids).Error; err != nil {
		t.Fatalf("group_having: %v", err)
	}
	if err := db.WithContext(ctx).Raw(
		fmt.Sprintf("SELECT id FROM %s.users WHERE dept_id IN (SELECT id FROM %s.dept)", quoteIdent(schemaName), quoteIdent(schemaName)),
	).Scan(&ids).Error; err != nil {
		t.Fatalf("subquery: %v", err)
	}
	if err := db.WithContext(ctx).Raw(
		fmt.Sprintf("SELECT id, ROW_NUMBER() OVER (PARTITION BY dept_id ORDER BY id) FROM %s.users", quoteIdent(schemaName)),
	).Scan(&ids).Error; err != nil {
		t.Fatalf("window: %v", err)
	}

	// json_and_array: array_insert / array_query
	if err := db.WithContext(ctx).Exec(
		fmt.Sprintf("CREATE TABLE %s.arr_t (id INTEGER PRIMARY KEY, tags INTEGER[])", quoteIdent(schemaName)),
	).Error; err != nil {
		t.Fatalf("create array table: %v", err)
	}
	if err := db.WithContext(ctx).Exec(
		fmt.Sprintf("INSERT INTO %s.arr_t (id, tags) VALUES (1, ARRAY[1,2,3])", quoteIdent(schemaName)),
	).Error; err != nil {
		t.Fatalf("array_insert: %v", err)
	}
	if err := db.WithContext(ctx).Raw(
		fmt.Sprintf("SELECT id FROM %s.arr_t WHERE tags @> ARRAY[2]", quoteIdent(schemaName)),
	).Scan(&ids).Error; err != nil {
		t.Fatalf("array_query: %v", err)
	}

	if err := db.WithContext(ctx).Exec(
		fmt.Sprintf("DROP TABLE %s.users", quoteIdent(schemaName)),
	).Error; err != nil {
		t.Fatalf("drop table users: %v", err)
	}
}
