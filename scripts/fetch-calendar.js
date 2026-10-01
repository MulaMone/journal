// Runs inside GitHub Actions (a server, not a browser) — no CORS applies
// here. Writes calendar.json at the repo root; the live app fetches that
// file same-origin.
//
// ForexFactory is the base feed. TradingView is then checked for High-impact
// USD events that ForexFactory doesn't list; only those are appended
// (source:"TV"). Events present in both are NOT duplicated — if ForexFactory
// rates a shared event lower than TradingView's High, it's upgraded to High
// (ffImpact keeps the original). If TradingView fails, FF-only is written.
const https = require("https");
const fs = require("fs");
const path = require("path");

const FF_URL = "https://nfs.faireconomy.media/ff_calendar_thisweek.json";
const TV_URL = "https://economic-calendar.tradingview.com/events";
const TV_COUNTRIES = "US"; // app only reads USD today
const OUT = path.join(__dirname, "..", "calendar.json");
const MATCH_WINDOW_MS = 60 * 60 * 1000; // same event if within 1h...
const MIN_SCORE = 0.5;                  // ...and titles overlap enough

function getJson(url, headers) {
  return new Promise((resolve, reject) => {
    const h = Object.assign({ "User-Agent": "Mozilla/5.0 (compatible; EdgeTerminalCalendarBot/1.0)", "Accept": "application/json" }, headers || {});
    https.get(url, { headers: h }, (res) => {
      if (res.statusCode !== 200) { res.resume(); return reject(new Error("HTTP " + res.statusCode)); }
      let data = "";
      res.on("data", (c) => { data += c; });
      res.on("end", () => { try { resolve(JSON.parse(data)); } catch (e) { reject(e); } });
    }).on("error", reject);
  });
}

// The app slices ev.date to get the ET calendar day, so TV's UTC timestamps
// are converted to the same ET-offset ISO format ForexFactory uses.
function toEtIso(ms) {
  const parts = new Intl.DateTimeFormat("en-US", {
    timeZone: "America/New_York", hourCycle: "h23", timeZoneName: "longOffset",
    year: "numeric", month: "2-digit", day: "2-digit", hour: "2-digit", minute: "2-digit", second: "2-digit",
  }).formatToParts(new Date(ms));
  const p = {};
  parts.forEach((x) => { p[x.type] = x.value; });
  const off = p.timeZoneName.replace("GMT", "") || "+00:00";
  return p.year + "-" + p.month + "-" + p.day + "T" + p.hour + ":" + p.minute + ":" + p.second + off;
}

// Title normalising so "Core CPI m/m" ≈ "Core Inflation Rate MoM", etc.
// Extend SYN if the log shows a shared event being added as a duplicate.
const SYN = [
  [/non farm (payrolls|employment change)|nonfarm (payrolls|employment change)/g, "nfp"],
  [/initial jobless claims|unemployment claims|jobless claims/g, "claims"],
  [/fed interest rate decision|federal funds rate|fomc rate decision/g, "fedrate"],
  [/consumer price index/g, "cpi"],
  [/core inflation rate/g, "core cpi"],
  [/inflation rate/g, "cpi"],
  [/producer price index/g, "ppi"],
  [/\b(preliminary|prelim|prel)\b/g, "prelim"],
];
const STOP = new Set(["us", "the", "of", "and", "s", "p"]);
function tokens(title) {
  let t = String(title || "").toLowerCase()
    .replace(/m\/m/g, "mom").replace(/y\/y/g, "yoy").replace(/q\/q/g, "qoq")
    .replace(/[^a-z0-9]+/g, " ");
  SYN.forEach((s) => { t = t.replace(s[0], s[1]); });
  return new Set(t.split(" ").filter((w) => w && !STOP.has(w)));
}
function jaccard(a, b) {
  let inter = 0;
  a.forEach((w) => { if (b.has(w)) inter++; });
  const uni = a.size + b.size - inter;
  return uni ? inter / uni : 0;
}

