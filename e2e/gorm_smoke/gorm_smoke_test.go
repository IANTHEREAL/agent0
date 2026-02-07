package gorm_smoke

import (
	"context"
	"crypto/rand"
	"encoding/hex"
	"errors"
	"fmt"
	"os"
	"strings"
	"testing"
	"time"

	"gorm.io/driver/postgres"
	"gorm.io/gorm"
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
	// pg-tikv currently stores timestamps with millisecond precision. Accept small
	// (<1ms) truncation/rounding deltas on roundtrip.
	if delta := absDuration(byTime.HappenedAt.Sub(createdAt)); delta > time.Millisecond {
		t.Fatalf("time roundtrip: expected %v, got %v (delta %v)", createdAt, byTime.HappenedAt, delta)
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
		return dsn
	}
	if strings.Contains(dsn, "sslmode=") {
		return dsn
	}
	if strings.Contains(dsn, "?") {
		return dsn + "&sslmode=disable"
	}
	return dsn + "?sslmode=disable"
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

func absDuration(d time.Duration) time.Duration {
	if d < 0 {
		return -d
	}
	return d
}
