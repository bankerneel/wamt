const GLYPHS = {
  W: ["10001", "10001", "10001", "10101", "10101", "11011", "10001"],
  H: ["10001", "10001", "10001", "11111", "10001", "10001", "10001"],
  O: ["01110", "10001", "10001", "10001", "10001", "10001", "01110"],
  A: ["01110", "10001", "10001", "11111", "10001", "10001", "10001"],
  T: ["11111", "00100", "00100", "00100", "00100", "00100", "00100"],
  E: ["11111", "10000", "10000", "11110", "10000", "10000", "11111"],
  M: ["10001", "11011", "10101", "10101", "10001", "10001", "10001"],
  Y: ["10001", "10001", "01010", "00100", "00100", "00100", "00100"],
  K: ["10001", "10010", "10100", "11000", "10100", "10010", "10001"],
  N: ["10001", "11001", "10101", "10011", "10001", "10001", "10001"],
  S: ["01111", "10000", "10000", "01110", "00001", "00001", "11110"],
  "?": ["01110", "10001", "00001", "00010", "00100", "00000", "00100"],
};
function pixelWordmark(text) {
  const rects = [];
  let x = 0;
  for (const ch of text) {
    if (ch === " ") { x += 3; continue; }
    const g = GLYPHS[ch];
    if (!g) continue;
    g.forEach((row, y) => {
      // Merge runs of lit pixels in a row into one rect: no seams.
      let run = -1;
      for (let i = 0; i <= 5; i++) {
        const on = i < 5 && row[i] === "1";
        if (on && run < 0) run = i;
        if (!on && run >= 0) { rects.push([x + run, y, i - run]); run = -1; }
      }
    });
    x += 6;
  }
  // Two units per pixel keeps every edge, and the half-pixel shadow, on
  // whole numbers, so crisp edges never round unevenly.
  const w = x * 2, h = 7 * 2 + 1;
  const body = (dx, dy, fill) => rects.map(([rx, ry, rw]) => `<rect x="${rx * 2 + dx}" y="${ry * 2 + dy}" width="${rw * 2}" height="2" style="fill:${fill}"/>`).join("");
  return `<svg viewBox="0 0 ${w} ${h}" shape-rendering="crispEdges" role="img" aria-label="Who Ate My Tokens?">
    <defs><linearGradient id="wm" gradientUnits="userSpaceOnUse" x1="0" y1="0" x2="0" y2="${h}">
      <stop offset="0" style="stop-color:var(--w-top)"/><stop offset=".55" style="stop-color:var(--w-mid)"/><stop offset="1" style="stop-color:var(--w-bot)"/>
    </linearGradient></defs>
    <g>${body(1, 1, "var(--w-shadow)")}</g><g>${body(0, 0, "url(#wm)")}</g></svg>`;
}

const title = document.querySelector(".wordmark");
const lines = document.body.classList.contains("social")
  ? ["WHO ATE MY", "TOKENS?"]
  : ["WHO ATE MY TOKENS?"];

lines.forEach((line, index) => {
  const markup = pixelWordmark(line).replaceAll("wm", "wm-" + index);
  title.insertAdjacentHTML("beforeend", markup);
});
