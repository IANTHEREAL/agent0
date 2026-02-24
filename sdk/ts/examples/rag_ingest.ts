#!/usr/bin/env npx tsx
/**
 * RAG Ingest Pipeline — reads files from fs9, chunks, embeds, stores in db9-server
 *
 * Usage: npx tsx examples/rag_ingest.ts
 *
 * Environment:
 *   DB9_API_URL          — API endpoint (default: https://db9.shared.aws.tidbcloud.com/api)
 *   DB9_DATABASE_ID      — Target database ID (REQUIRED)
 *   EMBEDDING_API_URL    — Embedding API URL (default: https://api.openai.com/v1/embeddings)
 *   EMBEDDING_API_KEY    — API key for embedding service (REQUIRED)
 *   EMBEDDING_MODEL      — Model name (default: text-embedding-3-small)
 *   EMBEDDING_DIMENSIONS — Vector dimensions (default: 1536)
 *   CHUNK_SIZE           — Characters per chunk (default: 1000)
 *   CHUNK_OVERLAP        — Overlap between chunks (default: 200)
 *   SOURCE_PATH          — fs9 directory to ingest (default: /uploads/)
 *   DB9_TOKEN            — Optional auth token (auto-registers if omitted)
 */

import { createDb9Client } from '../src/client';
import pg from 'pg';
const { Pool } = pg;

// ── 1. Configuration ──────────────────────────────────────────────

interface Config {
  apiUrl: string;
  databaseId: string;
  token?: string;
  embeddingApiUrl: string;
  embeddingApiKey: string;
  embeddingModel: string;
  embeddingDimensions: number;
  chunkSize: number;
  chunkOverlap: number;
  sourcePath: string;
}

function loadConfig(): Config {
  const databaseId = process.env.DB9_DATABASE_ID;
  if (!databaseId) {
    console.error('ERROR: DB9_DATABASE_ID is required');
    process.exit(1);
  }

  const embeddingApiKey = process.env.EMBEDDING_API_KEY;
  if (!embeddingApiKey) {
    console.error('ERROR: EMBEDDING_API_KEY is required');
    process.exit(1);
  }

  return {
    apiUrl: process.env.DB9_API_URL ?? 'https://db9.shared.aws.tidbcloud.com/api',
    databaseId,
    token: process.env.DB9_TOKEN,
    embeddingApiUrl:
      process.env.EMBEDDING_API_URL ?? 'https://api.openai.com/v1/embeddings',
    embeddingApiKey,
    embeddingModel: process.env.EMBEDDING_MODEL ?? 'text-embedding-3-small',
    embeddingDimensions: parseInt(process.env.EMBEDDING_DIMENSIONS ?? '1536', 10),
    chunkSize: parseInt(process.env.CHUNK_SIZE ?? '1000', 10),
    chunkOverlap: parseInt(process.env.CHUNK_OVERLAP ?? '200', 10),
    sourcePath: process.env.SOURCE_PATH ?? '/uploads/',
  };
}

const config = loadConfig();

// ── 2. SDK + pg setup ─────────────────────────────────────────────

const db9 = createDb9Client({
  baseUrl: config.apiUrl,
  token: config.token,
});

async function createPool(): Promise<InstanceType<typeof Pool>> {
  const dbInfo = await db9.databases.get(config.databaseId);
  if (!dbInfo.connection_string) {
    throw new Error(
      `Database ${config.databaseId} has no connection_string. ` +
        'Is the database active?'
    );
  }
  console.log(`[db] Connecting to database: ${config.databaseId}`);
  return new Pool({ connectionString: dbInfo.connection_string });
}

// ── 3. Schema creation ───────────────────────────────────────────

async function ensureSchema(pool: InstanceType<typeof Pool>): Promise<void> {
  const ddl = `
    CREATE TABLE IF NOT EXISTS rag_chunks (
      id SERIAL PRIMARY KEY,
      doc_path TEXT NOT NULL,
      chunk_index INTEGER NOT NULL,
      chunk_text TEXT NOT NULL,
      embedding VECTOR(${config.embeddingDimensions}),
      search_vector TSVECTOR,
      created_at TIMESTAMP DEFAULT NOW(),
      UNIQUE(doc_path, chunk_index)
    );
  `;
  await pool.query(ddl);
  console.log('[schema] rag_chunks table ready');
}

