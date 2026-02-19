# db9 Production Deployment Guide

## Architecture Overview

```
                     ┌─────────────────────────────────────┐
                     │         EKS Cluster (arm64)         │
                     │                                     │
  HTTPS (443)        │  ┌───────────┐     ┌─────────────┐  │
  ──────────────────►│  │  Ingress  │────►│   Landing   │  │
  db9.example.com    │  │  (nginx)  │     │   (nginx)   │  │
                     │  │           │     └─────────────┘  │
                     │  │  /api ────│───►┌──────────────┐  │
                     │  │           │    │   Backend    │  │
                     │  │  /sdk ────│───►│  (Rust/axum) │  │
                     │  │  /fs9 ────│─┐  └──────┬───────┘  │
                     │  └───────────┘ │         │          │
                     │                │  ┌──────▼───────┐  │
                     │                │  │  PostgreSQL  │  │
                     │                │  │  (metadata)  │  │
                     │                │  └──────────────┘  │
                     │                │                    │
  TCP (5433)         │  ┌─────────┐   │  ┌───────────────┐ │
  ──────────────────►│  │  NLB    │───│─►│   pg-tikv     │ │
  pg.example.com     │  └─────────┘   │  │  (SQL engine) │ │
                     │                │  └──────┬────────┘ │
                     │                │         │          │
                     │                │  ┌──────▼────────┐ │
                     │  ┌──────────┐  │  │    TiKV       │ │
                     │  │ fs9-meta │  │  │  (storage)    │ │
                     │  └────┬─────┘  │  └───────────────┘ │
                     │       │        │                    │
                     │  ┌────▼─────┐  │                    │
                     │  │fs9-server│◄─┘                    │
                     │  └──────────┘                       │
                     └─────────────────────────────────────┘
```

### Components

| Component | Description | K8s Resource | Namespace | Port |
|-----------|-------------|-------------|-----------|------|
| **pg-tikv** | PostgreSQL-compatible SQL engine on TiKV | Deployment | `tipg` | 5433 |
| **backend** | Admin API (Rust/axum) — user auth, DB lifecycle, billing | Deployment | `cloud-admin-portal` | 8090 |
| **landing** | Static landing page + CLI binaries (nginx:alpine) | Deployment | `cloud-admin-portal` | 80 |
| **PostgreSQL** | Backend metadata database | StatefulSet (10Gi PVC) | `cloud-admin-portal` | 5432 |
| **fs9-server** | Distributed filesystem REST API | Deployment | `cloud-admin-portal` | 9999 |
| **fs9-meta** | Filesystem metadata & auth service | Deployment | `cloud-admin-portal` | 9998 |
| **db9 CLI** | Client binary (4 platforms) | Built via GitHub Actions | N/A | N/A |

### Namespace Layout

- `tipg` — pg-tikv only (separate namespace for isolation)
- `cloud-admin-portal` — backend, landing, PostgreSQL, fs9-server, fs9-meta
- `tidb-serverless` — TiKV cluster (managed separately)

---

## Prerequisites

- Docker with buildx (arm64 cross-compilation)
- kubectl configured for the EKS cluster
- AWS CLI with SSO profile configured
- `gh` CLI (GitHub CLI) for CI workflows
- ECR repositories created with cross-account pull policy

### SSO Login

```bash
aws sso login --profile sso
```

### ECR Login

```bash
aws ecr get-login-password --region us-west-2 | \
  docker login --username AWS --password-stdin <ECR_ACCOUNT>.dkr.ecr.us-west-2.amazonaws.com
```

### Buildx Setup (one-time)

```bash
docker buildx create --name arm-builder --platform linux/arm64 --use
docker buildx inspect --bootstrap
```

---

## Deploy Workflow: db9 (pg-tikv + backend + CLI + landing)

### Step 0: Pull Latest & Detect Changes

