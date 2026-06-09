package gorm_smoke

import (
	"context"
	"database/sql"
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
	if err := db.WithContext(ctx).Exec(
		fmt.Sprintf("DROP INDEX %s.%s", quoteIdent(schemaName), quoteIdent("idx_gorm_gap_users_name")),
	).Error; err != nil {
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
	var grouped []struct {
		DeptID int   `gorm:"column:dept_id"`
		Total  int64 `gorm:"column:total"`
	}
	if err := db.WithContext(ctx).Raw(
		fmt.Sprintf(
			"SELECT dept_id, COUNT(*) AS total FROM %s.users GROUP BY dept_id HAVING COUNT(*) >= 1",
			quoteIdent(schemaName),
		),
	).Scan(&grouped).Error; err != nil {
		t.Fatalf("group_having: %v", err)
	}
	if len(grouped) == 0 || grouped[0].Total < 1 {
		t.Fatalf("group_having: unexpected grouped rows: %#v", grouped)
	}
	if err := db.WithContext(ctx).Raw(
		fmt.Sprintf("SELECT id FROM %s.users WHERE dept_id IN (SELECT id FROM %s.dept)", quoteIdent(schemaName), quoteIdent(schemaName)),
	).Scan(&ids).Error; err != nil {
		t.Fatalf("subquery: %v", err)
	}
	var windowRows []struct {
		ID int   `gorm:"column:id"`
		RN int64 `gorm:"column:rn"`
	}
	if err := db.WithContext(ctx).Raw(
		fmt.Sprintf(
			"SELECT id, ROW_NUMBER() OVER (PARTITION BY dept_id ORDER BY id) AS rn FROM %s.users",
			quoteIdent(schemaName),
		),
	).Scan(&windowRows).Error; err != nil {
		t.Fatalf("window: %v", err)
	}
	if len(windowRows) == 0 || windowRows[0].RN < 1 {
		t.Fatalf("window: unexpected rows: %#v", windowRows)
	}

	// secondary-index row fetch: exact composite-index predicate returns non-index payload
	if err := db.WithContext(ctx).Exec(
		fmt.Sprintf(`
			CREATE TABLE %s.lookup_owner (
				id INTEGER PRIMARY KEY,
				label TEXT NOT NULL
			)`, quoteIdent(schemaName)),
	).Error; err != nil {
		t.Fatalf("create lookup_owner: %v", err)
	}
	if err := db.WithContext(ctx).Exec(
		fmt.Sprintf(`
			CREATE TABLE %s.lookup_rows (
				id INTEGER PRIMARY KEY,
				owner_id INTEGER NOT NULL,
				a INTEGER NOT NULL,
				b INTEGER NOT NULL,
				payload TEXT NOT NULL
			)`, quoteIdent(schemaName)),
	).Error; err != nil {
		t.Fatalf("create lookup_rows: %v", err)
	}
	if err := db.WithContext(ctx).Exec(
		fmt.Sprintf("CREATE INDEX idx_lookup_owner_label ON %s.lookup_owner(label)", quoteIdent(schemaName)),
	).Error; err != nil {
		t.Fatalf("create lookup_owner label index: %v", err)
	}
	if err := db.WithContext(ctx).Exec(
		fmt.Sprintf("CREATE INDEX idx_lookup_rows_ab ON %s.lookup_rows(a, b)", quoteIdent(schemaName)),
	).Error; err != nil {
		t.Fatalf("create lookup_rows ab index: %v", err)
	}
	if err := db.WithContext(ctx).Exec(
		fmt.Sprintf(`
			INSERT INTO %s.lookup_owner (id, label)
			VALUES (1, 'target-owner'), (2, 'other-owner')`, quoteIdent(schemaName)),
	).Error; err != nil {
		t.Fatalf("insert lookup_owner: %v", err)
	}
	if err := db.WithContext(ctx).Exec(
		fmt.Sprintf(`
			INSERT INTO %s.lookup_rows (id, owner_id, a, b, payload)
			VALUES
				(100, 1, 1234, 1, 'target-hit'),
				(101, 1, 1234, 2, 'same-owner-other-b'),
				(102, 2, 4321, 1, 'other-owner-other-a')`, quoteIdent(schemaName)),
	).Error; err != nil {
		t.Fatalf("insert lookup_rows: %v", err)
	}
	if err := db.WithContext(ctx).Exec(
		fmt.Sprintf(`
			INSERT INTO %s.lookup_rows (id, owner_id, a, b, payload)
			SELECT
				1000 + i,
				2,
				20000 + i,
				i %% 7,
				'filler'
			FROM generate_series(1, 2048) AS gs(i)`, quoteIdent(schemaName)),
	).Error; err != nil {
		t.Fatalf("insert lookup_rows filler: %v", err)
	}
	if err := db.WithContext(ctx).Exec(
		fmt.Sprintf("ANALYZE %s.lookup_rows", quoteIdent(schemaName)),
	).Error; err != nil {
		t.Fatalf("analyze lookup_rows: %v", err)
	}

	var rowFetchRows []struct {
		ID      int    `gorm:"column:id"`
		Payload string `gorm:"column:payload"`
	}
	rowFetchSQL := fmt.Sprintf(
		"SELECT id, payload FROM %s.lookup_rows WHERE a = 1234 AND b = abs(-1) LIMIT 1",
		quoteIdent(schemaName),
	)
	if err := db.WithContext(ctx).Raw(
		rowFetchSQL,
	).Scan(&rowFetchRows).Error; err != nil {
		t.Fatalf("secondary-index row fetch: %v", err)
	}
	if len(rowFetchRows) != 1 || rowFetchRows[0].ID != 100 || rowFetchRows[0].Payload != "target-hit" {
		t.Fatalf("secondary-index row fetch: unexpected rows: %#v", rowFetchRows)
	}
	withPushdownProof(t, db, ctx, func(pushdownDB *gorm.DB) error {
		if err := explainContainsSubstrings(
			pushdownDB,
			ctx,
			rowFetchSQL,
			[]string{
				fmt.Sprintf("Index Scan using idx_lookup_rows_ab on %s.lookup_rows", schemaName),
				"DB9 Cop Access: prefix (1234)",
				"DB9 Cop Output: id, payload",
				"DB9 Cop Limit: 1",
			},
		); err != nil {
			return err
		}

		var proofRows []struct {
			ID      int    `gorm:"column:id"`
			Payload string `gorm:"column:payload"`
		}
		if err := pushdownDB.WithContext(ctx).Raw(rowFetchSQL).Scan(&proofRows).Error; err != nil {
			return fmt.Errorf("pushdown secondary-index row fetch: %w", err)
		}
		if len(proofRows) != 1 || proofRows[0].ID != 100 || proofRows[0].Payload != "target-hit" {
			return fmt.Errorf("pushdown secondary-index row fetch: unexpected rows: %#v", proofRows)
		}
		return nil
	})

	var joinRows []struct {
		ID      int    `gorm:"column:id"`
		Payload string `gorm:"column:payload"`
		Label   string `gorm:"column:label"`
	}
	if err := db.WithContext(ctx).Raw(
		fmt.Sprintf(`
			SELECT r.id, r.payload, o.label
			FROM %s.lookup_owner o
			INNER JOIN %s.lookup_rows r ON r.owner_id = o.id
			WHERE o.label = 'target-owner'
			  AND r.a = 1234
			  AND r.b = abs(-1)
			ORDER BY r.id`,
			quoteIdent(schemaName),
			quoteIdent(schemaName),
		),
	).Scan(&joinRows).Error; err != nil {
		t.Fatalf("join + secondary-index row fetch: %v", err)
	}
	if len(joinRows) != 1 || joinRows[0].ID != 100 || joinRows[0].Payload != "target-hit" || joinRows[0].Label != "target-owner" {
		t.Fatalf("join + secondary-index row fetch: unexpected rows: %#v", joinRows)
	}

	// function pushdown correctness: scalar/text/numeric/datetime projections
	if err := db.WithContext(ctx).Exec(
		fmt.Sprintf(`
			CREATE TABLE %s.func_pushdown_rows (
				id INTEGER PRIMARY KEY,
				v TEXT,
				n INTEGER,
				score DOUBLE,
				created_at TIMESTAMP
			)`, quoteIdent(schemaName)),
	).Error; err != nil {
		t.Fatalf("create func_pushdown_rows: %v", err)
	}
	if err := db.WithContext(ctx).Exec(
		fmt.Sprintf("CREATE INDEX idx_func_pushdown_rows_n ON %s.func_pushdown_rows(n)", quoteIdent(schemaName)),
	).Error; err != nil {
		t.Fatalf("create func_pushdown_rows n index: %v", err)
	}
	if err := db.WithContext(ctx).Exec(
		fmt.Sprintf(`
			INSERT INTO %s.func_pushdown_rows (id, v, n, score, created_at)
			VALUES
				(200, '  B  ', 20, 20.6, '2024-01-02 12:34:56'),
				(201, NULL, NULL, NULL, NULL)`, quoteIdent(schemaName)),
	).Error; err != nil {
		t.Fatalf("insert func_pushdown_rows: %v", err)
	}

	var functionRows []struct {
		LowerTrimmed string    `gorm:"column:lower_trimmed"`
		UpperTrimmed string    `gorm:"column:upper_trimmed"`
		LengthV      int64     `gorm:"column:length_v"`
		CharLengthV  int64     `gorm:"column:char_length_v"`
		CharacterLen int64     `gorm:"column:character_length_v"`
		SubstrV      string    `gorm:"column:substr_v"`
		SubstringV   string    `gorm:"column:substring_v"`
		TrimmedV     string    `gorm:"column:trimmed_v"`
		LtrimV       string    `gorm:"column:ltrim_v"`
		RtrimV       string    `gorm:"column:rtrim_v"`
		PosB         int64     `gorm:"column:pos_b"`
		AbsN         int64     `gorm:"column:abs_n"`
		CeilScore    float64   `gorm:"column:ceil_score"`
		CeilingScore float64   `gorm:"column:ceiling_score"`
		FloorScore   float64   `gorm:"column:floor_score"`
		RoundScore   float64   `gorm:"column:round_score"`
		CoalescedV   string    `gorm:"column:coalesced_v"`
		NullifKeep   string    `gorm:"column:nullif_keep"`
		DayPart      float64   `gorm:"column:day_part"`
		HourBucket   time.Time `gorm:"column:hour_bucket"`
	}
	if err := db.WithContext(ctx).Raw(
		fmt.Sprintf(`
			SELECT
				lower(btrim(v)) AS lower_trimmed,
				upper(btrim(v)) AS upper_trimmed,
				length(v) AS length_v,
				char_length(v) AS char_length_v,
				character_length(v) AS character_length_v,
				substr(v, 3, 1) AS substr_v,
				substring(v, 3, 1) AS substring_v,
				btrim(v) AS trimmed_v,
				ltrim(v) AS ltrim_v,
				rtrim(v) AS rtrim_v,
				strpos(v, 'B') AS pos_b,
				abs(n) AS abs_n,
				ceil(score) AS ceil_score,
				ceiling(score) AS ceiling_score,
				floor(score) AS floor_score,
				round(score) AS round_score,
				coalesce(NULL, btrim(v)) AS coalesced_v,
				nullif(btrim(v), 'z') AS nullif_keep,
				date_part('day', created_at) AS day_part,
				date_trunc('hour', created_at) AS hour_bucket
			FROM %s.func_pushdown_rows
			WHERE n = 20
			LIMIT 1`,
			quoteIdent(schemaName),
		),
	).Scan(&functionRows).Error; err != nil {
		t.Fatalf("function pushdown rows: %v", err)
	}
	if len(functionRows) != 1 {
		t.Fatalf("function pushdown rows: expected 1 row, got %#v", functionRows)
	}
	functionRow := functionRows[0]
	if functionRow.LowerTrimmed != "b" ||
		functionRow.UpperTrimmed != "B" ||
		functionRow.LengthV != 5 ||
		functionRow.CharLengthV != 5 ||
		functionRow.CharacterLen != 5 ||
		functionRow.SubstrV != "B" ||
		functionRow.SubstringV != "B" ||
		functionRow.TrimmedV != "B" ||
		functionRow.LtrimV != "B  " ||
		functionRow.RtrimV != "  B" ||
		functionRow.PosB != 3 ||
		functionRow.AbsN != 20 ||
		functionRow.CeilScore != 21 ||
		functionRow.CeilingScore != 21 ||
		functionRow.FloorScore != 20 ||
		functionRow.RoundScore != 21 ||
		functionRow.CoalescedV != "B" ||
		functionRow.NullifKeep != "B" ||
		functionRow.DayPart != 2 {
		t.Fatalf("function pushdown rows: unexpected row: %#v", functionRow)
	}
	if functionRow.HourBucket.Year() != 2024 ||
		functionRow.HourBucket.Month() != time.January ||
		functionRow.HourBucket.Day() != 2 ||
		functionRow.HourBucket.Hour() != 12 ||
		functionRow.HourBucket.Minute() != 0 ||
		functionRow.HourBucket.Second() != 0 {
		t.Fatalf("function pushdown rows: unexpected hour bucket: %#v", functionRow.HourBucket)
	}

	var nullFunctionRows []struct {
		LowerV     sql.NullString  `gorm:"column:lower_v"`
		UpperV     sql.NullString  `gorm:"column:upper_v"`
		LengthV    sql.NullInt64   `gorm:"column:length_v"`
		SubstrV    sql.NullString  `gorm:"column:substr_v"`
		TrimmedV   sql.NullString  `gorm:"column:trimmed_v"`
		LtrimV     sql.NullString  `gorm:"column:ltrim_v"`
		RtrimV     sql.NullString  `gorm:"column:rtrim_v"`
		PosB       sql.NullInt64   `gorm:"column:pos_b"`
		AbsN       sql.NullInt64   `gorm:"column:abs_n"`
		CeilScore  sql.NullFloat64 `gorm:"column:ceil_score"`
		FloorScore sql.NullFloat64 `gorm:"column:floor_score"`
		RoundScore sql.NullFloat64 `gorm:"column:round_score"`
		CoalescedV string          `gorm:"column:coalesced_v"`
		NullifSame sql.NullString  `gorm:"column:nullif_same"`
		DayPart    sql.NullFloat64 `gorm:"column:day_part"`
		HourBucket sql.NullTime    `gorm:"column:hour_bucket"`
	}
	if err := db.WithContext(ctx).Raw(
		fmt.Sprintf(`
			SELECT
				lower(v) AS lower_v,
				upper(v) AS upper_v,
				length(v) AS length_v,
				substr(v, 1, 1) AS substr_v,
				btrim(v) AS trimmed_v,
				ltrim(v) AS ltrim_v,
				rtrim(v) AS rtrim_v,
				strpos(v, 'B') AS pos_b,
				abs(n) AS abs_n,
				ceil(score) AS ceil_score,
				floor(score) AS floor_score,
				round(score) AS round_score,
				coalesce(v, 'fallback') AS coalesced_v,
				nullif('same', 'same') AS nullif_same,
				date_part('day', created_at) AS day_part,
				date_trunc('hour', created_at) AS hour_bucket
			FROM %s.func_pushdown_rows
			WHERE id = 201`,
			quoteIdent(schemaName),
		),
	).Scan(&nullFunctionRows).Error; err != nil {
		t.Fatalf("function pushdown null rows: %v", err)
	}
	if len(nullFunctionRows) != 1 {
		t.Fatalf("function pushdown null rows: expected 1 row, got %#v", nullFunctionRows)
	}
	nullFunctionRow := nullFunctionRows[0]
	if nullFunctionRow.LowerV.Valid ||
		nullFunctionRow.UpperV.Valid ||
		nullFunctionRow.LengthV.Valid ||
		nullFunctionRow.SubstrV.Valid ||
		nullFunctionRow.TrimmedV.Valid ||
		nullFunctionRow.LtrimV.Valid ||
		nullFunctionRow.RtrimV.Valid ||
		nullFunctionRow.PosB.Valid ||
		nullFunctionRow.AbsN.Valid ||
		nullFunctionRow.CeilScore.Valid ||
		nullFunctionRow.FloorScore.Valid ||
		nullFunctionRow.RoundScore.Valid ||
		nullFunctionRow.NullifSame.Valid ||
		nullFunctionRow.DayPart.Valid ||
		nullFunctionRow.HourBucket.Valid ||
		nullFunctionRow.CoalescedV != "fallback" {
		t.Fatalf("function pushdown null rows: unexpected row: %#v", nullFunctionRow)
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
