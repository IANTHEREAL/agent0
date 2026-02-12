#!/usr/bin/env bash
#
# db9 快速体验脚本
#
# 前置条件（三个服务都要跑起来）:
#   1. TiKV 集群:  uv run scripts/tikv_admin.py start --name dev --persistent
#   2. pg-tikv:    PD_ENDPOINTS=127.0.0.1:2379 PG_PORT=5433 cargo run --release
#   3. admin API:  cd cloud-admin-portal/backend && ./target/release/pgtikv-admin
#
# 用法:
#   bash cloud-admin-portal/quickstart.sh
#
set -euo pipefail

# ── 配置 ─────────────────────────────────────────────────────────
API="${DB9_API_URL:-http://localhost:8090/api}"
DB9="./target/release/db9"
EMAIL="demo-$(date +%s)@example.com"
PASSWORD="DemoPass123!"
DB_NAME="quickstart-db"

RED='\033[0;31m'; GREEN='\033[0;32m'; YELLOW='\033[1;33m'; CYAN='\033[0;36m'; NC='\033[0m'

step() { echo -e "\n${CYAN}▶ $1${NC}"; }
ok()   { echo -e "${GREEN}  ✓ $1${NC}"; }
fail() { echo -e "${RED}  ✗ $1${NC}"; exit 1; }
info() { echo -e "${YELLOW}  $1${NC}"; }

# ── 0. 构建 ──────────────────────────────────────────────────────
step "构建 db9 ..."
cd "$(dirname "$0")/backend"
if [ ! -f "$DB9" ]; then
    cargo build --release 2>&1 | tail -1
fi
[ -x "$DB9" ] || fail "找不到 $DB9，请先运行 cargo build --release"
ok "db9 二进制就绪: $DB9"

export DB9_API_URL="$API"

# ── 1. 检查服务是否可用 ──────────────────────────────────────────
step "检查 pgtikv-admin 是否在运行 ..."
if ! curl -sf "$API/health" > /dev/null 2>&1; then
    fail "无法连接 $API/health — 请先启动 pgtikv-admin"
fi
ok "API 服务正常"

# ── 2. 注册（用 curl，因为 db9 register 需要交互输入密码）────────
step "注册账号: $EMAIL"
REG_RESP=$(curl -sf -X POST "$API/customer/register" \
    -H "Content-Type: application/json" \
    -d "{\"email\":\"$EMAIL\",\"password\":\"$PASSWORD\"}")
CUSTOMER_ID=$(echo "$REG_RESP" | jq -r '.id')
[ "$CUSTOMER_ID" != "null" ] && [ -n "$CUSTOMER_ID" ] \
    || fail "注册失败: $REG_RESP"
ok "注册成功 — 客户 ID: $CUSTOMER_ID"

# ── 3. 登录 ──────────────────────────────────────────────────────
step "登录 ..."
LOGIN_RESP=$(curl -sf -X POST "$API/customer/login" \
    -H "Content-Type: application/json" \
    -d "{\"email\":\"$EMAIL\",\"password\":\"$PASSWORD\"}")
TOKEN=$(echo "$LOGIN_RESP" | jq -r '.token')
EXPIRES=$(echo "$LOGIN_RESP" | jq -r '.expires_at')
[ "$TOKEN" != "null" ] && [ -n "$TOKEN" ] \
    || fail "登录失败: $LOGIN_RESP"

mkdir -p ~/.db9 && chmod 700 ~/.db9
echo "token = \"$TOKEN\"" > ~/.db9/credentials && chmod 600 ~/.db9/credentials
ok "登录成功 — Token 过期时间: $EXPIRES"
info "凭证已保存到 ~/.db9/credentials"

# ── 4. 创建数据库 ────────────────────────────────────────────────
step "创建数据库: $DB_NAME"
"$DB9" db create --name "$DB_NAME" --region cn-east
DB_ID=$("$DB9" --json db list | jq -r '.[0].id')
[ -n "$DB_ID" ] && [ "$DB_ID" != "null" ] \
    || fail "未能获取数据库 ID"
ok "数据库 ID: $DB_ID"

# ── 5. 查看数据库列表 ────────────────────────────────────────────
step "数据库列表"
"$DB9" db list

# ── 6. 查看详情和连接信息 ────────────────────────────────────────
step "数据库详情"
"$DB9" db status "$DB_ID"

step "连接信息"
"$DB9" db connect "$DB_ID"

# ── 7. 查看 Token ────────────────────────────────────────────────
step "Token 列表"
"$DB9" token list

# ── 8. 尝试用 psql 连接 ──────────────────────────────────────────
step "尝试连接数据库 ..."
CONN=$("$DB9" --json db connect "$DB_ID" | jq -r '.connection_string // empty')
if [ -n "$CONN" ] && command -v psql &>/dev/null; then
    info "psql \"$CONN\""
    if psql "$CONN" -c "SELECT 'db9 quickstart ok!' AS greeting;" 2>/dev/null; then
        ok "psql 连接成功!"
    else
        info "psql 连接失败（pg-tikv 可能还没启动，不影响 demo）"
    fi
else
    info "跳过 psql 测试（psql 未安装或无连接串）"
fi

# ── 9. 清理 ──────────────────────────────────────────────────────
step "清理: 删除数据库 $DB_ID"
echo "y" | "$DB9" db delete "$DB_ID"
ok "数据库已删除"

step "登出"
"$DB9" logout
ok "已登出"

# ── 完成 ──────────────────────────────────────────────────────────
echo ""
echo -e "${GREEN}════════════════════════════════════════════${NC}"
echo -e "${GREEN}  db9 快速体验完成!${NC}"
echo -e "${GREEN}════════════════════════════════════════════${NC}"
echo ""
echo "完整流程: 注册 → 登录 → 建库 → 查看 → 连接 → 删库 → 登出"
echo ""
echo "日常使用:"
echo "  db9 register                         # 注册（仅需一次）"
echo "  db9 login                            # 登录"
echo "  db9 db create --name my-app          # 建库"
echo "  db9 db list                          # 查看所有库"
echo "  db9 db connect <ID>                  # 获取连接串"
echo "  psql \"postgresql://<ID>.admin:xxx@host:5433/postgres\""
echo ""
