import { createDb9Client } from './client';
import type { CredentialStore } from './credentials';
import type { FetchFn } from './http';
import type { DatabaseResponse } from './types';

export interface InstantDatabaseOptions {
  name?: string;
  baseUrl?: string;
  fetch?: FetchFn;
  credentialStore?: CredentialStore;
  seed?: string;
  seedFile?: string;
  timeout?: number;
  maxRetries?: number;
  retryDelay?: number;
}

export interface InstantDatabaseResult {
  databaseId: string;
  connectionString: string;
  adminUser: string;
  adminPassword: string;
  state: string;
  createdAt: string;
}

export async function instantDatabase(
  options: InstantDatabaseOptions = {}
): Promise<InstantDatabaseResult> {
  const dbName = options.name ?? 'default';

  const client = createDb9Client({
    baseUrl: options.baseUrl,
    fetch: options.fetch,
    credentialStore: options.credentialStore,
    timeout: options.timeout,
    maxRetries: options.maxRetries,
    retryDelay: options.retryDelay,
  });

  const existing = await client.databases.list();
  const found = existing.find((db: DatabaseResponse) => db.name === dbName);
  if (found) {
    return toResult(found);
  }

  const created = await client.databases.create({ name: dbName });

  if (options.seed) {
    await client.databases.sql(created.id, options.seed);
  } else if (options.seedFile) {
    await client.databases.sqlFile(created.id, options.seedFile);
  }

  return toResult(created);
}

function toResult(db: DatabaseResponse): InstantDatabaseResult {
  return {
    databaseId: db.id,
    connectionString: db.connection_string ?? '',
    adminUser: db.admin_user ?? '',
    adminPassword: db.admin_password ?? '',
    state: db.state,
    createdAt: db.created_at,
  };
}

export { createDb9Client } from './client';
export type { Db9ClientOptions, Db9Client } from './client';

export {
  Db9Error,
  Db9AuthError,
  Db9NotFoundError,
  Db9ConflictError,
} from './errors';

export {
  FileCredentialStore,
  MemoryCredentialStore,
  defaultCredentialStore,
} from './credentials';
export type { CredentialStore, Credentials } from './credentials';

export type { FetchFn, HttpClient, HttpClientOptions } from './http';

export type * from './types';
