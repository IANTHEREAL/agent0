import { createDb9Client } from '../src/client';

async function main() {
  const baseUrl = process.env.DB9_API_URL || 'http://localhost:8090/api';
  const client = createDb9Client({ baseUrl });

  let failures = 0;
  let dbId: string | null = null;

  try {
    try {
      const db = await client.databases.create({ name: 'fs-api-test' });
      dbId = db.id;
      console.log('✓ Create database');
    } catch (err) {
      console.log(`✗ Create database: ${err}`);
      failures++;
      return;
    }

    try {
      await client.fs.write(dbId, '/hello.txt', 'Hello from get-db9 fs API test!');
      console.log('✓ Write file');
    } catch (err) {
      console.log(`✗ Write file: ${err}`);
      failures++;
    }

    try {
      const content = await client.fs.read(dbId, '/hello.txt');
      if (content === 'Hello from get-db9 fs API test!') {
        console.log('✓ Read file (content matches)');
      } else {
        console.log(`✗ Read file: content mismatch, got "${content}"`);
        failures++;
      }
    } catch (err) {
      console.log(`✗ Read file: ${err}`);
      failures++;
    }

    try {
      const stat = await client.fs.stat(dbId, '/hello.txt');
      if (stat.file_type === 'regular' && stat.size > 0) {
        console.log(`✓ Stat file (file_type=regular, size=${stat.size})`);
      } else {
        console.log(`✗ Stat file: file_type=${stat.file_type}, size=${stat.size}`);
        failures++;
      }
    } catch (err) {
      console.log(`✗ Stat file: ${err}`);
      failures++;
    }

    try {
      const files = await client.fs.list(dbId, '/');
      const found = files.some((f) => f.path === '/hello.txt');
      if (found) {
        console.log('✓ List root (contains /hello.txt)');
      } else {
        console.log(`✗ List root: /hello.txt not found. Files: ${files.map((f) => f.path).join(', ')}`);
        failures++;
      }
    } catch (err) {
      console.log(`✗ List root: ${err}`);
      failures++;
    }

    try {
      await client.fs.mkdir(dbId, '/testdir');
      console.log('✓ Mkdir /testdir');
    } catch (err) {
      console.log(`✗ Mkdir /testdir: ${err}`);
      failures++;
    }

    try {
      await client.fs.write(dbId, '/testdir/hello.txt', 'Hello from subdirectory');
      console.log('✓ Write file to /testdir/hello.txt');
    } catch (err) {
      console.log(`✗ Write file to /testdir/hello.txt: ${err}`);
      failures++;
    }

    try {
      const content = await client.fs.read(dbId, '/testdir/hello.txt');
      if (content === 'Hello from subdirectory') {
        console.log('✓ Read file from /testdir/hello.txt (content matches)');
      } else {
        console.log(`✗ Read file from /testdir/hello.txt: content mismatch, got "${content}"`);
        failures++;
      }
    } catch (err) {
      console.log(`✗ Read file from /testdir/hello.txt: ${err}`);
      failures++;
    }

    try {
      const files = await client.fs.list(dbId, '/testdir/');
      const found = files.some((f) => f.path.includes('hello.txt'));
      if (found) {
        console.log('✓ List /testdir/ (contains hello.txt)');
      } else {
        console.log(`✗ List /testdir/: hello.txt not found. Files: ${files.map((f) => f.path).join(', ')}`);
        failures++;
      }
    } catch (err) {
      console.log(`✗ List /testdir/: ${err}`);
      failures++;
    }

    try {
      await client.fs.remove(dbId, '/testdir/hello.txt');
      console.log('✓ Remove /testdir/hello.txt');
    } catch (err) {
      console.log(`✗ Remove /testdir/hello.txt: ${err}`);
      failures++;
    }

    try {
      await client.fs.remove(dbId, '/testdir');
      console.log('✓ Remove /testdir');
    } catch (err) {
      console.log(`✗ Remove /testdir: ${err}`);
      failures++;
    }

    try {
      const files = await client.fs.list(dbId, '/');
      const hasTestdir = files.some((f) => f.path === '/testdir');
      if (!hasTestdir) {
        console.log('✓ Verify root is clean (no /testdir)');
      } else {
        console.log(`✗ Verify root is clean: /testdir still exists`);
        failures++;
      }
    } catch (err) {
      console.log(`✗ Verify root is clean: ${err}`);
      failures++;
    }

    const totalTests = 14;
    const passed = totalTests - failures;
    console.log(`\n${passed}/${totalTests} passed`);
  } finally {
    if (dbId) {
      try {
        await client.databases.delete(dbId);
        console.log('✓ Cleanup: database deleted');
      } catch (err) {
        console.log(`✗ Cleanup: failed to delete database: ${err}`);
      }
    }
  }

  process.exit(failures > 0 ? 1 : 0);
}

main().catch(console.error);
