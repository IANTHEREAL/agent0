import { Db9Error } from './errors';

export type FetchFn = typeof globalThis.fetch;

// BodyInit type for raw request bodies (string, Blob, ArrayBuffer, etc.)
export type BodyInit = string | Blob | ArrayBuffer | FormData | URLSearchParams | ReadableStream<Uint8Array>;

export interface HttpClientOptions {
  baseUrl: string;
  fetch?: FetchFn;
  headers?: Record<string, string>;
  timeout?: number;       // Request timeout in ms (default: none)
  maxRetries?: number;    // Max retry attempts on 5xx/network errors (default: 0 = disabled)
  retryDelay?: number;    // Base delay in ms for exponential backoff (default: 1000)
}

export interface HttpClient {
  get<T>(path: string, params?: Record<string, string | undefined>): Promise<T>;
  post<T>(path: string, body?: unknown): Promise<T>;
  put<T>(path: string, body?: unknown): Promise<T>;
  del<T>(path: string): Promise<T>;
  getRaw(path: string, params?: Record<string, string | undefined>): Promise<Response>;
  putRaw(path: string, body: BodyInit, headers?: Record<string, string>): Promise<Response>;
  postRaw(path: string, body?: BodyInit, headers?: Record<string, string>): Promise<Response>;
  delRaw(path: string, params?: Record<string, string | undefined>): Promise<Response>;
}

function delay(ms: number): Promise<void> {
  return new Promise((resolve) => setTimeout(resolve, ms));
}

export function createHttpClient(options: HttpClientOptions): HttpClient {
  const fetchFn = options.fetch ?? globalThis.fetch;
  const baseUrl = options.baseUrl.replace(/\/$/, '');

  async function request<T>(
    method: string,
    path: string,
    body?: unknown,
    params?: Record<string, string | undefined>
  ): Promise<T> {
    let url = `${baseUrl}${path}`;

    if (params) {
      const searchParams = new URLSearchParams();
      for (const [key, value] of Object.entries(params)) {
        if (value !== undefined) searchParams.set(key, value);
      }
      const qs = searchParams.toString();
      if (qs) url += `?${qs}`;
    }

    const reqHeaders: Record<string, string> = {
      'Content-Type': 'application/json',
      ...options.headers,
    };

    const init: RequestInit = { method, headers: reqHeaders };
    if (body !== undefined) {
      init.body = JSON.stringify(body);
    }

    const maxAttempts = Math.min(options.maxRetries ?? 0, 3) + 1;
    const baseDelay = options.retryDelay ?? 1000;
    let lastError: unknown;

    for (let attempt = 0; attempt < maxAttempts; attempt++) {
      let timeoutId: ReturnType<typeof setTimeout> | undefined;
      try {
        const fetchInit = { ...init };
        if (options.timeout) {
          const controller = new AbortController();
          fetchInit.signal = controller.signal;
          timeoutId = setTimeout(() => controller.abort(), options.timeout);
        }

        const response = await fetchFn(url, fetchInit);
        if (timeoutId) clearTimeout(timeoutId);

        if (!response.ok) {
          if (response.status >= 500 && attempt < maxAttempts - 1) {
            lastError = await Db9Error.fromResponse(response);
            await delay(baseDelay * Math.pow(2, attempt));
            continue;
          }
          throw await Db9Error.fromResponse(response);
        }

        if (response.status === 204) return undefined as T;
        return response.json() as Promise<T>;
      } catch (err) {
        if (timeoutId) clearTimeout(timeoutId);
        if (err instanceof TypeError && attempt < maxAttempts - 1) {
          lastError = err;
          await delay(baseDelay * Math.pow(2, attempt));
          continue;
        }
        throw err;
      }
    }
    throw lastError;
  }

  async function requestRaw(
    method: string,
    path: string,
    body?: BodyInit,
    params?: Record<string, string | undefined>,
    customHeaders?: Record<string, string>
  ): Promise<Response> {
    let url = `${baseUrl}${path}`;

    if (params) {
      const searchParams = new URLSearchParams();
      for (const [key, value] of Object.entries(params)) {
        if (value !== undefined) searchParams.set(key, value);
      }
      const qs = searchParams.toString();
      if (qs) url += `?${qs}`;
    }

    const headers: Record<string, string> = {
      ...options.headers,
      ...customHeaders,
    };

    const fetchInit: RequestInit = { method, headers };
    if (body !== undefined) fetchInit.body = body;

    let timeoutId: ReturnType<typeof setTimeout> | undefined;
    if (options.timeout) {
      const controller = new AbortController();
      fetchInit.signal = controller.signal;
      timeoutId = setTimeout(() => controller.abort(), options.timeout);
    }

    try {
      const response = await fetchFn(url, fetchInit);
      if (timeoutId) clearTimeout(timeoutId);
      if (!response.ok) throw await Db9Error.fromResponse(response);
      return response;
    } catch (err) {
      if (timeoutId) clearTimeout(timeoutId);
      throw err;
    }
  }

  return {
    get: <T>(path: string, params?: Record<string, string | undefined>) =>
      request<T>('GET', path, undefined, params),
    post: <T>(path: string, body?: unknown) => request<T>('POST', path, body),
    put: <T>(path: string, body?: unknown) => request<T>('PUT', path, body),
    del: <T>(path: string) => request<T>('DELETE', path),
    getRaw: (path: string, params?: Record<string, string | undefined>) =>
      requestRaw('GET', path, undefined, params),
    putRaw: (path: string, body: BodyInit, headers?: Record<string, string>) =>
      requestRaw('PUT', path, body, undefined, headers),
    postRaw: (path: string, body?: BodyInit, headers?: Record<string, string>) =>
      requestRaw('POST', path, body, undefined, headers),
    delRaw: (path: string, params?: Record<string, string | undefined>) =>
      requestRaw('DELETE', path, undefined, params),
  };
}
