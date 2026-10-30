// Client contract: the published @meddleware/content-hub-client's route tables and golden vectors against the
// built `content-hub` binary.
//
//   HUB_BIN=../../target/release/content-hub VECTORS=../../vectors.json npm test
//
// - every public route of the client answers on `public` and is absent (404) on `admin`;
// - every admin route of the client answers on `admin` and is absent (404) on `public`;
// - this repository's vectors.json is byte-for-byte the published one (D84: the client is its home).
//
// No database is needed: both processes start with a pool that cannot connect, so "answers" means anything but
// 404 (an admin route without an Access token answers 401; a public data route answers 503).
import { afterAll, beforeAll, describe, expect, it } from 'vitest'
import { spawn, type ChildProcess } from 'node:child_process'
import { readFileSync } from 'node:fs'
import { createRequire } from 'node:module'
import { resolve } from 'node:path'
import { ADMIN_ROUTES, PUBLIC_ROUTES, type Route } from '@meddleware/content-hub-client'

const HUB_BIN = resolve(process.env.HUB_BIN ?? '../../target/release/content-hub')
const VECTORS = resolve(process.env.VECTORS ?? '../../vectors.json')
const base = 18000 + Math.floor(Math.random() * 1000)
const PUBLIC = `http://127.0.0.1:${base}`
const ADMIN = `http://127.0.0.1:${base + 1}`

const procs: ChildProcess[] = []

function start(sub: 'public' | 'admin', port: number): ChildProcess {
  return spawn(HUB_BIN, [sub], {
    env: {
      // Unreachable on purpose (port 1 refuses); the probes and routes under test never query it.
      DATABASE_URL: 'postgres://nobody:nothing@127.0.0.1:1/none',
      ALLOW_INSECURE_DATABASE: '1',
      LISTEN_ADDR: `127.0.0.1:${port}`,
      RUST_LOG: 'warn',
      // Production settings with example values: nothing is fetched, as no request carries a token.
      ACCESS_TEAM_DOMAIN: 'https://team.example.org',
      ACCESS_AUD: 'contract-test',
      ADMIN_ORIGIN: 'https://hub.example.org',
      KETO_READ_URL: 'http://127.0.0.1:1',
      HOME_PAGE_URL: 'https://roadmap.example.org',
      FEEDS_BASE_URL: 'https://feeds.example.org',
    },
    stdio: 'inherit',
  })
}

async function waitUp(url: string): Promise<void> {
  for (let i = 0; i < 100; i++) {
    try {
      if ((await fetch(`${url}/healthz`)).ok) return
    } catch {
      /* not listening yet */
    }
    await new Promise((r) => setTimeout(r, 100))
  }
  throw new Error(`${url} did not start`)
}

const concrete = (path: string) => path.replace(/\{[^}]+\}/g, '1')

async function statusOf(baseUrl: string, r: Route): Promise<number> {
  return (await fetch(`${baseUrl}${concrete(r.path)}`, { method: r.method })).status
}

// In parallel: a data route waits for the unreachable pool (5 s) before answering 503 (so these tests allow 30 s).
async function expectStatuses(baseUrl: string, routes: readonly Route[], absent: boolean): Promise<void> {
  const got = await Promise.all(routes.map((r) => statusOf(baseUrl, r)))
  routes.forEach((r, i) => {
    const e = expect(got[i], `${r.method} ${r.path}`)
    if (absent) e.toBe(404)
    else e.not.toBe(404)
  })
}

beforeAll(async () => {
  procs.push(start('public', base), start('admin', base + 1))
  await Promise.all([waitUp(PUBLIC), waitUp(ADMIN)])
})

afterAll(() => {
  for (const p of procs) p.kill('SIGTERM')
})

describe('route exposure', () => {
  it('public serves every public route of the client and no admin route', async () => {
    await expectStatuses(PUBLIC, PUBLIC_ROUTES, false)
    await expectStatuses(PUBLIC, ADMIN_ROUTES, true)
  }, 30_000)

  it('admin serves every admin route of the client and no public route', async () => {
    await expectStatuses(ADMIN, ADMIN_ROUTES, false)
    await expectStatuses(ADMIN, PUBLIC_ROUTES, true)
  }, 30_000)
})

describe('golden vectors', () => {
  it("this repository's copy is the published file", () => {
    const require = createRequire(import.meta.url)
    const published = readFileSync(require.resolve('@meddleware/content-hub-client/vectors.json'), 'utf8')
    expect(readFileSync(VECTORS, 'utf8')).toBe(published)
  })
})
