package gorm_smoke

import (
	"context"
	"crypto/rand"
	"encoding/hex"
	"errors"
	"fmt"
	"net/url"
	"os"
	"strings"
	"testing"
	"time"

	"gorm.io/driver/postgres"
	"gorm.io/gorm"
	"gorm.io/gorm/clause"
	"gorm.io/gorm/logger"
	"gorm.io/gorm/schema"
)

type Widget struct {
	ID         uint   `gorm:"primaryKey"`
	Name       string `gorm:"not null;uniqueIndex"`
	HappenedAt time.Time
	Metadata   map[string]any `gorm:"serializer:json"`
	CreatedAt  time.Time
	UpdatedAt  time.Time
}

const gormPushdownEnv = "DB9_GORM_SMOKE_ENABLE_COP_PUSHDOWN"

func TestGormSmoke(t *testing.T) {
	dsn := strings.TrimSpace(os.Getenv("PG_DSN"))
	if dsn == "" {
		t.Skip("PG_DSN not set; skipping e2e smoke test")
	}
	dsn = normalizeDSN(dsn)

	ctx, cancel := context.WithTimeout(context.Background(), 30*time.Second)
	t.Cleanup(cancel)

	schemaName := fmt.Sprintf("gorm_smoke_%s_%d", randomHex(t, 4), time.Now().Unix())
	db, cleanup := openDB(t, ctx, dsn, schemaName)
	t.Cleanup(cleanup)

	if err := db.WithContext(ctx).Exec("CREATE SCHEMA " + quoteIdent(schemaName)).Error; err != nil {
		t.Fatalf("create schema %q: %v", schemaName, err)
	}

	if err := db.WithContext(ctx).AutoMigrate(&Widget{}); err != nil {
		t.Fatalf("AutoMigrate: %v", err)
	}

	var tableCount int64
	if err := db.WithContext(ctx).
		Raw(
			`SELECT COUNT(*) FROM information_schema.tables WHERE table_schema = ? AND table_name = ?`,
			schemaName,
			"widgets",
		).
		Scan(&tableCount).Error; err != nil {
		t.Fatalf("verify table created in schema: %v", err)
	}
	if tableCount != 1 {
		t.Fatalf("expected widgets table in schema %q; got %d", schemaName, tableCount)
	}

	createdAt := time.Now().UTC().Truncate(time.Microsecond)
	widget := Widget{
		Name:       "widget_1",
		HappenedAt: createdAt,
		Metadata: map[string]any{
			"hello": "world",
			"n":     1,
		},
	}
	if err := db.WithContext(ctx).Create(&widget).Error; err != nil {
		t.Fatalf("create: %v", err)
	}
	if widget.ID == 0 {
		t.Fatalf("expected widget.ID to be set")
	}
	if widget.CreatedAt.IsZero() || widget.UpdatedAt.IsZero() {
		t.Fatalf("expected timestamps to be set (created_at=%v updated_at=%v)", widget.CreatedAt, widget.UpdatedAt)
	}

	var byID Widget
	if err := db.WithContext(ctx).First(&byID, widget.ID).Error; err != nil {
		t.Fatalf("query by pk: %v", err)
	}
	if byID.Name != widget.Name {
		t.Fatalf("query by pk: expected name %q, got %q", widget.Name, byID.Name)
	}
	if got := byID.Metadata["hello"]; got != "world" {
		t.Fatalf("metadata roundtrip: expected hello=world, got %#v", got)
	}

	var byUnique Widget
	if err := db.WithContext(ctx).Where("name = ?", widget.Name).First(&byUnique).Error; err != nil {
		t.Fatalf("query by unique: %v", err)
	}
	if byUnique.ID != widget.ID {
		t.Fatalf("query by unique: expected id=%d, got %d", widget.ID, byUnique.ID)
	}
	withPushdownProof(t, db, ctx, func(pushdownDB *gorm.DB) error {
		var proofUnique Widget
		if err := pushdownDB.Select("id", "name").Where("name = ?", widget.Name).Take(&proofUnique).Error; err != nil {
			return fmt.Errorf("pushdown query by unique: %w", err)
		}
		if proofUnique.ID != widget.ID {
			return fmt.Errorf("pushdown query by unique: expected id=%d, got %d", widget.ID, proofUnique.ID)
		}
		return explainContainsSubstrings(
			pushdownDB,
			ctx,
			fmt.Sprintf(
				"SELECT id, name FROM %s.widgets WHERE name = %s LIMIT 1",
				quoteIdent(schemaName),
				quoteLiteral(widget.Name),
			),
			[]string{"DB9 Cop"},
		)
	})

	if err := db.WithContext(ctx).Transaction(func(tx *gorm.DB) error {
		tmp := Widget{
			Name:       "widget_rollback",
			HappenedAt: createdAt.Add(2 * time.Second),
			Metadata: map[string]any{
				"rolled": true,
			},
		}
		if err := tx.Create(&tmp).Error; err != nil {
			return err
		}
		return errors.New("force rollback")
	}); err == nil {
		t.Fatalf("expected transaction to return error (rollback)")
	}

	var shouldNotExist Widget
	if err := db.WithContext(ctx).Where("name = ?", "widget_rollback").First(&shouldNotExist).Error; !errors.Is(err, gorm.ErrRecordNotFound) {
		t.Fatalf("rollback persistence: expected record not found, got %v", err)
	}

	var byTime Widget
	if err := db.WithContext(ctx).Where("happened_at = ?", createdAt).First(&byTime).Error; err != nil {
		t.Fatalf("query by time param: %v", err)
	}
	// db9-server currently stores timestamps with millisecond precision. Accept small
	// (<1ms) truncation/rounding deltas on roundtrip.
	if delta := absDuration(byTime.HappenedAt.Sub(createdAt)); delta > time.Millisecond {
		t.Fatalf("time roundtrip: expected %v, got %v (delta %v)", createdAt, byTime.HappenedAt, delta)
	}

	err := db.WithContext(ctx).Transaction(func(tx *gorm.DB) error {
		pending := Widget{
			Name:       "widget_txn_visible",
			HappenedAt: createdAt.Add(3 * time.Second),
			Metadata: map[string]any{
				"phase": "inserted",
			},
		}
		if err := tx.Create(&pending).Error; err != nil {
			return fmt.Errorf("txn create: %w", err)
		}

		var inserted Widget
		if err := tx.Where("name = ?", pending.Name).First(&inserted).Error; err != nil {
			return fmt.Errorf("txn read after insert: %w", err)
		}
		if inserted.ID != pending.ID {
			return fmt.Errorf("txn read after insert: expected id=%d got %d", pending.ID, inserted.ID)
		}

		if err := tx.Model(&Widget{}).Where("id = ?", pending.ID).Updates(map[string]any{
			"name": "widget_txn_updated",
		}).Error; err != nil {
			return fmt.Errorf("txn update: %w", err)
		}

		var updated Widget
		if err := tx.Where("id = ?", pending.ID).First(&updated).Error; err != nil {
			return fmt.Errorf("txn read after update: %w", err)
		}
		if updated.Name != "widget_txn_updated" {
			return fmt.Errorf("txn read after update: expected updated name, got %q", updated.Name)
		}

		if err := tx.Delete(&Widget{}, pending.ID).Error; err != nil {
			return fmt.Errorf("txn delete: %w", err)
		}

		var count int64
		if err := tx.Model(&Widget{}).Where("id = ?", pending.ID).Count(&count).Error; err != nil {
			return fmt.Errorf("txn read after delete: %w", err)
		}
		if count != 0 {
			return fmt.Errorf("txn read after delete: expected count=0 got %d", count)
		}

		return errors.New("force rollback after write visibility check")
	})
	if err == nil {
		t.Fatalf("expected write-visibility transaction to rollback")
	}

	var rolledBackCount int64
	if err := db.WithContext(ctx).Model(&Widget{}).Where("name IN ?", []string{"widget_txn_visible", "widget_txn_updated"}).Count(&rolledBackCount).Error; err != nil {
		t.Fatalf("rollback visibility cleanup: %v", err)
	}
	if rolledBackCount != 0 {
		t.Fatalf("expected rolled back txn rows to disappear, got count=%d", rolledBackCount)
	}

	extra := []Widget{
		{
			Name:       "widget_batch_1",
			HappenedAt: createdAt.Add(4 * time.Second),
			Metadata: map[string]any{
				"slot":  "batch_1",
				"phase": "inserted",
			},
		},
		{
			Name:       "widget_batch_2",
			HappenedAt: createdAt.Add(5 * time.Second),
			Metadata: map[string]any{
				"slot":  "batch_2",
				"phase": "inserted",
			},
		},
	}
	if err := db.WithContext(ctx).Create(&extra).Error; err != nil {
		t.Fatalf("batch create: %v", err)
	}

	upsertedAt := createdAt.Add(6 * time.Second)
	upsert := Widget{
		ID:         extra[0].ID,
		Name:       "widget_batch_1_upserted",
		HappenedAt: upsertedAt,
	}
	if err := db.WithContext(ctx).Clauses(clause.OnConflict{
		Columns: []clause.Column{{Name: "id"}},
		DoUpdates: clause.Assignments(map[string]any{
			"name":        upsert.Name,
			"happened_at": upsert.HappenedAt,
		}),
	}).Create(&upsert).Error; err != nil {
		t.Fatalf("upsert committed row: %v", err)
	}

	if err := db.WithContext(ctx).Model(&Widget{}).Where("id = ?", extra[1].ID).Updates(map[string]any{
		"name": "widget_batch_2_updated",
	}).Error; err != nil {
		t.Fatalf("update committed row: %v", err)
	}
	if err := db.WithContext(ctx).Delete(&Widget{}, extra[1].ID).Error; err != nil {
		t.Fatalf("delete committed row: %v", err)
	}

	var snapshot []Widget
	if err := db.WithContext(ctx).Order("id").Find(&snapshot).Error; err != nil {
		t.Fatalf("final snapshot query: %v", err)
	}
	if len(snapshot) != 2 {
		t.Fatalf("final snapshot: expected 2 rows, got %d (%#v)", len(snapshot), snapshot)
	}
	if snapshot[0].ID != widget.ID || snapshot[0].Name != "widget_1" {
		t.Fatalf("final snapshot row0 mismatch: %#v", snapshot[0])
	}
	if got := snapshot[0].Metadata["hello"]; got != "world" {
		t.Fatalf("final snapshot row0 metadata mismatch: %#v", snapshot[0].Metadata)
	}
	if snapshot[1].ID != extra[0].ID || snapshot[1].Name != "widget_batch_1_upserted" {
		t.Fatalf("final snapshot row1 mismatch: %#v", snapshot[1])
	}
	if delta := absDuration(snapshot[1].HappenedAt.Sub(upsertedAt)); delta > time.Millisecond {
		t.Fatalf("final snapshot row1 happened_at mismatch: expected %v got %v (delta %v)", upsertedAt, snapshot[1].HappenedAt, delta)
	}

	var finalCount int64
	if err := db.WithContext(ctx).Model(&Widget{}).Count(&finalCount).Error; err != nil {
		t.Fatalf("final count: %v", err)
	}
	if finalCount != 2 {
		t.Fatalf("final count: expected 2 got %d", finalCount)
	}
}

