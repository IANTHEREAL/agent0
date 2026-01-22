import 'pg';

declare module 'pg' {
  export interface QueryConfig<I = any[]> {
    queryMode?: 'extended' | 'simple';
  }
}

