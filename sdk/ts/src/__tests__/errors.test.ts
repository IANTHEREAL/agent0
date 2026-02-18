import { describe, it, expect } from 'vitest';
import {
  Db9Error,
  Db9AuthError,
  Db9NotFoundError,
  Db9ConflictError,
} from '../errors';

describe('Db9Error', () => {
  it('should create a Db9Error with message and status code', () => {
    const error = new Db9Error('Test error', 500);
    expect(error.message).toBe('Test error');
    expect(error.statusCode).toBe(500);
    expect(error.name).toBe('Db9Error');
  });

  it('should create a Db9Error with response', () => {
    const response = new Response('test', { status: 500 });
    const error = new Db9Error('Test error', 500, response);
    expect(error.response).toBe(response);
  });

  it('should parse error response with message field', async () => {
    const response = new Response(JSON.stringify({ message: 'Not found' }), {
      status: 404,
      headers: { 'Content-Type': 'application/json' },
    });
    const error = await Db9Error.fromResponse(response);
    expect(error.message).toBe('Not found');
    expect(error.statusCode).toBe(404);
  });

  it('should return Db9AuthError for 401 status', async () => {
    const response = new Response(
      JSON.stringify({ message: 'Unauthorized' }),
      {
        status: 401,
        headers: { 'Content-Type': 'application/json' },
      }
    );
    const error = await Db9Error.fromResponse(response);
    expect(error).toBeInstanceOf(Db9AuthError);
    expect(error.statusCode).toBe(401);
    expect(error.name).toBe('Db9AuthError');
  });

  it('should return Db9NotFoundError for 404 status', async () => {
    const response = new Response(
      JSON.stringify({ message: 'Resource not found' }),
      {
        status: 404,
        headers: { 'Content-Type': 'application/json' },
      }
    );
    const error = await Db9Error.fromResponse(response);
    expect(error).toBeInstanceOf(Db9NotFoundError);
    expect(error.statusCode).toBe(404);
    expect(error.name).toBe('Db9NotFoundError');
  });

  it('should return Db9ConflictError for 409 status', async () => {
    const response = new Response(
      JSON.stringify({ message: 'Resource conflict' }),
      {
        status: 409,
        headers: { 'Content-Type': 'application/json' },
      }
    );
    const error = await Db9Error.fromResponse(response);
    expect(error).toBeInstanceOf(Db9ConflictError);
    expect(error.statusCode).toBe(409);
    expect(error.name).toBe('Db9ConflictError');
  });

  it('should return generic Db9Error for 500 status', async () => {
    const response = new Response(
      JSON.stringify({ message: 'Internal server error' }),
      {
        status: 500,
        headers: { 'Content-Type': 'application/json' },
      }
    );
    const error = await Db9Error.fromResponse(response);
    expect(error).toBeInstanceOf(Db9Error);
    expect(error.statusCode).toBe(500);
    expect(error.name).toBe('Db9Error');
  });

  it('should fall back to statusText when body is not JSON', async () => {
    const response = new Response('Internal Server Error', {
      status: 500,
      statusText: 'Internal Server Error',
    });
    const error = await Db9Error.fromResponse(response);
    expect(error.message).toBe('Internal Server Error');
  });

  it('should fall back to statusText when message field is missing', async () => {
    const response = new Response(JSON.stringify({ error: 'some error' }), {
      status: 500,
      statusText: 'Server Error',
      headers: { 'Content-Type': 'application/json' },
    });
    const error = await Db9Error.fromResponse(response);
    expect(error.message).toBe('Server Error');
  });

  it('should create Db9AuthError directly', () => {
    const error = new Db9AuthError('Auth failed');
    expect(error.statusCode).toBe(401);
    expect(error.name).toBe('Db9AuthError');
  });

  it('should create Db9NotFoundError directly', () => {
    const error = new Db9NotFoundError('Not found');
    expect(error.statusCode).toBe(404);
    expect(error.name).toBe('Db9NotFoundError');
  });

  it('should create Db9ConflictError directly', () => {
    const error = new Db9ConflictError('Conflict');
    expect(error.statusCode).toBe(409);
    expect(error.name).toBe('Db9ConflictError');
  });
});
