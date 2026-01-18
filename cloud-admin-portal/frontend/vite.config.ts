import { defineConfig } from 'vite'
import react from '@vitejs/plugin-react'
import path from 'path'

// https://vitejs.dev/config/
export default defineConfig({
  plugins: [react()],
  resolve: {
    alias: {
      '@': path.resolve(__dirname, './src'),
    },
  },
  // Pre-bundle heavy dependencies to avoid slow first load
  // See: https://vite.dev/guide/dep-pre-bundling
  optimizeDeps: {
    include: [
      // Icon library with 1000+ icons - major bottleneck
      'lucide-react',
      // Radix UI primitives
      '@radix-ui/react-dialog',
      '@radix-ui/react-dropdown-menu',
      '@radix-ui/react-label',
      '@radix-ui/react-slot',
      '@radix-ui/react-toast',
      // Styling utilities
      'class-variance-authority',
      'clsx',
      'tailwind-merge',
      // Data fetching
      '@tanstack/react-query',
      '@tanstack/react-table',
      // Forms
      'react-hook-form',
      '@hookform/resolvers',
      'zod',
      // Router
      'react-router-dom',
    ],
  },
  server: {
    port: 5173,
    allowedHosts: true,
    proxy: {
      '/api': {
        target: process.env.VITE_BACKEND_URL || 'http://localhost:8090',
        changeOrigin: true,
      },
    },
    // Pre-transform frequently used files on server start
    warmup: {
      clientFiles: [
        './src/main.tsx',
        './src/App.tsx',
        './src/pages/TenantsPage.tsx',
        './src/pages/TenantDetailPage.tsx',
        './src/components/layout/AppLayout.tsx',
        './src/components/ui/*.tsx',
      ],
    },
  },
})
