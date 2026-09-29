// Offline support. The app shell and the track / tag lists come fresh from the Pi when it answers (4 s), else from
// the last copy; downloaded tracks (the "tracks" cache, filled by ui/src/offline.rs) play from the phone.
// Anything answered from a cache is marked with x-mlm-offline so the app knows the Pi is out of reach.
const SHELL = ["/", "/mlm-ui.js", "/mlm-ui_bg.wasm", "/worklet.js", "/manifest.json", "/icon-192.png",
               "/icon-512.png", "/icon-mono.png", "/silence.wav", "/api/tracks", "/api/tags"];

self.addEventListener("install", e => {
  self.skipWaiting();
  e.waitUntil(caches.open("shell").then(c => c.addAll(SHELL)));
});
self.addEventListener("activate", e => e.waitUntil(self.clients.claim()));

const cached = async req => {
  const r = await caches.match(req);
  if (!r) return Response.error();
  const h = new Headers(r.headers);
  h.set("x-mlm-offline", "1");
  return new Response(r.body, { status: r.status, headers: h });
};
const timeout = ms => new Promise((_, no) => setTimeout(() => no(new Error("timeout")), ms));

self.addEventListener("fetch", e => {
  const req = e.request, path = new URL(req.url).pathname;
  if (path === "/api/tags/ops") { // the answer is the server's whole tags doc: keep it as the offline copy
    return e.respondWith(fetch(req).then(r => {
      if (r.ok) { const copy = r.clone(); e.waitUntil(caches.open("shell").then(c => c.put("/api/tags", copy))); }
      return r;
    }));
  }
  if (req.method !== "GET" || new URL(req.url).origin !== location.origin) return;
  if (/^\/api\/tracks\/\d+\/pcm$/.test(path)) { // downloaded: from the phone, else streamed from the Pi
    return e.respondWith(caches.open("tracks").then(c => c.match(path)).then(r => r || fetch(req)));
  }
  if (SHELL.includes(path)) { // network first, refreshing the copy
    return e.respondWith(Promise.race([fetch(req), timeout(4000)]).then(r => {
      if (r.ok) { const copy = r.clone(); e.waitUntil(caches.open("shell").then(c => c.put(req, copy))); }
      return r;
    }).catch(() => cached(req)));
  }
  if (/^\/api\/tracks\/\d+\/art$/.test(path)) { // cover art: downloaded tracks keep theirs for the lock screen
    return e.respondWith(fetch(req).catch(() => cached(req)));
  }
});
