// Test runner settings for the client-contract project (tests/contract): CI-only tooling that starts the built binary
// and checks it against the published client package. Nothing in this folder ships in the image.
import { defineConfig } from 'vitest/config'

export default defineConfig({
  test: {
    // The tests start child processes and make HTTP requests, so no browser environment is needed.
    environment: 'node',
    // Only the contract test file runs.
    include: ['*.test.ts'],
    // A request may wait for the unreachable database pool (5 s) before it answers, so tests get 20 s.
    testTimeout: 20_000,
    // Starting the two processes in `beforeAll` is allowed up to a minute.
    hookTimeout: 60_000,
  },
})