func TestGormPreparedAndVectorSmoke(t *testing.T) {
	dsn := strings.TrimSpace(os.Getenv("PG_DSN"))
	if dsn == "" {
		t.Skip("PG_DSN not set; skipping e2e smoke test")
	}
	dsn = normalizeDSN(dsn)

	ctx, cancel := context.WithTimeout(context.Background(), 30*time.Second)
	t.Cleanup(cancel)

	schemaName := fmt.Sprintf("gorm_adv_%s_%d", randomHex(t, 4), time.Now().Unix())
	db, cleanup := openDB(t, ctx, dsn, schemaName)
	t.Cleanup(cleanup)

	if err := db.WithContext(ctx).Exec("CREATE SCHEMA " + quoteIdent(schemaName)).Error; err != nil {
		t.Fatalf("create schema %q: %v", schemaName, err)
	}

	// prepared statement / positional bind / repeated execute
	var sum1, sum2 int
	if err := db.WithContext(ctx).Raw("SELECT ?::int + ?::int", 1, 2).Scan(&sum1).Error; err != nil {
		t.Fatalf("prepared statement bind #1: %v", err)
	}
	if err := db.WithContext(ctx).Raw("SELECT ?::int + ?::int", 3, 4).Scan(&sum2).Error; err != nil {
		t.Fatalf("prepared statement bind #2: %v", err)
	}
	if sum1 != 3 || sum2 != 7 {
		t.Fatalf("unexpected prepared statement results: got (%d, %d)", sum1, sum2)
	}

	// vector column / vector index / vector insert / vector distance / vector filter
	createVectorTable := fmt.Sprintf(`
		CREATE TABLE %s.vectors (
			id INTEGER PRIMARY KEY,
			embedding vector(3) NOT NULL
		)`, quoteIdent(schemaName))
	if err := db.WithContext(ctx).Exec(createVectorTable).Error; err != nil {
		t.Fatalf("create vector table: %v", err)
	}

	createVectorIndex := fmt.Sprintf(
		"CREATE INDEX idx_vectors_embedding ON %s.vectors USING hnsw (embedding)",
		quoteIdent(schemaName),
	)
	if err := db.WithContext(ctx).Exec(createVectorIndex).Error; err != nil {
		t.Fatalf("create vector index: %v", err)
	}

	insertVectors := fmt.Sprintf(
		"INSERT INTO %s.vectors (id, embedding) VALUES (1, '[1,0,0]'), (2, '[0,1,0]')",
		quoteIdent(schemaName),
	)
	if err := db.WithContext(ctx).Exec(insertVectors).Error; err != nil {
		t.Fatalf("insert vectors: %v", err)
	}

	var ids []int
	queryVector := fmt.Sprintf(
		"SELECT id FROM %s.vectors WHERE embedding <-> '[1,0,0]' < 1.0 ORDER BY embedding <-> '[1,0,0]'",
		quoteIdent(schemaName),
	)
	if err := db.WithContext(ctx).Raw(queryVector).Scan(&ids).Error; err != nil {
		t.Fatalf("query vector distance/filter: %v", err)
	}
	if len(ids) == 0 || ids[0] != 1 {
		t.Fatalf("unexpected vector query result: %#v", ids)
	}
}

