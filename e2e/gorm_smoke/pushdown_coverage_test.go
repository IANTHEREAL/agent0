package gorm_smoke

import (
	"context"
	"database/sql"
	"encoding/json"
	"fmt"
	"os"
	"path/filepath"
	"reflect"
	"runtime"
	"strings"
	"testing"
	"time"

	"gorm.io/gorm"
)

type pushdownCoverageManifest struct {
	PointAccess  string                  `json:"point_access"`
	SelectFilter string                  `json:"select_filter"`
	Suites       []pushdownCoverageSuite `json:"suites"`
}

type pushdownCoverageSuite struct {
	Name           string                 `json:"name"`
	Comment        string                 `json:"comment"`
	BaseTable      string                 `json:"base_table"`
	SetupSQL       []string               `json:"setup_sql"`
	Cases          []pushdownCoverageCase `json:"cases"`
	UpdateStrategy string                 `json:"update_strategy"`
}

type pushdownCoverageCase struct {
	Name        string `json:"name"`
	Expr        string `json:"expr"`
	ExpectedSQL string `json:"expected_sql"`
}

type pushdownMarkerSnapshot struct {
	ID     int
	Marker sql.NullString
}

func TestGormPushdownFunctionOperatorCoverage(t *testing.T) {
	if !gormPushdownEnabled() {
		t.Skip("DB9_GORM_SMOKE_ENABLE_COP_PUSHDOWN not set; skipping pushdown coverage suite")
	}

	dsn := strings.TrimSpace(os.Getenv("PG_DSN"))
	if dsn == "" {
		t.Skip("PG_DSN not set; skipping e2e smoke test")
	}
	dsn = normalizeDSN(dsn)

	manifest := loadPushdownCoverageManifest(t)

	ctx, cancel := context.WithTimeout(context.Background(), 10*time.Minute)
	t.Cleanup(cancel)

	schemaName := fmt.Sprintf("gorm_push_cov_%s_%d", randomHex(t, 4), time.Now().Unix())
	db, cleanup := openDB(t, ctx, dsn, schemaName)
	t.Cleanup(cleanup)

	if err := db.WithContext(ctx).Exec("CREATE SCHEMA " + quoteIdent(schemaName)).Error; err != nil {
		t.Fatalf("create schema %q: %v", schemaName, err)
	}

	for _, suite := range manifest.Suites {
		suite := suite
		t.Run(suite.Name, func(t *testing.T) {
			runGormPushdownCoverageSuite(t, db, ctx, schemaName, manifest, suite)
		})
	}
}

func runGormPushdownCoverageSuite(
	t *testing.T,
	db *gorm.DB,
	ctx context.Context,
	schemaName string,
	manifest pushdownCoverageManifest,
	suite pushdownCoverageSuite,
) {
	t.Helper()

	for _, stmt := range suite.SetupSQL {
		mustExecPushdownSQL(t, db, ctx, renderPushdownSQL(stmt, schemaName))
	}

	updateOnTable := suite.BaseTable + "_update_on"
	updateOffTable := suite.BaseTable + "_update_off"
	createPushdownCoverageClone(t, db, ctx, schemaName, suite.BaseTable, updateOnTable)
	createPushdownCoverageClone(t, db, ctx, schemaName, suite.BaseTable, updateOffTable)

	for _, testCase := range suite.Cases {
		testCase := testCase
		t.Run(testCase.Name, func(t *testing.T) {
			selectQuery := fmt.Sprintf(
				"SELECT %s AS %s FROM %s WHERE %s LIMIT 1",
				testCase.Expr,
				testCase.Name+"_out",
				qualifiedPushdownTable(schemaName, suite.BaseTable),
				manifest.SelectFilter,
			)

			withCopPushdownSetting(t, db, ctx, true, func(pushdownDB *gorm.DB) error {
				return explainContainsSubstrings(
					pushdownDB,
					ctx,
					selectQuery,
					[]string{
						"DB9 Cop Output: " + testCase.Name + "_out",
					},
				)
			})

			selectMatchQuery := buildPushdownCoverageSelectMatchQuery(
				schemaName,
				suite.BaseTable,
				manifest.SelectFilter,
				testCase,
			)
			selectOnIDs := queryIntRowsWithPushdown(t, db, ctx, true, selectMatchQuery)
			if len(selectOnIDs) != 1 {
				t.Fatalf("select-on result set mismatch for %s/%s: got %#v", suite.Name, testCase.Name, selectOnIDs)
			}
			selectOffIDs := queryIntRowsWithPushdown(t, db, ctx, false, selectMatchQuery)
			if !reflect.DeepEqual(selectOnIDs, selectOffIDs) {
				t.Fatalf(
					"select parity mismatch for %s/%s: on=%#v off=%#v",
					suite.Name,
					testCase.Name,
					selectOnIDs,
					selectOffIDs,
				)
			}

			resetMarkerColumn(t, db, ctx, schemaName, updateOnTable)
			resetMarkerColumn(t, db, ctx, schemaName, updateOffTable)

			updateOnQuery := buildPushdownCoverageUpdateQuery(
				schemaName,
				suite.BaseTable,
				updateOnTable,
				manifest.SelectFilter,
				suite.UpdateStrategy,
				testCase,
			)
			rowsOn := execRowsAffectedWithPushdown(t, db, ctx, true, updateOnQuery)
			if rowsOn != 1 {
				t.Fatalf("update-on rows affected mismatch for %s/%s: got %d", suite.Name, testCase.Name, rowsOn)
			}

			updateOffQuery := buildPushdownCoverageUpdateQuery(
				schemaName,
				suite.BaseTable,
				updateOffTable,
				manifest.SelectFilter,
				suite.UpdateStrategy,
				testCase,
			)
			rowsOff := execRowsAffectedWithPushdown(t, db, ctx, false, updateOffQuery)
			if rowsOff != 1 {
				t.Fatalf("update-off rows affected mismatch for %s/%s: got %d", suite.Name, testCase.Name, rowsOff)
			}

			onSnapshot := fetchMarkerSnapshot(t, db, ctx, schemaName, updateOnTable)
			offSnapshot := fetchMarkerSnapshot(t, db, ctx, schemaName, updateOffTable)
			if !reflect.DeepEqual(onSnapshot, offSnapshot) {
				t.Fatalf("update parity mismatch for %s/%s: on=%#v off=%#v", suite.Name, testCase.Name, onSnapshot, offSnapshot)
			}

			postUpdateOnQuery := buildPushdownCoveragePostUpdateSelectQuery(schemaName, updateOnTable, testCase)
			postUpdateOnIDs := queryIntRowsWithPushdown(t, db, ctx, true, postUpdateOnQuery)
			if len(postUpdateOnIDs) != 1 {
				t.Fatalf(
					"post-update select-on result set mismatch for %s/%s: got %#v",
					suite.Name,
					testCase.Name,
					postUpdateOnIDs,
				)
			}

			postUpdateOffQuery := buildPushdownCoveragePostUpdateSelectQuery(schemaName, updateOffTable, testCase)
			postUpdateOffIDs := queryIntRowsWithPushdown(t, db, ctx, false, postUpdateOffQuery)
			if !reflect.DeepEqual(postUpdateOnIDs, postUpdateOffIDs) {
				t.Fatalf(
					"post-update select parity mismatch for %s/%s: on=%#v off=%#v",
					suite.Name,
					testCase.Name,
					postUpdateOnIDs,
					postUpdateOffIDs,
				)
			}
		})
	}
}

