export const apiUrl = process.env.DB9_API_URL || 'http://localhost:8090/api';

export let e2eStackUp = false;
export let e2eDatabaseOpsUp = false;

async function checkHealth(): Promise<boolean> {
  const healthUrl = `${apiUrl.replace(/\/+$/, '')}/health`;
  try {
    const response = await fetch(healthUrl, { method: 'GET' });
    return response.ok;
  } catch {
    return false;
  }
}

function uniqueName(prefix: string): string {
  return `${prefix}-${Date.now()}-${Math.random().toString(36).slice(2, 8)}`;
}

async function checkDatabaseOps(): Promise<boolean> {
  const base = apiUrl.replace(/\/+$/, '');
  const registerResponse = await fetch(`${base}/customer/anonymous-register`, {
    method: 'POST',
    headers: { 'Content-Type': 'application/json' },
    body: '{}',
  });

  if (!registerResponse.ok) {
    return false;
  }

  const registerPayload = (await registerResponse.json()) as { token?: string };
  if (!registerPayload.token) {
    return false;
  }

  const createResponse = await fetch(`${base}/customer/databases`, {
    method: 'POST',
    headers: {
      Authorization: `Bearer ${registerPayload.token}`,
      'Content-Type': 'application/json',
    },
    body: JSON.stringify({ name: uniqueName('e2e-probe') }),
  });

  if (!createResponse.ok) {
    return false;
  }

  const database = (await createResponse.json()) as { id?: string };
  if (!database.id) {
    return false;
  }

  await fetch(`${base}/customer/databases/${database.id}`, {
    method: 'DELETE',
    headers: { Authorization: `Bearer ${registerPayload.token}` },
  });

  return true;
}

e2eStackUp = await checkHealth();

if (e2eStackUp) {
  try {
    e2eDatabaseOpsUp = await checkDatabaseOps();
  } catch {
    e2eDatabaseOpsUp = false;
  }
}

if (!e2eStackUp) {
  console.warn(`[e2e] DB9 stack is unreachable at ${apiUrl}; skipping E2E tests.`);
}

if (e2eStackUp && !e2eDatabaseOpsUp) {
  console.warn(`[e2e] Database provisioning is unavailable at ${apiUrl}; skipping DB/FS E2E tests.`);
}
