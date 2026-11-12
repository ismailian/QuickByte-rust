// Two jobs: keep the download details in step with releases/latest.json, and
// draw the block map. The page works without either; the link in the HTML points
// at a real installer, so a release that only changes latest.json cannot break it.

(() => {
  "use strict";

  // ---- The current release -------------------------------------------------

  const formatSize = (bytes) => `${(bytes / (1024 * 1024)).toFixed(1)} MB`;

  const formatDate = (value) =>
    new Date(value).toLocaleDateString("en-GB", { day: "numeric", month: "long", year: "numeric", timeZone: "UTC" });

  async function showLatestRelease() {
    let release;
    try {
      const response = await fetch("/releases/latest.json", { cache: "no-cache" });
      if (!response.ok) return;
      release = await response.json();
    } catch {
      return; // The figures already in the page stand.
    }

    if (!release?.version || !release?.downloadUrl) return;

    const path = new URL(release.downloadUrl).pathname;
    const fill = (name, text) => {
      if (text) document.querySelectorAll(`[data-release="${name}"]`).forEach((node) => { node.textContent = text; });
    };

    document.querySelectorAll("[data-installer]").forEach((link) => { link.href = path; });
    fill("version", release.version);
    fill("size", release.fileSizeBytes ? formatSize(release.fileSizeBytes) : null);
    fill("date", release.releaseDate ? formatDate(release.releaseDate) : null);
    fill("sha256", release.sha256);
    fill("file", path.split("/").pop());
  }

  showLatestRelease();

  // ---- The block map -------------------------------------------------------
  //
  // A file as a grid of blocks, read left to right. Eight connections start at
  // even offsets. When one reaches the end of its range, it takes the back half
  // of whichever range has the most left, the rule QuickByte itself follows, so
  // the slow connection does not hold the whole file up.

  const canvas = document.getElementById("block-map");
  if (!canvas) return;

  const status = document.getElementById("map-status");
  const replay = document.getElementById("map-replay");
  const context = canvas.getContext("2d");
  const reducedMotion = matchMedia("(prefers-reduced-motion: reduce)");
  const darkMode = matchMedia("(prefers-color-scheme: dark)");

  const CELL = 11;
  const GAP = 2;
  const STARTING_CONNECTIONS = 8;
  const MINIMUM_SPLIT_SHARE = 0.012; // of the file; QuickByte's own floor is 1 MiB
  const FILL_SECONDS = 15;

  let columns, rows, total, colours, cellWidth;
  let segments, nextId, written, finished, lastTime, frame, sweep;

  const readColours = () => {
    const style = getComputedStyle(document.documentElement);
    const token = (name) => style.getPropertyValue(name).trim();
    colours = { track: token("--track"), fill: token("--blue"), head: token("--head"), done: token("--green") };
  };

  const measure = () => {
    const width = canvas.clientWidth || document.documentElement.clientWidth;
    columns = Math.max(24, Math.floor((width + GAP) / (CELL + GAP)));
    cellWidth = (width + GAP) / columns - GAP; // stretched a hair, so the grid meets both edges
    rows = width < 640 ? 11 : 8;
    total = columns * rows;

    const scale = Math.min(window.devicePixelRatio || 1, 2);
    const height = rows * (CELL + GAP) - GAP;
    canvas.width = Math.round(width * scale);
    canvas.height = Math.round(height * scale);
    canvas.style.height = `${height}px`;
    context.setTransform(scale, 0, 0, scale, 0, 0);
  };

  /** Per-block speed. Connection 5 is the slow one every download has. */
  const speedFor = (index) => {
    const base = total / STARTING_CONNECTIONS / (FILL_SECONDS * 0.62);
    return base * (index === 4 ? 0.16 : 0.6 + 0.8 * Math.abs(Math.sin(index * 2.4 + 1)));
  };

  const reset = () => {
    const span = total / STARTING_CONNECTIONS;
    segments = Array.from({ length: STARTING_CONNECTIONS }, (_, index) => ({
      start: Math.round(index * span),
      end: Math.round((index + 1) * span),
      position: Math.round(index * span),
      speed: speedFor(index),
    }));
    nextId = STARTING_CONNECTIONS;
    written = new Uint8Array(total);
    finished = false;
    sweep = 0;
    lastTime = null;
  };

  const running = () => segments.filter((segment) => segment.position < segment.end);

  /** Gives a freed connection the back half of the range with the most left. */
  const splitFor = (freed) => {
    const widest = running().reduce(
      (best, segment) => (segment.end - segment.position > (best ? best.end - best.position : 0) ? segment : best),
      null);
    if (!widest || widest.end - widest.position < Math.max(8, total * MINIMUM_SPLIT_SHARE)) return;

    const middle = Math.round((widest.position + widest.end) / 2);
    segments.push({ start: middle, end: widest.end, position: middle, speed: freed.speed });
    widest.end = middle;
    nextId += 1;
  };

  const advance = (seconds) => {
    for (const segment of running()) {
      const from = Math.floor(segment.position);
      segment.position = Math.min(segment.end, segment.position + segment.speed * seconds);
      written.fill(1, from, Math.floor(segment.position));

      if (segment.position >= segment.end) {
        written.fill(1, from, segment.end);
        splitFor(segment);
      }
    }
    finished = running().length === 0;
  };

  const draw = () => {
    const stepX = cellWidth + GAP;
    const stepY = CELL + GAP;
    const doneThrough = sweep * total;
    context.clearRect(0, 0, canvas.clientWidth, canvas.height);

    for (let index = 0; index < total; index += 1) {
      const x = (index % columns) * stepX;
      const y = Math.floor(index / columns) * stepY;
      context.fillStyle = index < doneThrough ? colours.done : written[index] ? colours.fill : colours.track;
      context.fillRect(x, y, cellWidth, CELL);
    }

    context.fillStyle = colours.head;
    for (const segment of running()) {
      const index = Math.floor(segment.position);
      context.fillRect((index % columns) * stepX, Math.floor(index / columns) * stepY, cellWidth, CELL);
    }
  };

  const describe = () => {
    const percent = Math.floor((written.reduce((sum, block) => sum + block, 0) / total) * 100);
    if (sweep >= 1) return `Done. ${nextId} ranges merged into one file.`;
    if (finished) return "All bytes in. Merging.";
    const splits = nextId - STARTING_CONNECTIONS;
    return `${percent}% with ${running().length} connections, ${splits} ${splits === 1 ? "range" : "ranges"} split so far`;
  };

  const tick = (time) => {
    const seconds = lastTime === null ? 0 : Math.min((time - lastTime) / 1000, 0.05);
    lastTime = time;

    if (finished) sweep = Math.min(1, sweep + seconds / 1.1);
    else advance(seconds);

    draw();
    status.textContent = describe();
    frame = sweep < 1 ? requestAnimationFrame(tick) : null;
  };

  const stop = () => {
    if (frame) cancelAnimationFrame(frame);
    frame = null;
    lastTime = null;
  };

  const start = () => {
    stop();
    frame = requestAnimationFrame(tick);
  };

  /** Motion off, or nothing to run yet: show the map part way through, still. */
  const showStill = () => {
    reset();
    for (let elapsed = 0; elapsed < FILL_SECONDS * 0.5; elapsed += 0.05) advance(0.05);
    draw();
    status.textContent = describe();
  };

  readColours();
  measure();

  if (reducedMotion.matches) showStill();
  else { reset(); draw(); }

  // Runs while on screen; a map nobody can see has no business using the CPU.
  new IntersectionObserver(([entry]) => {
    if (reducedMotion.matches) return;
    if (!entry.isIntersecting) stop();
    else if (sweep < 1) start();
  }).observe(canvas);

  replay.addEventListener("click", () => { reset(); start(); });

  darkMode.addEventListener("change", () => { readColours(); draw(); });

  let lastWidth = canvas.clientWidth;
  addEventListener("resize", () => {
    if (canvas.clientWidth === lastWidth) return;
    lastWidth = canvas.clientWidth;
    stop();
    measure();
    if (reducedMotion.matches) showStill();
    else { reset(); start(); }
  });
})();
