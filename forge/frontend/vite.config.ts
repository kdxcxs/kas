import { svelte } from '@sveltejs/vite-plugin-svelte';
import { defineConfig, loadEnv } from 'vite';

export default defineConfig(({ mode }) => {
  const env = loadEnv(mode, process.cwd(), '');
  return {
    plugins: [svelte()],
    server: {
      host: '127.0.0.1',
      port: Number(env.KAS_FORGE_FRONTEND_PORT || 5173),
      proxy: {
        '/api': {
          target: env.KAS_API_URL || 'http://127.0.0.1:3000',
          changeOrigin: true,
          rewrite: (path) => path.replace(/^\/api/, '')
        },
        '/package-api': {
          target: env.KAS_PACKAGE_REQUEST_API || 'http://127.0.0.1:3004',
          changeOrigin: true,
          rewrite: (path) => path.replace(/^\/package-api/, '')
        }
      }
    }
  };
});

