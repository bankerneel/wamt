// Check the pace math against a saved /api/state response.
// Usage: node tools/pace-check.js /path/to/state.json
"use strict";
const fs = require("fs");
const path = require("path");

const arg = process.argv[2];
if (!arg) {
  console.error("usage: node tools/pace-check.js <saved-api-state.json>");
  process.exit(2);
}
const root = path.resolve(__dirname, "..");
const html = fs.readFileSync(path.join(root, "ui/index.html"), "utf8");
const from = "// ---- pace math ----";
const to = "// ---- end pace math ----";
const a = html.indexOf(from), b = html.indexOf(to, a);
if (a < 0 || b < 0) throw new Error("pace math block not found in ui/index.html");
const block = html.slice(a, b + to.length);
const pace = new Function(block + "\nreturn { paceStats, paceHourFrac, paceAverageActive };")();

const state = JSON.parse(fs.readFileSync(arg, "utf8"));
const live = state.live || {}, days = state.days || {};
const hourFrac = pace.paceHourFrac(live.now == null ? Date.now() : live.now, live.off);
const stats = pace.paceStats(days.week || [], hourFrac);
const avg = pace.paceAverageActive(days.month || []);

console.log("local time of day:", hourFrac.toFixed(3), "hours");
if (!stats) {
  console.log("pace: unavailable (need at least 3 earlier active days and a positive baseline)");
} else {
  console.log("ratio:", stats.ratio.toFixed(2) + "x");
  console.log("normal by now:", stats.normal.toFixed(1), "weighted units");
  console.log("today so far:", stats.today.toFixed(1), "weighted units");
  console.log("normal full day:", stats.normalFull.toFixed(1), "weighted units");
  console.log("earlier active days used:", stats.daysUsed);
}
console.log("avg active day (30d):", avg.avg.toFixed(1), "over", avg.count, "days");
