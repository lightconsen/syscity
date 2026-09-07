import { defineConfig, type Plugin } from 'vite'
import react from '@vitejs/plugin-react'
import { VitePWA } from 'vite-plugin-pwa'
import path from 'path'
import pkg from './package.json'

/** Substitute the {VERSION} placeholder in index.html (page title) with the
 *  package version. The gateway does the same replacement when serving the
 *  built UI; this keeps the dev server and prod build consistent. */
const replaceVersionPlaceholder: Plugin = {
  name: 'replace-version-placeholder',
  transformIndexHtml(html: string) {
    return html.replace('{VERSION}', pkg.version)
  },
}

export default defineConfig({
  plugins: [replaceVersionPlaceholder, react(), VitePWA({
      registerType: 'autoUpdate',
      workbox: {
        globPatterns: ['**/*.{js,css,html,png,svg,ico}'],
        maximumFileSizeToCacheInBytes: 3 * 1024 * 1024,
      },
      manifest: {
        name: 'Syscity Agent',
        short_name: 'Syscity',
        description: 'AI-powered chat interface for Syscity',
        theme_color: '#B22AC2',
        background_color: '#ffffff',
        display: 'standalone',
        orientation: 'portrait',
        scope: '/',
        start_url: '/',
        icons: [
          {
            src: '/syscity.png',
            sizes: '512x512',
            type: 'image/png',
            purpose: 'any maskable',
          },
        ],
      },
    })],
  base: './',
  resolve: {
    alias: {
      '@': path.resolve(__dirname, './src'),
    },
  },
  build: {
    outDir: '../dist',
    emptyOutDir: true,
  },
})
