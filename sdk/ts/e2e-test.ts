import {
  createDb9Client,
  instantDatabase,
  MemoryCredentialStore,
} from './src/index';

const BASE_URL = 'https://db9.shared.aws.tidbcloud.com/api';
const SEEDED_MESSAGE = 'hello from get-db9';

type StepRecord = {
  name: string;
  ok: boolean;
};

function pass(message: string): void {
  console.log(`✅ ${message}`);
}

function fail(message: string): void {
  console.log(`❌ ${message}`);
}

function assert(condition: unknown, message: string): asserts condition {
  if (!condition) {
    throw new Error(message);
  }
}

async function main(): Promise<void> {
  console.log('🧪 get-db9 SDK E2E Test');
  console.log('========================');

  const steps: StepRecord[] = [];
  const credentialStore = new MemoryCredentialStore();
  const dbName = `sdk-test-${Date.now()}`;
  const seedSql = [
    'CREATE TABLE sdk_test (id SERIAL PRIMARY KEY, msg TEXT);',
    `INSERT INTO sdk_test (msg) VALUES ('${SEEDED_MESSAGE}');`,
  ].join(' ');

  let databaseId: string | undefined;
  let token: string | undefined;

  try {
    const created = await instantDatabase({
      baseUrl: BASE_URL,
      name: dbName,
      seed: seedSql,
      credentialStore,
    });

    assert(created.databaseId.length > 0, 'instantDatabase missing databaseId');
    assert(
      created.connectionString.length > 0,
      'instantDatabase missing connectionString'
    );
    assert(created.adminPassword.length > 0, 'instantDatabase missing adminPassword');
    databaseId = created.databaseId;
    steps.push({ name: 'instantDatabase()', ok: true });
    pass(`instantDatabase() — created DB ${dbName} (id: ${created.databaseId})`);
    pass(`Connection string: ${created.connectionString}`);

    const credentials = await credentialStore.load();
    assert(credentials?.token, 'No token stored in MemoryCredentialStore');
    token = credentials.token;
    const client = createDb9Client({
      baseUrl: BASE_URL,
      token,
    });
    steps.push({ name: 'createDb9Client()', ok: true });
    pass('createDb9Client() — authenticated with stored token');

    const databases = await client.databases.list();
    assert(
      databases.some((db) => db.id === databaseId || db.name === dbName),
      `Created database ${dbName} not found in databases.list()`
    );
    steps.push({ name: 'databases.list()', ok: true });
    pass(`databases.list() — found ${databases.length} database(s)`);

    const selectOne = await client.databases.sql(databaseId, 'SELECT 1 AS test');
    const testColumnIndex = selectOne.columns.findIndex((column) => column.name === 'test');
    assert(testColumnIndex >= 0, 'SELECT 1 result missing test column');
    assert(selectOne.rows.length > 0, 'SELECT 1 returned no rows');
    assert(selectOne.rows[0][testColumnIndex] === 1, 'SELECT 1 result value mismatch');
    steps.push({ name: 'databases.sql() SELECT 1', ok: true });
    pass('databases.sql() — SELECT 1 returned expected value');

    const seeded = await client.databases.sql(
      databaseId,
      'SELECT id, msg FROM sdk_test ORDER BY id'
    );
    const idColumnIndex = seeded.columns.findIndex((column) => column.name === 'id');
    const msgColumnIndex = seeded.columns.findIndex((column) => column.name === 'msg');
    assert(idColumnIndex >= 0, 'Seed query result missing id column');
    assert(msgColumnIndex >= 0, 'Seed query result missing msg column');
    assert(seeded.rows.length > 0, 'Seed query returned no rows');
    assert(seeded.rows[0][idColumnIndex] === 1, 'Seed query id mismatch');
    assert(
      seeded.rows[0][msgColumnIndex] === SEEDED_MESSAGE,
      'Seed query message mismatch'
    );
    steps.push({ name: 'databases.sql() seeded table', ok: true });
    pass(
      `databases.sql() — SELECT * FROM sdk_test returned ${JSON.stringify(seeded.rows)}`
    );
  } catch (error) {
    fail('Test flow failed');
    throw error;
  } finally {
    if (databaseId && token) {
      try {
        const cleanupClient = createDb9Client({ baseUrl: BASE_URL, token });
        await cleanupClient.databases.delete(databaseId);
        steps.push({ name: 'databases.delete()', ok: true });
        pass(`databases.delete() — cleaned up ${databaseId}`);
      } catch (cleanupError) {
        steps.push({ name: 'databases.delete()', ok: false });
        fail(`databases.delete() cleanup failed for ${databaseId}`);
        console.error(cleanupError);
      }
    } else {
      steps.push({ name: 'databases.delete()', ok: false });
      fail('Cleanup skipped: missing databaseId or token');
    }

    console.log('========================');
    const passed = steps.filter((step) => step.ok).length;
    const failed = steps.length - passed;
    if (failed === 0) {
      console.log(`All ${passed} tests passed!`);
    } else {
      console.log(`${passed} passed, ${failed} failed.`);
      process.exitCode = 1;
    }
  }
}

main().catch((error) => {
  console.error('');
  console.error('Full error:');
  console.error(error);
  process.exit(1);
});
