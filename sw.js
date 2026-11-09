// Minimal service worker: required by Android Chrome/Samsung Internet to mint a WebAPK.
// Network-first for the page shell (falls back to cache offline); all other requests (Supabase, APIs) pass through untouched.
var CACHE='edge-shell-v1';
self.addEventListener('install',function(e){self.skipWaiting();});
self.addEventListener('activate',function(e){
  e.waitUntil(caches.keys().then(function(ks){return Promise.all(ks.filter(function(k){return k!==CACHE;}).map(function(k){return caches.delete(k);}));}).then(function(){return self.clients.claim();}));
});
self.addEventListener('fetch',function(e){
  var r=e.request;
  if(r.mode!=='navigate')return;
  e.respondWith(fetch(r).then(function(res){
    var copy=res.clone();caches.open(CACHE).then(function(c){c.put('./',copy);});
    return res;
  }).catch(function(){return caches.match('./');}));
});
