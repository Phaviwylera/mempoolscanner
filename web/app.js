const POLL_MS = 5000;
const DEFAULT_MAX_LAG = 3;
const QUALITY_ORDER = ["candidate", "state_dependent", "sim_failed", "no_code", "stale_fork", "other"];
const KIND_TEXT = {
  candidate: "CALL OK",
  state_dependent: "BLOCK DEPENDENT",
  stale_fork: "STALE FORK",
  no_code: "NO CODE",
  sim_failed: "SIM FAILED",
  other: "UNCLASSIFIED",
};
const state = { hits: [], filter: "all", online: false, quality: null };

const $ = (selector) => document.querySelector(selector);
const shortHash = (value = "") => value.length > 18 ? `${value.slice(0, 10)}…${value.slice(-6)}` : value || "—";
const shortAddress = (value = "") => value ? `${value.slice(0, 7)}…${value.slice(-5)}` : "contract creation";

// Label prefix only: the quality labels are read as-is, never renamed.
function labelKind(label = "") {
  const head = String(label || "").split(":")[0].trim().toLowerCase();
  return QUALITY_ORDER.includes(head) ? head : "other";
}

const shown = (value) => value === null || value === undefined || value === "" ? "not recorded" : String(value);

function formatWei(value, decimals = 18, places = 5, suffix = " ETH") {
  try {
    const amount = BigInt(String(value ?? 0));
    const unit = 10n ** BigInt(decimals);
    const whole = amount / unit;
    const fraction = (amount % unit).toString().padStart(decimals, "0").slice(0, places).replace(/0+$/, "");
    return `${whole.toLocaleString()}${fraction ? `.${fraction}` : ""}${suffix}`;
  } catch { return "—"; }
}

function relativeTime(value) {
  const date = new Date(value);
  if (Number.isNaN(date.getTime())) return "—";
  const seconds = Math.max(0, Math.floor((Date.now() - date.getTime()) / 1000));
  if (seconds < 10) return "JUST NOW";
  if (seconds < 60) return `${seconds}S AGO`;
  if (seconds < 3600) return `${Math.floor(seconds / 60)}M AGO`;
  return date.toLocaleTimeString("en-GB", { hour: "2-digit", minute: "2-digit", timeZone: "UTC" }) + " UTC";
}

function setOnline(online, message = "") {
  state.online = online;
  const badge = $("#feed-status");
  const label = $("#feed-pill");
  const detail = $("#feed-detail");
  badge.classList.toggle("offline", !online);
  badge.querySelector("b").textContent = online ? "API ONLINE" : "API OFFLINE";
  label.textContent = online ? "API ONLINE" : "RETRYING";
  label.className = `state-pill ${online ? "ready" : "waiting"}`;
  detail.textContent = message || (online ? "Recent-hit endpoint reachable" : "Retrying connection");
}

function makeCell(text, className = "") {
  const cell = document.createElement("td");
  if (className) cell.className = className;
  cell.textContent = text;
  return cell;
}

// ---- global fork-health banner -------------------------------------------
function renderBanner(data) {
  const banner = $("#fork-banner");
  const summary = data && data.banner ? data.banner : null;
  const kind = summary && summary.state ? summary.state : "unknown";
  banner.dataset.state = kind;
  $("#fork-banner-state").textContent = `FORK HEALTH: ${kind.toUpperCase()}`;
  $("#fork-banner-text").textContent = summary
    ? summary.text
    : "fork evidence endpoint unavailable — quality cannot be confirmed";
  const meta = [];
  if (summary) {
    if (summary.lag !== null && summary.lag !== undefined) meta.push(`LAG ${summary.lag}`);
    if (summary.max_observed_lag !== null && summary.max_observed_lag !== undefined) meta.push(`MAX LAG ${summary.max_observed_lag}`);
    if (summary.stalled) meta.push("STALLED");
    if (summary.newest_ts) meta.push(`NEWEST ${relativeTime(summary.newest_ts)}`);
    if (summary.window) meta.push(`WINDOW ${summary.window}`);
  }
  $("#fork-banner-meta").textContent = meta.join(" · ") || "NO RECORDS";
}

function rollupRow(kind, count, share) {
  const row = document.createElement("div");
  row.className = "fork-row";
  row.dataset.kind = kind;
  const name = document.createElement("span");
  name.className = "fork-row-name";
  name.textContent = kind;
  const track = document.createElement("i");
  track.className = "fork-row-track";
  const fill = document.createElement("b");
  fill.style.width = `${Math.max(0, Math.min(100, Number(share) || 0))}%`;
  track.append(fill);
  const value = document.createElement("em");
  value.textContent = `${count} · ${share === null || share === undefined ? "—" : `${share}%`}`;
  row.append(name, track, value);
  return row;
}