```bash
cd <REPO_ROOT> && git pull

DEPLOYED_COMMIT=$(kubectl get configmap db9-deploy-info -n cloud-admin-portal \
  -o jsonpath='{.data.commit}' 2>/dev/null || echo "none")
CURRENT_COMMIT=$(git rev-parse HEAD)

# Skip if already deployed
if [ "$DEPLOYED_COMMIT" = "$CURRENT_COMMIT" ]; then
  echo "Already up to date: $(git rev-parse --short HEAD)"
  exit 0
fi

# Detect which components changed
PGTIKV_CHANGED=$(git diff --name-only ${DEPLOYED_COMMIT}..HEAD -- \
  src/ vendor/ crates/ Cargo.toml Cargo.lock Dockerfile build.rs | head -1)
BACKEND_CHANGED=$(git diff --name-only ${DEPLOYED_COMMIT}..HEAD -- \
  cloud-admin-portal/backend/ | head -1)
CLI_CHANGED=$(git diff --name-only ${DEPLOYED_COMMIT}..HEAD -- \
  cloud-admin-portal/backend/src/db9.rs \
  cloud-admin-portal/backend/src/cli_common.rs \
  cloud-admin-portal/backend/Cargo.toml \
  .github/workflows/release-db9.yml | head -1)
LANDING_CHANGED=$(git diff --name-only ${DEPLOYED_COMMIT}..HEAD -- \
  cloud-admin-portal/landing/ | head -1)
```

Only rebuild components that actually changed. If CLI changed, landing must also be rebuilt (new binaries get bundled).

### Step 1: Push Local Commits (critical before CI)

```bash
git push  # CI builds from remote HEAD — unpushed commits won't be included
```

### Step 2: Trigger CLI Build (if CLI changed)

```bash
gh workflow run release-db9.yml
sleep 5
RUN_ID=$(gh run list --workflow=release-db9.yml --limit 1 --json databaseId -q '.[0].databaseId')
```

### Step 3: Build & Push Images (parallel)

**pg-tikv** (if changed):
```bash
cd <REPO_ROOT>
docker buildx build --platform linux/arm64 \
  --build-arg BUILD_GIT_HASH=$(git rev-parse --short=8 HEAD) \
  --build-arg BUILD_DATE=$(date -u +%Y-%m-%d) \
  -t <ECR_URI>/pg-tikv:latest \
  --push .
```

**Backend** (if changed):
```bash
cd <REPO_ROOT>/cloud-admin-portal/backend
docker buildx build --platform linux/arm64 \
  -t <ECR_URI>/cloud-admin-portal-backend:latest \
  --push .
```

### Step 4: Wait for CI & Download Artifacts (if CLI was triggered)

```bash
gh run watch $RUN_ID

cd <REPO_ROOT>/cloud-admin-portal/landing/releases
rm -f db9-*
gh run download $RUN_ID

# Flatten artifact directories
for dir in db9-*/; do
  name="${dir%/}"
  cp "$name/$name" "${name}.bin"
  rm -rf "$name"
  mv "${name}.bin" "$name"
done
```

Verify 4 binaries: `db9-linux-amd64`, `db9-linux-arm64`, `db9-darwin-amd64`, `db9-darwin-arm64`.

### Step 5: Build & Push Landing Image (if landing or CLI changed)

```bash
cd <REPO_ROOT>/cloud-admin-portal/landing
docker buildx build --platform linux/arm64 \
  -t <ECR_URI>/cloud-admin-portal-landing:latest \
  --push .
```

### Step 6: Apply Manifests & Restart

```bash
kubectl apply -f <REPO_ROOT>/cloud-admin-portal/k8s/deploy.yaml

# pg-tikv is in 'tipg' namespace (not cloud-admin-portal!)
kubectl rollout restart deployment pg-tikv -n tipg
kubectl rollout status deployment pg-tikv -n tipg --timeout=120s

# Backend & landing are in 'cloud-admin-portal'
kubectl rollout restart deployment cloud-admin-backend -n cloud-admin-portal
kubectl rollout restart deployment cloud-admin-landing -n cloud-admin-portal
kubectl rollout status deployment cloud-admin-backend -n cloud-admin-portal --timeout=120s
kubectl rollout status deployment cloud-admin-landing -n cloud-admin-portal --timeout=120s
```