func openDB(t *testing.T, ctx context.Context, dsn, schemaName string) (*gorm.DB, func()) {
	t.Helper()

	db, err := gorm.Open(
		postgres.New(postgres.Config{
			DSN: dsn,
		}),
		&gorm.Config{
			NamingStrategy: schema.NamingStrategy{
				TablePrefix: schemaName + ".",
			},
			Logger: logger.Default.LogMode(logger.Silent),
		},
	)
	if err != nil {
		t.Fatalf("open db: %v", err)
	}

	sqlDB, err := db.DB()
	if err != nil {
		t.Fatalf("db.DB(): %v", err)
	}

	return db, func() {
		_ = db.WithContext(ctx).Exec("DROP SCHEMA IF EXISTS " + quoteIdent(schemaName) + " CASCADE").Error
		_ = sqlDB.Close()
	}
}

func normalizeDSN(dsn string) string {
	if !(strings.HasPrefix(dsn, "postgres://") || strings.HasPrefix(dsn, "postgresql://")) {
		return strings.TrimSpace(dsn)
	}

	parsed, err := url.Parse(dsn)
	if err != nil {
		return dsn
	}

	query := parsed.Query()
	if query.Get("sslmode") == "" {
		query.Set("sslmode", "disable")
	}
	parsed.RawQuery = query.Encode()
	return parsed.String()
}