// ── 4. Text chunking ─────────────────────────────────────────────

function chunkText(text: string, size: number, overlap: number): string[] {
  const chunks: string[] = [];
  let start = 0;
  while (start < text.length) {
    chunks.push(text.slice(start, start + size));
    start += size - overlap;
  }
  return chunks;
}

// ── 5. Embedding generation ──────────────────────────────────────

const EMBEDDING_BATCH_SIZE = 128;
const MAX_RETRIES = 3;

async function getEmbeddingsBatch(texts: string[]): Promise<number[][]> {
  let attempt = 0;

  while (true) {
    const response = await fetch(config.embeddingApiUrl, {
      method: 'POST',
      headers: {
        'Content-Type': 'application/json',
        Authorization: `Bearer ${config.embeddingApiKey}`,
      },
      body: JSON.stringify({
        input: texts,
        model: config.embeddingModel,
        ...(config.embeddingDimensions
          ? { dimensions: config.embeddingDimensions }
          : {}),
      }),
    });

    if (response.status === 429 && attempt < MAX_RETRIES) {
      const waitMs = Math.pow(2, attempt) * 1000;
      console.warn(`[embed] Rate limited, retrying in ${waitMs}ms...`);
      await new Promise((r) => setTimeout(r, waitMs));
      attempt++;
      continue;
    }

    if (!response.ok) {
      throw new Error(
        `Embedding API error: ${response.status} ${await response.text()}`
      );
    }

    const data = (await response.json()) as {
      data: { embedding: number[] }[];
    };
    return data.data.map((d) => d.embedding);
  }
}

async function getEmbeddings(texts: string[]): Promise<number[][]> {
  const allEmbeddings: number[][] = [];

  for (let i = 0; i < texts.length; i += EMBEDDING_BATCH_SIZE) {
    const batch = texts.slice(i, i + EMBEDDING_BATCH_SIZE);
    console.log(
      `[embed] Batch ${Math.floor(i / EMBEDDING_BATCH_SIZE) + 1}/${Math.ceil(texts.length / EMBEDDING_BATCH_SIZE)} (${batch.length} texts)`
    );
    const embeddings = await getEmbeddingsBatch(batch);
    allEmbeddings.push(...embeddings);
  }

  return allEmbeddings;
}

// ── 6. Batch insert ──────────────────────────────────────────────

const INSERT_BATCH_SIZE = 100;

async function insertChunks(
  pool: InstanceType<typeof Pool>,
  docPath: string,
  chunks: string[],
  embeddings: number[][]
): Promise<void> {
  for (let i = 0; i < chunks.length; i += INSERT_BATCH_SIZE) {
    const batchEnd = Math.min(i + INSERT_BATCH_SIZE, chunks.length);
    const batchChunks = chunks.slice(i, batchEnd);
    const batchEmbeddings = embeddings.slice(i, batchEnd);

    const values: unknown[] = [];
    const placeholders: string[] = [];

    for (let j = 0; j < batchChunks.length; j++) {
      const offset = j * 5;
      placeholders.push(
        `($${offset + 1}, $${offset + 2}, $${offset + 3}, $${offset + 4}::vector, to_tsvector('english', $${offset + 5}))`
      );
      values.push(
        docPath,
        i + j, // chunk_index
        batchChunks[j],
        JSON.stringify(batchEmbeddings[j]), // vector as JSON array string
        batchChunks[j] // for tsvector
      );
    }

    const sql = `
      INSERT INTO rag_chunks (doc_path, chunk_index, chunk_text, embedding, search_vector)
      VALUES ${placeholders.join(', ')}
      ON CONFLICT (doc_path, chunk_index) DO UPDATE SET
        chunk_text = EXCLUDED.chunk_text,
        embedding = EXCLUDED.embedding,
        search_vector = EXCLUDED.search_vector
    `;
    await pool.query(sql, values);

    console.log(
      `[insert] Rows ${i + 1}–${batchEnd} of ${chunks.length} for ${docPath}`
    );
  }
}