**Never restart `cloud-admin-pg` StatefulSet** unless specifically needed — it's a persistent database.

### Step 7: Record Deployed Commit

```bash
kubectl create configmap db9-deploy-info \
  --from-literal=commit=$(git rev-parse HEAD) \
  --from-literal=date="$(date -u +%Y-%m-%dT%H:%M:%SZ)" \
  --from-literal=short="$(git rev-parse --short HEAD)" \
  --from-literal=message="$(git log -1 --pretty=%s)" \
  -n cloud-admin-portal \
  --dry-run=client -o yaml | kubectl apply -f -
```

### Step 8: Verify

```bash
curl -sS -o /dev/null -w "%{http_code}" https://<DOMAIN>/        # Landing → 200
curl -sS -o /dev/null -w "%{http_code}" https://<DOMAIN>/api/health  # Backend → 200
curl -sS -o /dev/null -w "%{http_code}" https://<DOMAIN>/install     # CLI installer → 200
```

---

## Deploy Workflow: fs9 (Distributed Filesystem)

### Build & Push Images (parallel)

```bash
cd <FS9_REPO_ROOT>

# fs9-server
docker buildx build --platform linux/arm64 \
  -f docker/Dockerfile.server-tikv \
  -t <ECR_URI>/fs9-server:latest \
  --push .

# fs9-meta
docker buildx build --platform linux/arm64 \
  -f docker/Dockerfile.meta-rust \
  -t <ECR_URI>/fs9-meta:latest \
  --push .
```

### Restart (meta before server — server depends on meta for mount configs)

```bash
kubectl apply -f <REPO_ROOT>/cloud-admin-portal/k8s/deploy.yaml

kubectl rollout restart deployment fs9-meta -n cloud-admin-portal
kubectl rollout status deployment fs9-meta -n cloud-admin-portal --timeout=120s

kubectl rollout restart deployment fs9-server -n cloud-admin-portal
kubectl rollout status deployment fs9-server -n cloud-admin-portal --timeout=120s
```

### Verify

```bash
curl -sS -o /dev/null -w "%{http_code}" https://<DOMAIN>/fs9/health
kubectl logs -n cloud-admin-portal deployment/fs9-server --tail=10
# Should see: "FS9 Server listening on http://0.0.0.0:9999"
# Should see: "Default pagefs config loaded for auto-provisioning"
```

---

## Deploy a New Service to the Cluster

### Checklist

1. **Create ECR repository** with cross-account pull policy
2. **Build Docker image** for `linux/arm64`
3. **Write K8s manifest** (Deployment + Service + Ingress/NLB + TLS Certificate)
4. **Apply & verify** pod health
5. **Create DNS record** in Route53 (CNAME to Ingress ELB or NLB)

### Exposure Options

| Type | Use Case | K8s Resources |
|------|----------|---------------|
| HTTP/HTTPS via Ingress | Websites, REST APIs | ClusterIP Service + Ingress + Certificate |
| TCP via NLB | Databases, custom protocols | LoadBalancer Service |
| Internal only | Inter-service communication | ClusterIP Service |

### TLS

- cert-manager with Let's Encrypt (DNS-01 via Route53), auto-renews
- For TCP services: mount cert secret as volume at `/tls/`

---

## pg-tikv Runtime Configuration

### Critical Environment Variables

| Variable | Purpose | Required |
|----------|---------|----------|
| `PGTIKV_INSECURE=1` | Allow cleartext password auth for internal connections (backend uses NoTls) | Yes (staging) |
| `PGTIKV_BOOTSTRAP_ADMIN_PASSWORD=admin` | Default admin password for new keyspaces | Yes |
| `PGTIKV_SESSION_TTL_HOURS` | Session TTL (default: 1h) | No |

