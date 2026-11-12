import React from 'react'
import ReactDOM from 'react-dom/client'
import './index.css'
import './i18n'
import App from './App'
import { ErrorBoundary } from './components/shared/ErrorBoundary'
import { installExternalLinkHandler } from './lib/external'

// The desktop shell's WebView cannot open a new window, so every outbound link
// has to go through the host instead (see lib/external.ts).
installExternalLinkHandler()

// The outermost boundary: without it a throw anywhere in render unmounts the
// tree and leaves a blank white window. The inner boundaries in App.tsx keep
// the damage narrower than this one can.
ReactDOM.createRoot(document.getElementById('root')!).render(
  <React.StrictMode>
    <ErrorBoundary area="app">
      <App />
    </ErrorBoundary>
  </React.StrictMode>,
)

// Register the service worker with **absolute** paths.
//
// vite-plugin-pwa's injected script uses `./sw.js` with scope `./`, which the
// browser resolves against the document's path. That is correct only at `/`:
// on `/cloud/login/callback` — the cloud OAuth return, a route this app really
// serves — it asks for `/cloud/login/sw.js` and gets a 404. The gateway already
// rewrites the document's own asset URLs to absolute for nested routes; the
// registration script was the one thing it could not rewrite, because it is a
// separate file.
//
// Production only, and only over HTTP: the dev server generates no `sw.js`,
// and the desktop shell loads the UI from a `tauri://` scheme, where service
// workers cannot register at all.
if (
  import.meta.env.PROD &&
  'serviceWorker' in navigator &&
  window.location.protocol.startsWith('http')
) {
  window.addEventListener('load', () => {
    navigator.serviceWorker
      .register('/sw.js', { scope: '/' })
      .catch((err) => console.warn('Service worker registration failed:', err))
  })
}