func loadPushdownCoverageManifest(t *testing.T) pushdownCoverageManifest {
	t.Helper()

	_, file, _, ok := runtime.Caller(0)
	if !ok {
		t.Fatal("resolve current file for pushdown coverage manifest")
	}

	manifestPath := filepath.Clean(filepath.Join(filepath.Dir(file), "..", "pushdown_coverage_manifest.json"))
	raw, err := os.ReadFile(manifestPath)
	if err != nil {
		t.Fatalf("read pushdown coverage manifest %q: %v", manifestPath, err)
	}

	var manifest pushdownCoverageManifest
	if err := json.Unmarshal(raw, &manifest); err != nil {
		t.Fatalf("unmarshal pushdown coverage manifest %q: %v", manifestPath, err)
	}
	return manifest
}

func renderPushdownSQL(sqlText, schemaName string) string {
	return strings.ReplaceAll(sqlText, "{schema}", quoteIdent(schemaName))
}

func qualifiedPushdownTable(schemaName, tableName string) string {
	return quoteIdent(schemaName) + "." + quoteIdent(tableName)
}

func mustExecPushdownSQL(t *testing.T, db *gorm.DB, ctx context.Context, sqlText string) {
	t.Helper()
	if err := db.WithContext(ctx).Exec(sqlText).Error; err != nil {
		t.Fatalf("exec %q: %v", sqlText, err)
	}
}

func buildPushdownCoverageUpdateQuery(
	schemaName string,
	sourceTable string,
	tableName string,
	selectFilter string,
	updateStrategy string,
	testCase pushdownCoverageCase,
) string {
	qualifiedTable := qualifiedPushdownTable(schemaName, tableName)
	sourceQualifiedTable := qualifiedPushdownTable(schemaName, sourceTable)

	switch updateStrategy {
	case "", "direct_filter":
		return fmt.Sprintf(
			"UPDATE %s SET marker = %s WHERE %s AND ((%s) IS NOT DISTINCT FROM %s)",
			qualifiedTable,
			quoteLiteral(testCase.Name),
			selectFilter,
			testCase.Expr,
			testCase.ExpectedSQL,
		)
	case "projected_match_by_id":
		// Keep the update statement case-local, but source the matching proof from the
		// already-green base-table select path rather than the update clone itself.
		return fmt.Sprintf(
			"UPDATE %s SET marker = %s WHERE id = (SELECT id FROM %s WHERE %s AND ((%s) IS NOT DISTINCT FROM %s) LIMIT 1)",
			qualifiedTable,
			quoteLiteral(testCase.Name),
			sourceQualifiedTable,
			selectFilter,
			testCase.Expr,
			testCase.ExpectedSQL,
		)
	default:
		panic(fmt.Sprintf("unsupported pushdown coverage update strategy %q", updateStrategy))
	}
}