// ── 7. Index creation ────────────────────────────────────────────

async function createIndexes(pool: InstanceType<typeof Pool>): Promise<void> {
  console.log('[index] Creating GIN full-text search index...');
  await pool.query(`
    CREATE INDEX CONCURRENTLY IF NOT EXISTS idx_rag_fts
    ON rag_chunks USING gin(search_vector)
  `);
  console.log('[index] idx_rag_fts ready');
}

// ── 8. Example hybrid query ──────────────────────────────────────

async function exampleHybridSearch(
  pool: InstanceType<typeof Pool>
): Promise<void> {
  console.log('\n── Example Hybrid Search Query ──────────────────');
  console.log('The following SQL combines vector similarity with FTS:');
  console.log(`
  SELECT doc_path, chunk_text,
    embedding <=> $1::vector AS distance
  FROM rag_chunks
  WHERE search_vector @@ plainto_tsquery('english', $2)
  ORDER BY distance
  LIMIT 5
  `);
  console.log(
    'To run: pass a query embedding as $1 and search text as $2.\n'
  );

    const result = await pool.query(`
    SELECT doc_path, chunk_index,
      LEFT(chunk_text, 80) AS preview
    FROM rag_chunks
    ORDER BY id
    LIMIT 5
  `);

  if (result.rows.length > 0) {
    console.log('Sample stored chunks:');
    for (const row of result.rows) {
      console.log(
        `  [${row.doc_path}#${row.chunk_index}] ${row.preview}...`
      );
    }
  }
}

// ── 9. Main ──────────────────────────────────────────────────────

async function main(): Promise<void> {
  console.log('╔══════════════════════════════════════════╗');
  console.log('║     RAG Ingest Pipeline — db9-server/fs9    ║');
  console.log('╚══════════════════════════════════════════╝\n');

  const pool = await createPool();

  try {
    await ensureSchema(pool);

    console.log(`\n[fs9] Listing files in ${config.sourcePath}...`);
    const entries = await db9.fs.list(config.databaseId, config.sourcePath);
    const files = entries.filter((e) => e.type === 'file');

    if (files.length === 0) {
      console.log('[fs9] No files found. Upload text files to fs9 first.');
      console.log(`  Example: db9 fs upload ${config.databaseId} ./myfile.txt /uploads/myfile.txt`);
      return;
    }

    console.log(`[fs9] Found ${files.length} file(s)\n`);

    let totalChunks = 0;

    for (const file of files) {
      console.log(`── Processing: ${file.path} (${file.size} bytes) ──`);

      const content = await db9.fs.read(config.databaseId, file.path);
      if (!content || content.trim().length === 0) {
        console.log(`  [skip] Empty file`);
        continue;
      }

      const chunks = chunkText(content, config.chunkSize, config.chunkOverlap);
      console.log(`  [chunk] ${chunks.length} chunk(s)`);

      const embeddings = await getEmbeddings(chunks);
      console.log(`  [embed] ${embeddings.length} embedding(s) generated`);

      await insertChunks(pool, file.path, chunks, embeddings);
      totalChunks += chunks.length;

      console.log(`  [done] ${chunks.length} chunks stored\n`);
    }

    await createIndexes(pool);
    await exampleHybridSearch(pool);

    console.log('\n── Summary ─────────────────────────────────');
    console.log(`  Files processed: ${files.length}`);
    console.log(`  Chunks created:  ${totalChunks}`);
    console.log(`  Vector dims:     ${config.embeddingDimensions}`);
    console.log(`  Indexes:         idx_rag_fts (GIN)`);
    console.log('  Done!\n');
  } finally {
    await pool.end();
  }
}

main().catch((err) => {
  console.error('Fatal error:', err);
  process.exit(1);
});
