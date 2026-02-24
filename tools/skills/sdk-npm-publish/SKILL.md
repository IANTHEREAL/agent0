---
name: sdk-npm-publish
description: "Build and publish the get-db9 TypeScript SDK to npm. Handles build, version bump, and publish with granular access token."
---

# Publish get-db9 SDK to npm

## SDK Location

```
/home/dongxu/lab/db9-server-sdk/sdk/ts/
```

If the worktree doesn't exist, the SDK source is also at:
```
/home/dongxu/lab/db9-server/sdk/ts/
```

## npm Credentials

- **Account**: c4pt0r (huang@pingcap.com)
- **Granular Access Token** (bypass 2FA): `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`

## Publish Steps

Run all commands from the SDK directory (`sdk/ts/`):

### 1. Build and test

```bash
npm run build
npm test
```

### 2. Bump version (if needed)

```bash
# Patch bump (0.1.0 -> 0.1.1)
npm version patch --no-git-tag-version

# Minor bump (0.1.0 -> 0.2.0)
npm version minor --no-git-tag-version

# Or edit package.json "version" directly
```

### 3. Rebuild after version bump

```bash
npm run build
```

### 4. Publish

```bash
# Write token to local .npmrc (DO NOT commit this file)
echo "//registry.npmjs.org/:_authToken=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" > .npmrc

# Publish
npm publish --access public

# Clean up token
rm .npmrc
```

### 5. Verify

```bash
npm view get-db9 version
```

## Package Info

- **Name**: `get-db9`
- **Registry**: https://www.npmjs.com/package/get-db9
- **Exports**:
  - `get-db9` — `instantDatabase()`, `createCustomerClient()`, errors, credentials
  - `get-db9/customer` — `createCustomerClient()` standalone

## E2E Smoke Test (after publish)

```bash
mkdir -p /tmp/sdk-smoke && cd /tmp/sdk-smoke
npm init -y && node -e "let p=require('./package.json'); p.type='module'; require('fs').writeFileSync('package.json',JSON.stringify(p,null,2))"
npm install get-db9@latest tsx

cat > test.ts << 'EOF'
import { instantDatabase, createCustomerClient, MemoryCredentialStore } from 'get-db9';
const store = new MemoryCredentialStore();
const db = await instantDatabase({ name: `smoke-${Date.now()}`, credentialStore: store, seed: 'SELECT 1' });
console.log('DB created:', db.databaseId, db.connectionString);
const creds = await store.load();
const client = createCustomerClient({ token: creds!.token });
const r = await client.databases.sql(db.databaseId, 'SELECT 1 AS ok');
console.log('SQL result:', r.rows);
await client.databases.delete(db.databaseId);
console.log('Cleaned up. All good!');
EOF

npx tsx test.ts
rm -rf /tmp/sdk-smoke
```

## Important Notes

- `.npmrc` with token must NEVER be committed to git (it's in `.gitignore`)
- Always `rm .npmrc` after publishing
- The token has "bypass 2FA" permission — treat it as a secret
- Always run `npm run build` before `npm publish` (dist/ must be fresh)
- Always run `npm test` before publishing to catch regressions
