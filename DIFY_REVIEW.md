# Dify PostgreSQL 兼容性测试报告

**测试时间**: 2026-01-22
**目标数据库**: sv.0xffff.me:5433

---

## 当前阻塞问题

无

---

## 已解决的问题

- ✅ Extended Query Protocol 参数类型推断 (OID 705 unknown)
- ✅ JOIN ... USING 语法支持
- ✅ pg_type 内置类型定义
- ✅ ALTER COLUMN ... TYPE ... USING 语法支持
- ✅ pg_attribute 新增 attgenerated, attidentity 列
- ✅ format_type(atttypid, atttypmod) 函数
- ✅ pg_get_expr(adbin, adrelid) 函数
- ✅ pg_get_serial_sequence(table, column) 函数
- ✅ pg_table_is_visible(oid) 函数
