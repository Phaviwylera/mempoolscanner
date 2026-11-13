const POLL_MS = 5000;
const state = { hits: [], filter: "all", online: false };

const $ = (selector) => document.querySelector(selector);
const shortHash = (value = "") => value.length > 18 ? `${value.slice(0, 10)}…${value.slice(-6)}` : value || "—";
const shortAddress = (value = "") => value ? `${value.slice(0, 7)}…${value.slice(-5)}` : "contract creation";

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
  const label = document.createElement("span"); label.className = "latest-label"; label.textContent = hit.label || "SIMULATION RESULT";
  const detail = document.createElement("div"); detail.className = "latest-detail";
  const tx = document.createElement("span"); tx.textContent = `TX  ${shortHash(hit.hash)}`;
  const dest = document.createElement("span"); dest.textContent = `TO  ${shortAddress(hit.to)}`;
  const timing = document.createElement("span"); timing.textContent = `SEEN  ${relativeTime(hit.ts)}`;
  detail.append(tx, dest, timing); host.append(title, label, detail);
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
    const result = document.createElement("td"); const chip = document.createElement("span"); const ok = Boolean(hit.sim?.ok); chip.className = `result-chip${ok ? "" : " failed"}`; const dot = document.createElement("i"); const label = document.createElement("span"); label.textContent = ok ? "CALL OK" : "SIM FAILED"; chip.append(dot, label); result.append(chip); row.append(result);
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
}

$("#refresh-button").addEventListener("click", refresh);
$("#clear-filter").addEventListener("click", (event) => {
  state.filter = state.filter === "all" ? "ok" : state.filter === "ok" ? "failed" : "all";
  const label = state.filter === "all" ? "ALL MATCHES" : state.filter === "ok" ? "CALL OK" : "SIM FAILED";
  event.currentTarget.firstChild.textContent = `${label} `;
  renderTable();
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

