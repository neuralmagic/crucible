/// <reference types="vitest/config" />
import { defineConfig } from 'vite';
import react from '@vitejs/plugin-react';
import tailwindcss from '@tailwindcss/vite';

export default defineConfig({
  plugins: [react(), tailwindcss()],
  build: {
    outDir: 'dist',
    emptyOutDir: true,
  },
  server: {
    proxy: {
      '/api': {
        target: process.env.VITE_API_TARGET ?? 'http://127.0.0.1:8899',
        // Dev-server identity. A native-auth controller (ADR-0029) wants a bearer:
        // set VITE_API_BEARER to a minted API key. The x-auth-request-user header
        // only works against a controller still behind oauth2-proxy or one that
        // honors break-glass identity headers; set VITE_DEV_USER to test as
        // someone else there, with the login in CONTROLLER_ADMINS for the admin
        // surfaces, and VITE_DEV_GROUPS (comma-separated) to carry group claims.
        headers: {
          'x-auth-request-user': process.env.VITE_DEV_USER ?? 'dev',
          ...(process.env.VITE_DEV_GROUPS !== undefined && {
            'x-auth-request-groups': process.env.VITE_DEV_GROUPS,
          }),
          ...(process.env.VITE_API_BEARER !== undefined && {
            Authorization: `Bearer ${process.env.VITE_API_BEARER}`,
          }),
        },
      },
    },
  },
  test: {
    exclude: ['e2e/**', 'node_modules/**', 'dist/**'],
  },
});