// ---- aggregate fork quality ---------------------------------------------
function renderRollup(data) {
  const host = $("#fork-rollup");
  const note = $("#fork-rollup-note");
  const rollup = data && data.rollup ? data.rollup : null;
  host.replaceChildren();

  $("#fork-window-note").textContent = data && data.window
    ? `FILTER ${String(data.window.filter || "all").toUpperCase()} · ${shown(data.window.filtered)} / ${shown(data.window.returned)} RECORDS`
    : "WINDOW —";
  $("#fork-window-chip").textContent = rollup && rollup.available ? String(rollup.with_fork) : "0";
  $("#fork-lag-median").textContent = rollup && rollup.lag ? shown(rollup.lag.median) : "not recorded";
  $("#fork-lag-max").textContent = rollup && rollup.lag ? shown(rollup.lag.max) : "not recorded";
  $("#fork-checked").textContent = rollup && rollup.checked_share !== null && rollup.checked_share !== undefined
    ? `${rollup.checked_share}%`
    : "not recorded";

  if (!rollup || !rollup.available) {
    // Never render zeros here: zero counts would read as "everything is fine".
    const empty = document.createElement("p");
    empty.className = "fork-unavailable";
    empty.textContent = rollup && rollup.note ? rollup.note : "fork data unavailable for this range";
    host.append(empty);
    note.textContent = rollup && rollup.total
      ? `${rollup.total} record(s) in this window predate fork evidence.`
      : "No records in this window yet.";
    return;
  }

  for (const kind of QUALITY_ORDER) {
    const count = rollup.counts ? rollup.counts[kind] ?? 0 : 0;
    if (kind === "other" && !count) continue;
    host.append(rollupRow(kind, count, rollup.percent ? rollup.percent[kind] : null));
  }
  const parts = [`${rollup.with_fork} of ${rollup.total} record(s) carry fork evidence`];
  if (rollup.checked_share !== null && rollup.checked_share !== undefined) parts.push(`${rollup.checked_share}% ran the stability check`);
  note.textContent = parts.join(" · ") + ".";
}

// ---- per-hit fork provenance --------------------------------------------
function buildEvidence(hit) {
  const fork = hit && hit.fork && typeof hit.fork === "object" && Object.keys(hit.fork).length ? hit.fork : null;
  const maxLag = state.quality && state.quality.window && typeof state.quality.window.max_lag === "number"
    ? state.quality.window.max_lag
    : DEFAULT_MAX_LAG;
  const wrap = document.createElement("div");
  wrap.className = "fork-evidence";

  const head = document.createElement("div");
  head.className = "fork-evidence-head";
  const title = document.createElement("span");
  title.textContent = "FORK EVIDENCE";
  const kind = labelKind(hit ? hit.label : "");
  const chip = document.createElement("b");
  chip.className = "fk-chip";
  chip.dataset.kind = kind;
  chip.textContent = kind;
  head.append(title, chip);
  wrap.append(head);

  if (!fork) {
    const missing = document.createElement("p");
    missing.className = "fork-missing";
    missing.textContent = "not recorded · this observation predates fork evidence";
    wrap.append(missing);
    return wrap;
  }

  const list = document.createElement("dl");
  list.className = "fork-fields";
  const row = (term, value, flagged = false) => {
    const item = document.createElement("div");
    const dt = document.createElement("dt");
    dt.textContent = term;
    const dd = document.createElement("dd");
    dd.textContent = value;
    if (flagged) dd.classList.add("flagged");
    item.append(dt, dd);
    list.append(item);
  };

  row("BLOCK", shown(fork.block));
  const lag = typeof fork.lag === "number" ? fork.lag : null;
  const stale = lag !== null && lag > maxLag;
  const headText = fork.head === null || fork.head === undefined
    ? "not recorded"
    : lag === null
      ? `${fork.head} · lag not recorded`
      : `${fork.head} · lag ${lag}${stale ? " ⚠ stale" : ""}`;
  row("HEAD / LAG", headText, stale);
  row("DEPTH", shown(fork.depth));
  row("CONTRACT CODE", fork.code_present === true ? "present" : fork.code_present === false ? "absent" : "not recorded");
  const checked = Array.isArray(fork.checked) ? fork.checked : [];
  row("STABILITY CHECK", checked.length ? `ran · ${checked.join(" + ")}` : "not run");
  const reference = fork.reference && typeof fork.reference === "object" ? fork.reference : null;
  row("REFERENCE OUTCOME", reference ? `block ${shown(reference.block)} · ${reference.ok ? "ok" : "reverted"}` : "not recorded");
  wrap.append(list);
  return wrap;
}

