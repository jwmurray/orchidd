// orchidd service worker.
//
// Two caching strategies, chosen to fit how this app is used from a phone:
//
//   app shell (/, icons, manifest)  cache-first  — opens instantly, works offline
//   /api/*                          network-first with cache fallback — always
//                                   prefers live data, but a cold open with no
//                                   signal still shows the last readings fetched
//
// Bump CACHE when the shell changes so old clients pick up the new HTML.
const CACHE = 'orchidd-v3';
const SHELL = ['/', '/manifest.webmanifest', '/icon-192.png', '/icon-512.png', '/apple-touch-icon.png'];

self.addEventListener('install', (e) => {
  e.waitUntil(caches.open(CACHE).then((c) => c.addAll(SHELL)).then(() => self.skipWaiting()));
});

self.addEventListener('activate', (e) => {
  e.waitUntil(
    caches.keys()
      .then((keys) => Promise.all(keys.filter((k) => k !== CACHE).map((k) => caches.delete(k))))
      .then(() => self.clients.claim())
  );
});

// Only responses that are actually our JSON are worth caching. Behind Cloudflare
// Access an expired session answers with an HTML login page (or a redirect to
// one); caching that would poison the app with a login page where data belongs.
function isUsableApiResponse(res) {
  if (!res || !res.ok || res.redirected) return false;
  const type = res.headers.get('content-type') || '';
  return type.includes('application/json');
}

self.addEventListener('fetch', (e) => {
  const req = e.request;
  if (req.method !== 'GET') return;

  const url = new URL(req.url);
  if (url.origin !== self.location.origin) return;

  if (url.pathname.startsWith('/api/')) {
    e.respondWith(
      fetch(req)
        .then((res) => {
          if (isUsableApiResponse(res)) {
            const copy = res.clone();
            caches.open(CACHE).then((c) => c.put(req, copy));
          }
          return res;
        })
        .catch(() => caches.match(req).then((hit) => hit || Response.error()))
    );
    return;
  }

  e.respondWith(
    caches.match(req).then((hit) => hit || fetch(req).then((res) => {
      if (res.ok && res.type === 'basic') {
        const copy = res.clone();
        caches.open(CACHE).then((c) => c.put(req, copy));
      }
      return res;
    }))
  );
});
