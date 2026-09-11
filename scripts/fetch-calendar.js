// Runs inside GitHub Actions (a server, not a browser) — no CORS applies
// here, so this can hit ForexFactory's feed directly. Writes calendar.json
// at the repo root; the live app fetches that file same-origin.
const https = require("https");
const fs = require("fs");
const path = require("path");

const URL = "https://nfs.faireconomy.media/ff_calendar_thisweek.json";
const OUT = path.join(__dirname, "..", "calendar.json");

https.get(URL, { headers: { "User-Agent": "Mozilla/5.0 (compatible; EdgeTerminalCalendarBot/1.0)" } }, (res) => {
  if (res.statusCode !== 200) {
    console.error("Unexpected status " + res.statusCode);
    process.exit(1);
  }
  let data = "";
  res.on("data", (chunk) => { data += chunk; });
  res.on("end", () => {
    try {
      const rows = JSON.parse(data);
      if (!Array.isArray(rows)) throw new Error("response was not an array");
      const obj = { rows: rows, fetchedAt: Date.now() };
      fs.writeFileSync(OUT, JSON.stringify(obj));
      console.log("Wrote " + OUT + " with " + rows.length + " events");
    } catch (e) {
      console.error("Failed to parse feed: " + e.message);
      process.exit(1);
    }
  });
}).on("error", (e) => {
  console.error("Fetch failed: " + e.message);
  process.exit(1);
});