function renderLatest(hit) {
  const host = $("#latest-hit");
  host.replaceChildren();
  if (!hit) {
    host.className = "latest-empty";
    const orbit = document.createElement("span"); orbit.className = "empty-orbit";
    const dot = document.createElement("i"); orbit.append(dot);
    const title = document.createElement("b"); title.textContent = "No matching calls yet";
    const note = document.createElement("small"); note.textContent = "The feed will appear here when a watched selector is observed.";
    host.append(orbit, title, note);
    return;
  }
  host.className = "latest-hit";
  const title = document.createElement("div"); title.className = "latest-name"; title.textContent = hit.name || "Unknown selector";
  const label = document.createElement("span");
  label.className = "latest-label";
  label.dataset.kind = labelKind(hit.label);
  label.textContent = hit.label || "SIMULATION RESULT";
  const detail = document.createElement("div"); detail.className = "latest-detail";
  const tx = document.createElement("span"); tx.textContent = `TX  ${shortHash(hit.hash)}`;
  const dest = document.createElement("span"); dest.textContent = `TO  ${shortAddress(hit.to)}`;
  const timing = document.createElement("span"); timing.textContent = `SEEN  ${relativeTime(hit.ts)}`;
  detail.append(tx, dest, timing);
  host.append(title, label, detail, buildEvidence(hit));
}

function renderTable() {
  const body = $("#activity-body");
  body.replaceChildren();
  const visible = state.hits.filter((hit) => state.filter === "all" || Boolean(hit.sim?.ok) === (state.filter === "ok"));
  $("#activity-count").textContent = String(state.hits.length);
  $("#shown-count").textContent = String(visible.length);
  if (!visible.length) {
    const row = document.createElement("tr"); row.className = "empty-row";
    const cell = document.createElement("td"); cell.colSpan = 8;
    const mark = document.createElement("span"); mark.className = "empty-mark"; mark.textContent = "⌁";
    const title = document.createElement("b"); title.textContent = state.hits.length ? "No observations match this filter" : "Waiting for the first selector match";
    const note = document.createElement("small"); note.textContent = state.hits.length ? "Choose All matches to show every recorded observation." : "Only actual observations are shown here. No sample transactions are inserted.";
    cell.append(mark, title, note); row.append(cell); body.append(row); return;
  }
  for (const hit of visible.slice(0, 50)) {
    const row = document.createElement("tr");
    const name = document.createElement("td"); name.className = "function-name";
    const nameTag = document.createElement("span"); nameTag.className = "function-chip"; nameTag.textContent = hit.name || "selector"; name.append(nameTag); row.append(name);
    const hash = document.createElement("td"); const link = document.createElement("a"); link.className = "tx-link"; link.href = `https://etherscan.io/tx/${encodeURIComponent(hit.hash || "")}`; link.target = "_blank"; link.rel = "noopener noreferrer"; link.textContent = shortHash(hit.hash); hash.append(link); row.append(hash);
    row.append(makeCell(shortAddress(hit.from), "address"), makeCell(shortAddress(hit.to), "address"), makeCell(formatWei(hit.value)), makeCell(Number(hit.gas || 0).toLocaleString()));
    const result = document.createElement("td"); const chip = document.createElement("span"); const ok = Boolean(hit.sim?.ok); const kind = labelKind(hit.label);
    chip.className = `result-chip${ok ? "" : " failed"}`; chip.dataset.kind = kind;
    const dot = document.createElement("i"); const label = document.createElement("span");
    label.textContent = KIND_TEXT[kind] || (ok ? "CALL OK" : "SIM FAILED");
    chip.append(dot, label); result.append(chip); row.append(result);
    const time = document.createElement("td"); time.textContent = new Date(hit.ts).toLocaleTimeString("en-GB", { hour: "2-digit", minute: "2-digit", second: "2-digit", timeZone: "UTC" }); row.append(time);
    body.append(row);
  }
}

function render() {
  const total = state.hits.length;
  const success = state.hits.filter((hit) => Boolean(hit.sim?.ok)).length;
  const failed = total - success;
  $("#metric-total").textContent = total.toLocaleString();
  $("#metric-ok").textContent = success.toLocaleString();
  $("#metric-failed").textContent = failed.toLocaleString();
  const latest = state.hits[0];
  $("#metric-last").textContent = latest ? relativeTime(latest.ts) : "—";
  $("#metric-last-sub").textContent = latest ? `${latest.name || "selector"} · ${shortHash(latest.hash)}` : "Waiting for a selector match";
  renderLatest(latest); renderTable();
}

