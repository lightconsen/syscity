import React from 'react'
import ReactDOM from 'react-dom/client'
import './index.css'
import './i18n'
import App from './App'

ReactDOM.createRoot(document.getElementById('root')!).render(
  <React.StrictMode>
    <App />
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