func buildPushdownCoverageSelectMatchQuery(
	schemaName string,
	tableName string,
	selectFilter string,
	testCase pushdownCoverageCase,
) string {
	return fmt.Sprintf(
		"SELECT id FROM %s WHERE %s AND ((%s) IS NOT DISTINCT FROM %s) ORDER BY id",
		qualifiedPushdownTable(schemaName, tableName),
		selectFilter,
		testCase.Expr,
		testCase.ExpectedSQL,
	)
}

func buildPushdownCoveragePostUpdateSelectQuery(
	schemaName string,
	tableName string,
	testCase pushdownCoverageCase,
) string {
	return fmt.Sprintf(
		"SELECT id FROM %s WHERE marker = %s ORDER BY id",
		qualifiedPushdownTable(schemaName, tableName),
		quoteLiteral(testCase.Name),
	)
}

func createPushdownCoverageClone(
	t *testing.T,
	db *gorm.DB,
	ctx context.Context,
	schemaName string,
	sourceTable string,
	cloneTable string,
) {
	t.Helper()

	source := qualifiedPushdownTable(schemaName, sourceTable)
	clone := qualifiedPushdownTable(schemaName, cloneTable)
	// db9-server does not accept PostgreSQL's full LIKE ... INCLUDING ALL table-clone syntax.
	// Materialize the copy directly instead; the coverage harness only needs identical rows.
	mustExecPushdownSQL(t, db, ctx, fmt.Sprintf("CREATE TABLE %s AS SELECT * FROM %s", clone, source))
	mustExecPushdownSQL(t, db, ctx, fmt.Sprintf("ANALYZE %s", clone))
}

func withCopPushdownSetting(
	t *testing.T,
	db *gorm.DB,
	ctx context.Context,
	enabled bool,
	fn func(pushdownDB *gorm.DB) error,
) {
	t.Helper()

	setting := "off"
	if enabled {
		setting = "on"
	}

	if err := db.WithContext(ctx).Transaction(func(tx *gorm.DB) error {
		if err := tx.Exec("SET LOCAL db9.enable_cop_pushdown = " + setting).Error; err != nil {
			return fmt.Errorf("set local db9.enable_cop_pushdown=%s: %w", setting, err)
		}
		return fn(tx.WithContext(ctx))
	}); err != nil {
		t.Fatalf("pushdown transaction (%s): %v", setting, err)
	}
}

func queryIntRowsWithPushdown(
	t *testing.T,
	db *gorm.DB,
	ctx context.Context,
	enabled bool,
	query string,
) []int {
	t.Helper()

	var values []int
	withCopPushdownSetting(t, db, ctx, enabled, func(pushdownDB *gorm.DB) error {
		rows, err := pushdownDB.Raw(query).Rows()
		if err != nil {
			return err
		}
		defer rows.Close()

		for rows.Next() {
			var value int
			if err := rows.Scan(&value); err != nil {
				return err
			}
			values = append(values, value)
		}
		return rows.Err()
	})
	return values
}

func execRowsAffectedWithPushdown(
	t *testing.T,
	db *gorm.DB,
	ctx context.Context,
	enabled bool,
	query string,
) int64 {
	t.Helper()

	var rowsAffected int64
	withCopPushdownSetting(t, db, ctx, enabled, func(pushdownDB *gorm.DB) error {
		result := pushdownDB.Exec(query)
		if result.Error != nil {
			return result.Error
		}
		rowsAffected = result.RowsAffected
		return nil
	})
	return rowsAffected
}

func resetMarkerColumn(t *testing.T, db *gorm.DB, ctx context.Context, schemaName, tableName string) {
	t.Helper()
	mustExecPushdownSQL(
		t,
		db,
		ctx,
		fmt.Sprintf("UPDATE %s SET marker = NULL", qualifiedPushdownTable(schemaName, tableName)),
	)
}

func fetchMarkerSnapshot(
	t *testing.T,
	db *gorm.DB,
	ctx context.Context,
	schemaName string,
	tableName string,
) []pushdownMarkerSnapshot {
	t.Helper()

	rows, err := db.WithContext(ctx).Raw(
		fmt.Sprintf(
			"SELECT id, marker FROM %s ORDER BY id",
			qualifiedPushdownTable(schemaName, tableName),
		),
	).Rows()
	if err != nil {
		t.Fatalf("query marker snapshot for %s: %v", tableName, err)
	}
	defer rows.Close()

	var snapshot []pushdownMarkerSnapshot
	for rows.Next() {
		var row pushdownMarkerSnapshot
		if err := rows.Scan(&row.ID, &row.Marker); err != nil {
			t.Fatalf("scan marker snapshot for %s: %v", tableName, err)
		}
		snapshot = append(snapshot, row)
	}
	if err := rows.Err(); err != nil {
		t.Fatalf("iterate marker snapshot for %s: %v", tableName, err)
	}
	return snapshot
}
