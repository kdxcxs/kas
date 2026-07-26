/// <reference types="vitest/config" />

import { svelte } from '@sveltejs/vite-plugin-svelte';
import { defineConfig, loadEnv } from 'vite';

export default defineConfig(({ mode }) => {
  const env = loadEnv(mode, process.cwd(), '');
  const kasTarget = env.KAS_API_URL || 'http://127.0.0.1:3000';
  const fileTarget = env.KAS_FILE_API_URL || 'http://127.0.0.1:3001';

  return {
    plugins: [svelte()],
    server: {
      host: '127.0.0.1',
      port: 5173,
      proxy: {
        '/api': {
          target: kasTarget,
          changeOrigin: true,
          rewrite: (path) => path.replace(/^\/api/, '')
        },
        '/files-api': {
          target: fileTarget,
          changeOrigin: true,
          rewrite: (path) => path.replace(/^\/files-api/, '')
        }
      }
    },
    test: {
      include: ['src/**/*.test.ts']
    }
  };
});