async function refreshQuality() {
  try {
    const response = await fetch(`/api/quality?limit=100&filter=${encodeURIComponent(state.filter)}`, { cache: "no-store", headers: { Accept: "application/json" } });
    if (!response.ok) throw new Error(`quality feed returned ${response.status}`);
    state.quality = await response.json();
  } catch {
    state.quality = null;
  }
  renderBanner(state.quality);
  renderRollup(state.quality);
  const latest = state.hits[0];
  if (latest) renderLatest(latest);
}

async function refresh() {
  try {
    const response = await fetch("/api/hits?limit=100", { cache: "no-store", headers: { Accept: "application/json" } });
    if (!response.ok) throw new Error(`feed returned ${response.status}`);
    const payload = await response.json();
    if (!Array.isArray(payload.hits)) throw new Error("invalid feed response");
    state.hits = payload.hits;
    setOnline(true);
    $("#updated-at").textContent = `UPDATED ${new Date().toLocaleTimeString("en-GB", { hour: "2-digit", minute: "2-digit", second: "2-digit", timeZone: "UTC" })} UTC`;
    render();
  } catch (error) {
    setOnline(false, "API unavailable · retrying");
    $("#updated-at").textContent = "FEED RETRYING";
  }
  await refreshQuality();
}

$("#refresh-button").addEventListener("click", refresh);
$("#clear-filter").addEventListener("click", (event) => {
  state.filter = state.filter === "all" ? "ok" : state.filter === "ok" ? "failed" : "all";
  const label = state.filter === "all" ? "ALL MATCHES" : state.filter === "ok" ? "CALL OK" : "SIM FAILED";
  event.currentTarget.firstChild.textContent = `${label} `;
  renderTable();
  refreshQuality();
});

// Ambient orbital field inspired by the reference; it does not map transaction paths.
function startField() {
  const canvas = $("#field-canvas");
  const context = canvas.getContext("2d");
  if (!context) return;
  const reduceMotion = window.matchMedia("(prefers-reduced-motion: reduce)").matches;
  let width = 0, height = 0, points = [], frame = 0;
  function resize() {
    const rect = canvas.getBoundingClientRect();
    const ratio = Math.min(window.devicePixelRatio || 1, 2);
    width = rect.width; height = rect.height; canvas.width = width * ratio; canvas.height = height * ratio;
    context.setTransform(ratio, 0, 0, ratio, 0, 0);
    const count = Math.max(40, Math.min(105, Math.round(width * height / 2300)));
    points = Array.from({ length: count }, () => {
      const angle = Math.random() * Math.PI * 2;
      const radius = Math.pow(Math.random(), .72) * Math.min(width * .47, height * .52);
      return { angle, radius, drift: (Math.random() - .5) * .0012, size: Math.random() * 1.4 + .3, phase: Math.random() * 6.28, hue: Math.random() };
    });
  }
  function draw() {
    context.clearRect(0, 0, width, height);
    const cx = width * .5, cy = height * .49;
    const positions = points.map((point, index) => {
      point.angle += reduceMotion ? 0 : point.drift;
      const pulse = 1 + Math.sin(frame * .012 + point.phase) * .025;
      const x = cx + Math.cos(point.angle) * point.radius * pulse;
      const y = cy + Math.sin(point.angle) * point.radius * pulse * .68;
      return { x, y, point, index };
    });
    for (let i = 0; i < positions.length; i++) {
      const a = positions[i];
      for (let j = i + 1; j < Math.min(i + 4, positions.length); j++) {
        const b = positions[j]; const distance = Math.hypot(a.x - b.x, a.y - b.y);
        if (distance < 65) {
          context.beginPath(); context.moveTo(a.x, a.y); context.lineTo(b.x, b.y);
          context.strokeStyle = `rgba(${a.point.hue > .84 ? "250,91,179" : "101,230,210"},${(1 - distance / 65) * .13})`;
          context.lineWidth = .65; context.stroke();
        }
      }
      const glow = .35 + (Math.sin(frame * .025 + a.point.phase) + 1) * .28;
      context.beginPath(); context.arc(a.x, a.y, a.point.size, 0, Math.PI * 2);
      context.fillStyle = a.point.hue > .84 ? `rgba(250,91,179,${glow})` : `rgba(112,239,218,${glow})`;
      context.fill();
    }
    frame++;
    if (!reduceMotion) requestAnimationFrame(draw);
  }
  resize(); draw();
  window.addEventListener("resize", () => { resize(); if (reduceMotion) draw(); }, { passive: true });
}

startField(); refresh(); window.setInterval(refresh, POLL_MS);
