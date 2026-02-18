import { Db9Error } from './errors';

export type FetchFn = typeof globalThis.fetch;

export interface HttpClientOptions {
  baseUrl: string;
  fetch?: FetchFn;
  headers?: Record<string, string>;
}

export interface HttpClient {
  get<T>(path: string, params?: Record<string, string | undefined>): Promise<T>;
  post<T>(path: string, body?: unknown): Promise<T>;
  put<T>(path: string, body?: unknown): Promise<T>;
  del<T>(path: string): Promise<T>;
}

export function createHttpClient(options: HttpClientOptions): HttpClient {
  const fetchFn = options.fetch ?? globalThis.fetch;
  const baseUrl = options.baseUrl.replace(/\/$/, ''); // strip trailing slash

  async function request<T>(
    method: string,
    path: string,
    body?: unknown,
    params?: Record<string, string | undefined>
  ): Promise<T> {
    let url = `${baseUrl}${path}`;

    // Append query params for GET requests
    if (params) {
      const searchParams = new URLSearchParams();
      for (const [key, value] of Object.entries(params)) {
        if (value !== undefined) {
          searchParams.set(key, value);
        }
      }
      const qs = searchParams.toString();
      if (qs) url += `?${qs}`;
    }

    const headers: Record<string, string> = {
      'Content-Type': 'application/json',
      ...options.headers,
    };

    const init: RequestInit = { method, headers };
    if (body !== undefined) {
      init.body = JSON.stringify(body);
    }

    const response = await fetchFn(url, init);

    if (!response.ok) {
      throw await Db9Error.fromResponse(response);
    }

    // Handle 204 No Content
    if (response.status === 204) {
      return undefined as T;
    }

    return response.json() as Promise<T>;
  }

  return {
    get: <T>(path: string, params?: Record<string, string | undefined>) =>
      request<T>('GET', path, undefined, params),
    post: <T>(path: string, body?: unknown) => request<T>('POST', path, body),
    put: <T>(path: string, body?: unknown) => request<T>('PUT', path, body),
    del: <T>(path: string) => request<T>('DELETE', path),
  };
}
