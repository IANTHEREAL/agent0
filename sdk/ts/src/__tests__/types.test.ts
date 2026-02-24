import { describe, it, expect } from 'vitest';
import type {
  CreateTokenRequest,
  CreateTokenResponse,
  SqlErrorDetail,
  Fs9EventEntry,
  Fs9EventOptions,
  SqlResult,
} from '../types';

describe('types compile checks', () => {
  describe('CreateTokenRequest', () => {
    it('accepts empty object', () => {
      const req: CreateTokenRequest = {};
      expect(req).toBeDefined();
    });

    it('accepts name field', () => {
      const req: CreateTokenRequest = { name: 'test' };
      expect(req).toBeDefined();
    });

    it('accepts expires_in_days field', () => {
      const req: CreateTokenRequest = { expires_in_days: 30 };
      expect(req).toBeDefined();
    });

    it('accepts both name and expires_in_days', () => {
      const req: CreateTokenRequest = { name: 'test', expires_in_days: 30 };
      expect(req).toBeDefined();
    });
  });

  describe('CreateTokenResponse', () => {
    it('has required fields id, token, created_at', () => {
      const resp: CreateTokenResponse = {
        id: 'token-123',
        name: 'my-token',
        token: 'secret-token-value',
        created_at: '2026-02-23T10:00:00Z',
      };
      expect(resp.id).toBeDefined();
      expect(resp.name).toBeDefined();
      expect(resp.token).toBeDefined();
      expect(resp.created_at).toBeDefined();
    });

    it('accepts optional expires_at field', () => {
      const resp: CreateTokenResponse = {
        id: 'token-123',
        name: 'my-token',
        token: 'secret-token-value',
        created_at: '2026-02-23T10:00:00Z',
        expires_at: '2026-03-25T10:00:00Z',
      };
      expect(resp.expires_at).toBeDefined();
    });
  });

  describe('SqlErrorDetail', () => {
    it('has required message field', () => {
      const err: SqlErrorDetail = { message: 'syntax error' };
      expect(err.message).toBeDefined();
    });

    it('accepts optional code field', () => {
      const err: SqlErrorDetail = {
        message: 'syntax error',
        code: '42601',
      };
      expect(err.code).toBeDefined();
    });

    it('accepts optional position field', () => {
      const err: SqlErrorDetail = {
        message: 'syntax error',
        position: 15,
      };
      expect(err.position).toBeDefined();
    });

    it('accepts optional hint field', () => {
      const err: SqlErrorDetail = {
        message: 'syntax error',
        hint: 'Check your SQL syntax',
      };
      expect(err.hint).toBeDefined();
    });

    it('accepts optional detail field', () => {
      const err: SqlErrorDetail = {
        message: 'syntax error',
        detail: 'Unexpected token at position 15',
      };
      expect(err.detail).toBeDefined();
    });

    it('accepts all optional fields together', () => {
      const err: SqlErrorDetail = {
        message: 'syntax error',
        code: '42601',
        position: 15,
        hint: 'Check your SQL syntax',
        detail: 'Unexpected token at position 15',
      };
      expect(err).toBeDefined();
    });
  });

  describe('Fs9EventEntry', () => {
    it('has required fields id, type, path, timestamp', () => {
      const entry: Fs9EventEntry = {
        id: 'evt-123',
        type: 'file_created',
        path: '/home/user/file.txt',
        timestamp: '2026-02-23T10:00:00Z',
      };
      expect(entry.id).toBeDefined();
      expect(entry.type).toBeDefined();
      expect(entry.path).toBeDefined();
      expect(entry.timestamp).toBeDefined();
    });

    it('accepts optional user_id field', () => {
      const entry: Fs9EventEntry = {
        id: 'evt-123',
        type: 'file_created',
        path: '/home/user/file.txt',
        timestamp: '2026-02-23T10:00:00Z',
        user_id: 'user-456',
      };
      expect(entry.user_id).toBeDefined();
    });

    it('accepts optional size field', () => {
      const entry: Fs9EventEntry = {
        id: 'evt-123',
        type: 'file_created',
        path: '/home/user/file.txt',
        timestamp: '2026-02-23T10:00:00Z',
        size: 1024,
      };
      expect(entry.size).toBeDefined();
    });

    it('accepts optional metadata field', () => {
      const entry: Fs9EventEntry = {
        id: 'evt-123',
        type: 'file_created',
        path: '/home/user/file.txt',
        timestamp: '2026-02-23T10:00:00Z',
        metadata: { owner: 'user', permissions: '644' },
      };
      expect(entry.metadata).toBeDefined();
    });

    it('accepts all optional fields together', () => {
      const entry: Fs9EventEntry = {
        id: 'evt-123',
        type: 'file_created',
        path: '/home/user/file.txt',
        timestamp: '2026-02-23T10:00:00Z',
        user_id: 'user-456',
        size: 1024,
        metadata: { owner: 'user', permissions: '644' },
      };
      expect(entry).toBeDefined();
    });
  });

  describe('Fs9EventOptions', () => {
    it('accepts empty object (all optional)', () => {
      const opts: Fs9EventOptions = {};
      expect(opts).toBeDefined();
    });

    it('accepts limit field', () => {
      const opts: Fs9EventOptions = { limit: 50 };
      expect(opts.limit).toBeDefined();
    });

    it('accepts offset field', () => {
      const opts: Fs9EventOptions = { offset: 100 };
      expect(opts.offset).toBeDefined();
    });

    it('accepts path field', () => {
      const opts: Fs9EventOptions = { path: '/home/user' };
      expect(opts.path).toBeDefined();
    });

    it('accepts type field', () => {
      const opts: Fs9EventOptions = { type: 'file_created' };
      expect(opts.type).toBeDefined();
    });

    it('accepts all optional fields together', () => {
      const opts: Fs9EventOptions = {
        limit: 50,
        offset: 100,
        path: '/home/user',
        type: 'file_created',
      };
      expect(opts).toBeDefined();
    });
  });

  describe('SqlResult.error backwards compatibility', () => {
    it('accepts string error', () => {
      const result: SqlResult = {
        columns: [],
        rows: [],
        row_count: 0,
        command: 'SELECT',
        error: 'syntax error',
      };
      expect(result.error).toBe('syntax error');
    });

    it('accepts SqlErrorDetail error', () => {
      const result: SqlResult = {
        columns: [],
        rows: [],
        row_count: 0,
        command: 'SELECT',
        error: {
          message: 'syntax error',
          code: '42601',
        },
      };
      expect(result.error).toBeDefined();
    });

    it('accepts undefined error', () => {
      const result: SqlResult = {
        columns: [],
        rows: [],
        row_count: 0,
        command: 'SELECT',
      };
      expect(result.error).toBeUndefined();
    });
  });
});