func gormPushdownEnabled() bool {
	switch strings.ToLower(strings.TrimSpace(os.Getenv(gormPushdownEnv))) {
	case "1", "true", "t", "yes", "y", "on":
		return true
	default:
		return false
	}
}

func withPushdownProof(
	t *testing.T,
	db *gorm.DB,
	ctx context.Context,
	fn func(pushdownDB *gorm.DB) error,
) {
	t.Helper()
	if !gormPushdownEnabled() {
		return
	}

	if err := db.WithContext(ctx).Transaction(func(tx *gorm.DB) error {
		if err := tx.Exec("SET LOCAL db9.enable_cop_pushdown = on").Error; err != nil {
			return fmt.Errorf("enable db9 cop pushdown for proof query: %w", err)
		}
		return fn(tx.WithContext(ctx))
	}); err != nil {
		t.Fatalf("pushdown proof transaction: %v", err)
	}
}

func explainContainsSubstrings(
	db *gorm.DB,
	ctx context.Context,
	query string,
	expected []string,
	args ...any,
) error {
	if !gormPushdownEnabled() {
		return nil
	}

	lines, err := explainLines(db, ctx, query, args...)
	if err != nil {
		return err
	}

	for _, needle := range expected {
		found := false
		for _, line := range lines {
			if strings.Contains(line, needle) {
				found = true
				break
			}
		}
		if !found {
			return fmt.Errorf(
				"expected %q in EXPLAIN VERBOSE for %q, got %#v",
				needle,
				query,
				lines,
			)
		}
	}
	return nil
}

