// Minimal service worker: required by Android Chrome/Samsung Internet to mint a WebAPK.
// Network-first for the page shell (falls back to cache offline); all other requests (Supabase, APIs) pass through untouched.
// Exception: the version-pinned chart library is cached on install and served cache-first, so the Chart tab never waits on unpkg after a cold start.
var CACHE='edge-shell-v1';
var LIB='https://unpkg.com/lightweight-charts@4.1.3/dist/lightweight-charts.standalone.production.js';
self.addEventListener('install',function(e){
  self.skipWaiting();
  e.waitUntil(fetch(LIB,{mode:'no-cors'}).then(function(res){return caches.open(CACHE).then(function(c){return c.put(LIB,res);});}).catch(function(){}));
});
self.addEventListener('activate',function(e){
  e.waitUntil(caches.keys().then(function(ks){return Promise.all(ks.filter(function(k){return k!==CACHE;}).map(function(k){return caches.delete(k);}));}).then(function(){return self.clients.claim();}));
});
self.addEventListener('fetch',function(e){
  var r=e.request;
  if(r.url===LIB){
    e.respondWith(caches.match(LIB).then(function(hit){
      return hit||fetch(r).then(function(res){
        var copy=res.clone();caches.open(CACHE).then(function(c){c.put(LIB,copy);});
        return res;
      });
    }));
    return;
  }
  if(r.mode!=='navigate')return;
  e.respondWith(fetch(r).then(function(res){
    var copy=res.clone();caches.open(CACHE).then(function(c){c.put('./',copy);});
    return res;
  }).catch(function(){return caches.match('./');}));
});