These are set directly on the deployment (not in deploy.yaml):
```bash
kubectl set env deployment/pg-tikv -n tipg \
  PGTIKV_BOOTSTRAP_ADMIN_PASSWORD=<password> \
  PGTIKV_INSECURE=1
```

Without these, `db9 db create` fails with "Failed to set admin password".

### Build Args

pg-tikv embeds version info at build time:
```bash
--build-arg BUILD_GIT_HASH=$(git rev-parse --short=8 HEAD)
--build-arg BUILD_DATE=$(date -u +%Y-%m-%d)
```

`build.rs` checks env vars first, falls back to git/date commands.

---

## fs9 Configuration

### Service Dependencies

```
fs9-server → fs9-meta (mount configs)
fs9-server → TiKV (pagefs storage, mTLS)
fs9-server → backend (db9 token validation)
fs9-meta   → PostgreSQL (metadata store)
```

### Key Points

- pagefs plugin connects to TiKV for persistent storage (one keyspace per tenant)
- Auto-provisioning: when a db9-authenticated request arrives for an unknown tenant, fs9-server auto-creates namespace + pagefs mount
- pg-tikv's fs9 SQL extension connects to fs9-server via HTTP with short-lived JWT tokens
- Cross-namespace communication: pg-tikv (in `tipg`) → fs9-server (in `cloud-admin-portal`) via `fs9-server.cloud-admin-portal.svc.cluster.local:9999`
- `fs9-secret` must exist in both namespaces

---

## db9 CLI Build Pipeline

- **CI**: GitHub Actions (`.github/workflows/release-db9.yml`)
- **Platforms**: 4 native runners (no cross-compilation for macOS)
  - `ubuntu-latest` → `db9-linux-amd64`
  - `ubuntu-24.04-arm` → `db9-linux-arm64`
  - `macos-14` → `db9-darwin-amd64`
  - `macos-latest` → `db9-darwin-arm64`
- **Distribution**: binaries bundled into landing page nginx image at `/releases/db9-{os}-{arch}`
- **Install**: `curl -fsSL https://<DOMAIN>/install | sh`

---

## Parallelization Strategy

To minimize total deploy time:

```
Time ──────────────────────────────────────────────────►

 ┌─ Trigger CLI CI ──────────── Wait for CI ── Download artifacts ─┐
 │                                                                  │
 ├─ Build pg-tikv image ──────┐                                     ├─ Build landing image
 │                             ├─ Restart pg-tikv + backend         │
 └─ Build backend image ──────┘                                     └─ Restart landing
```

- **Parallel group**: CLI CI trigger + pg-tikv build + backend build
- **Sequential**: Wait for CI → download → build landing (needs CLI binaries)
- **Then**: restart all changed deployments

---

## Lessons Learned

### Serialization Compatibility

- **bincode 1.3 does NOT honor `#[serde(default)]`** for appended struct fields. Adding a field to a bincode-serialized struct is a breaking change in both directions (old data can't be read by new code, new data can't be read by old code).
- **Fix**: implement fallback deserialization with legacy struct definitions. Try current format first, fall back to legacy format on failure.
- Always consider storage format compatibility when adding fields to persisted structs.

### Stack Overflow in Async Executors

- Deep async call chains (analyzer → optimizer → executor → sub-dispatchers) can overflow the default thread stack.
- **Symptoms**: `fatal runtime error: stack overflow` with no useful backtrace.
- **Fixes**: Box large futures at the source (`Box::pin`), split monolithic dispatchers into smaller sub-modules.

### Docker Build Pitfalls

