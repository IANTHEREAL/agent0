import { createCustomerClient, type CustomerClientOptions } from './customer';
import {
  defaultCredentialStore,
  type CredentialStore,
  type Credentials,
} from './credentials';
import type { FetchFn } from './http';
import type { DatabaseResponse } from './types';

export interface InstantDatabaseOptions {
  name?: string;
  baseUrl?: string;
  fetch?: FetchFn;
  credentialStore?: CredentialStore;
  seed?: string;
  seedFile?: string;
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
  const store = options.credentialStore ?? defaultCredentialStore();
  const dbName = options.name ?? 'default';

  const creds = await store.load();
  let token: string;

  if (creds?.token) {
    token = creds.token;
  } else {
    const publicClient = createCustomerClient({
      baseUrl: options.baseUrl,
      fetch: options.fetch,
    });
    const regResult = await publicClient.auth.anonymousRegister();
    token = regResult.token;

    await store.save({
      token: regResult.token,
      is_anonymous: regResult.is_anonymous,
      anonymous_id: regResult.anonymous_id,
      anonymous_secret: regResult.anonymous_secret,
    });
  }

  const client = createCustomerClient({
    baseUrl: options.baseUrl,
    fetch: options.fetch,
    token,
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

export { createCustomerClient } from './customer';
export type { CustomerClientOptions, CustomerClient } from './customer';

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