function merge(ffRows, tvEvents) {
  const ff = [];
  ffRows.forEach((r) => {
    const t = Date.parse(r.date);
    if (r.country === "USD" && Number.isFinite(t)) ff.push({ r: r, t: t, k: tokens(r.title) });
  });
  const tv = [];
  tvEvents.forEach((e) => {
    const t = Date.parse(e.date);
    if (e.country === "US" && e.importance >= 1 && Number.isFinite(t)) tv.push({ e: e, t: t, k: tokens(e.title) });
  });

  // Best-scoring pairs claim each other first (one-to-one), so "CPI m/m" and
  // "Core CPI m/m" in the same slot pair with their own twins, not each other.
  const pairs = [];
  tv.forEach((a, ai) => ff.forEach((b, bi) => {
    const dt = Math.abs(a.t - b.t);
    if (dt > MATCH_WINDOW_MS) return;
    const s = jaccard(a.k, b.k);
    if (s >= MIN_SCORE) pairs.push({ ai: ai, bi: bi, s: s, dt: dt });
  }));
  pairs.sort((x, y) => (y.s - x.s) || (x.dt - y.dt));

  const usedA = new Set(), usedB = new Set(), upgraded = [];
  pairs.forEach((p) => {
    if (usedA.has(p.ai) || usedB.has(p.bi)) return;
    usedA.add(p.ai); usedB.add(p.bi);
    const row = ff[p.bi].r;
    if (row.impact !== "High") {
      upgraded.push(row.title + " (FF " + row.impact + " → High)");
      row.ffImpact = row.impact; row.impact = "High"; row.source = "FF+TV";
    }
  });

  const added = [], addedInfo = [];
  tv.forEach((a, ai) => {
    if (usedA.has(ai)) return;
    const row = { title: a.e.title, country: "USD", impact: "High", date: toEtIso(a.t), source: "TV" };
    added.push(row);
    const near = ff.filter((b) => Math.abs(a.t - b.t) <= MATCH_WINDOW_MS).map((b) => b.r.title);
    addedInfo.push(row.date + "  " + row.title + (near.length ? "   [FF same slot: " + near.join(" | ") + "]" : ""));
  });
  return { rows: ffRows.concat(added), added: added, addedInfo: addedInfo, upgraded: upgraded };
}

async function main() {
  let ff;
  try {
    ff = await getJson(FF_URL);
    if (!Array.isArray(ff)) throw new Error("response was not an array");
  } catch (e) {
    console.error("ForexFactory fetch failed: " + e.message); // keep last good calendar.json
    process.exit(1);
  }

  // Same Sun–Sat window FF's "thisweek" feed covers, so TV events outside it
  // aren't mistaken for "missing from FF".
  const now = new Date();
  const sun = Date.UTC(now.getUTCFullYear(), now.getUTCMonth(), now.getUTCDate() - now.getUTCDay());
  const qs = new URLSearchParams({
    from: new Date(sun).toISOString(),
    to: new Date(sun + 7 * 864e5 - 1).toISOString(),
    countries: TV_COUNTRIES,
  });
  let tv = [];
  try {
    const d = await getJson(TV_URL + "?" + qs.toString(), { Origin: "https://www.tradingview.com", Referer: "https://www.tradingview.com/" });
    if (!d || !Array.isArray(d.result)) throw new Error("unexpected shape");
    tv = d.result;
  } catch (e) {
    console.error("TradingView fetch failed, writing FF only: " + e.message);
  }

  const m = merge(ff, tv);
  fs.writeFileSync(OUT, JSON.stringify({ rows: m.rows, fetchedAt: Date.now() }));
  console.log("Wrote " + OUT + ": " + ff.length + " FF events + " + m.added.length + " TV-only High events (" + tv.length + " TV events fetched)");
  if (m.addedInfo.length) { console.log("MISSING FROM FF (added from TradingView):"); m.addedInfo.forEach((l) => console.log("  + " + l)); }
  if (m.upgraded.length) { console.log("UPGRADED to High by TradingView:"); m.upgraded.forEach((l) => console.log("  ^ " + l)); }
}

if (require.main === module) main();
module.exports = { merge: merge, toEtIso: toEtIso, tokens: tokens };