- **Wrong build context**: running `docker buildx build --push .` from the wrong directory builds the wrong image (e.g., nginx instead of pg-tikv). Always verify your working directory matches the intended Dockerfile.
- **Architecture mismatch**: EKS nodes are arm64. x86_64 images cause `exec format error` at runtime.
- **Rust compiler version drift**: `Cargo.lock` can pull new crate versions requiring newer rustc. If build fails with version errors, bump the `rust:X.XX-bookworm` base image in Dockerfile.
- **Cache staleness**: if buildx fails with cache errors, prune with `docker buildx prune --builder arm-builder -f`.

### Namespace Awareness

- pg-tikv runs in `tipg` namespace, everything else in `cloud-admin-portal`.
- Forgetting `-n tipg` for pg-tikv operations is a common mistake.
- Cross-namespace service references use full DNS: `<service>.<namespace>.svc.cluster.local`.

### Deployment Safety

- **Never restart the PostgreSQL StatefulSet** (`cloud-admin-pg`) during routine deploys — it's a persistent database.
- **Never force-push to master** — it can remove other people's merged PRs. Always create branches for fixes.
- **Always verify with `kubectl rollout status`** — a successful image push doesn't mean the pod is healthy.
- **Track deployed commits** via ConfigMap to enable incremental change detection.

### Credential Management

- pg-tikv K8s deployment env vars (like `PGTIKV_INSECURE`, `PGTIKV_BOOTSTRAP_ADMIN_PASSWORD`) are NOT in `deploy.yaml` — they're set via `kubectl set env` and persist across restarts.
- Secrets (`cloud-admin-pg-secret`, `fs9-secret`) are managed separately from the deployment manifests.
- SSO sessions expire frequently — always check auth before starting a deploy.

---

## Rollback Procedure

If a deployment causes issues:

```bash
# 1. Identify the last known good commit
kubectl get configmap db9-deploy-info -n cloud-admin-portal -o yaml

# 2. Check out the good commit
git checkout <GOOD_COMMIT>

# 3. Rebuild and push the affected component
docker buildx build --platform linux/arm64 \
  --build-arg BUILD_GIT_HASH=$(git rev-parse --short=8 HEAD) \
  --build-arg BUILD_DATE=$(date -u +%Y-%m-%d) \
  -t <ECR_URI>/<IMAGE>:latest \
  --push .

# 4. Restart the deployment
kubectl rollout restart deployment <NAME> -n <NAMESPACE>
kubectl rollout status deployment <NAME> -n <NAMESPACE> --timeout=120s

# 5. Verify the fix, then return to master
git checkout master
```

---

## Troubleshooting

| Problem | Diagnosis | Fix |
|---------|-----------|-----|
| ECR push 403 Forbidden | Auth expired | Re-run `aws ecr get-login-password ...` |
| SSO session expired | `aws sso login` | Re-login with SSO profile |
| Pod CrashLoopBackOff | Check logs: `kubectl logs -n <ns> -l app=<name> --tail=50` | Fix code or rollback |
| `exec format error` | Wrong image architecture | Rebuild with `--platform linux/arm64` |
| `Failed to set admin password` | Missing pg-tikv env vars | Set `PGTIKV_BOOTSTRAP_ADMIN_PASSWORD` and `PGTIKV_INSECURE=1` |
| `Failed to deserialize schema` | Backward-incompatible storage format change | Add fallback deserialization for legacy format |
| `stack overflow` in pg-tikv | Deep async recursion | Box large futures, split dispatchers |
| CI job fails | Check failed job logs | `gh run view <ID> --log-failed` |
| Image not updating after push | Image caching | Ensure `imagePullPolicy: Always` or use unique tags |
| Cross-namespace DNS failure | Wrong service reference | Use `<svc>.<ns>.svc.cluster.local` |
| `Cargo.lock` requires newer rustc | Dependency version bump | Update `rust:X.XX-bookworm` in Dockerfile |
| buildx builder not found | Builder not created | `docker buildx create --name arm-builder --platform linux/arm64 --use` |
| Certificate stuck in Pending | cert-manager issue | Check `kubectl logs -n cert-manager -l app=cert-manager` |
