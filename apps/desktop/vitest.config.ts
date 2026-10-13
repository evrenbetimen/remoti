import { defineConfig } from 'vitest/config';

export default defineConfig({
  test: {
    root: '.',
    include: ['electron/**/*.test.ts', 'src/**/*.test.ts'],
    environment: 'node',
  },
});