func explainDoesNotContainSubstrings(
	db *gorm.DB,
	ctx context.Context,
	query string,
	unexpected []string,
	args ...any,
) error {
	if !gormPushdownEnabled() {
		return nil
	}

	lines, err := explainLines(db, ctx, query, args...)
	if err != nil {
		return err
	}

	for _, needle := range unexpected {
		for _, line := range lines {
			if strings.Contains(line, needle) {
				return fmt.Errorf(
					"did not expect %q in EXPLAIN VERBOSE for %q, got %#v",
					needle,
					query,
					lines,
				)
			}
		}
	}
	return nil
}

func explainLines(db *gorm.DB, ctx context.Context, query string, args ...any) ([]string, error) {
	rows, err := db.WithContext(ctx).Raw("EXPLAIN VERBOSE "+query, args...).Rows()
	if err != nil {
		return nil, fmt.Errorf("explain pushdown candidate %q: %w", query, err)
	}
	defer rows.Close()

	var lines []string
	for rows.Next() {
		var line string
		if err := rows.Scan(&line); err != nil {
			return nil, fmt.Errorf("scan EXPLAIN row for %q: %w", query, err)
		}
		lines = append(lines, line)
	}
	if err := rows.Err(); err != nil {
		return nil, fmt.Errorf("iterate EXPLAIN rows for %q: %w", query, err)
	}
	return lines, nil
}

func assertExplainContainsSubstrings(
	t *testing.T,
	db *gorm.DB,
	ctx context.Context,
	query string,
	expected []string,
	args ...any,
) {
	t.Helper()
	if err := explainContainsSubstrings(db, ctx, query, expected, args...); err != nil {
		t.Fatal(err)
	}
}

func assertExplainContainsDB9Cop(t *testing.T, db *gorm.DB, ctx context.Context, query string, args ...any) {
	t.Helper()
	assertExplainContainsSubstrings(t, db, ctx, query, []string{"DB9 Cop"}, args...)
}

func randomHex(t *testing.T, byteLen int) string {
	t.Helper()

	b := make([]byte, byteLen)
	if _, err := rand.Read(b); err != nil {
		t.Fatalf("rand.Read: %v", err)
	}
	return hex.EncodeToString(b)
}

func quoteIdent(ident string) string {
	return `"` + strings.ReplaceAll(ident, `"`, `""`) + `"`
}

func quoteLiteral(value string) string {
	return "'" + strings.ReplaceAll(value, "'", "''") + "'"
}

func absDuration(d time.Duration) time.Duration {
	if d < 0 {
		return -d
	}
	return d
}
