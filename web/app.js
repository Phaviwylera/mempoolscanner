const POLL_MS = 5000;
const DEFAULT_MAX_LAG = 3;
const QUALITY_ORDER = ["candidate", "state_dependent", "sim_failed", "no_code", "stale_fork", "other"];
const FILTER_CYCLE = ["all", "ok", "extractable", "failed"];
const FILTER_LABELS = {
  all: "ALL MATCHES",
  ok: "REVM OK",
  extractable: "EXTRACTABLE",
  failed: "SIM FAILED",
};
const KIND_TEXT = {
  candidate: "CALL OK",
  state_dependent: "BLOCK DEPENDENT",
  stale_fork: "STALE FORK",
  no_code: "NO CODE",
  sim_failed: "SIM FAILED",
  other: "UNCLASSIFIED",
};
const PROBE_TEXT = {
  extractable: "EXTRACTABLE",
  beneficiary_locked: "LOCKED TO VICTIM",
  sender_protected: "PROTECTED",
  not_applicable: "N/A",
};

const DEFAULT_ETH_INR = 260000;
const state = { hits: [], filter: "all", online: false, quality: null, ethInr: DEFAULT_ETH_INR, fxSource: "reference" };

function currentEthInr() {
  const qRate = Number(state.quality?.fx?.eth_inr || state.quality?.extractable?.eth_inr_rate || 0);
  if (Number.isFinite(qRate) && qRate > 0) return qRate;
  if (Number.isFinite(state.ethInr) && state.ethInr > 0) return state.ethInr;
  return DEFAULT_ETH_INR;
}

const INR_FORMATTER = new Intl.NumberFormat("en-IN", {
  minimumFractionDigits: 2,
  maximumFractionDigits: 2,
});

const INR_FORMATTER_4 = new Intl.NumberFormat("en-IN", {
  minimumFractionDigits: 4,
  maximumFractionDigits: 4,
});

function parsePositiveWei(rawVal) {
  try {
    const s = String(rawVal ?? "0").trim();
    if (!s || s === "0" || s.startsWith("-")) return 0n;
    const w = BigInt(s);
    return w > 0n ? w : 0n;
  } catch {
    return 0n;
  }
}

function rowEffectiveWei(hit) {
  const txVal = parsePositiveWei(hit?.value);
  const grossVal = parsePositiveWei(hit?.decision?.gross_value_wei);
  return grossVal > txVal ? grossVal : txVal;
}

function formatInrFromWei(weiStr, ethInr = currentEthInr(), { plus = false } = {}) {
  try {
    const raw = String(weiStr ?? "0").trim();
    if (!raw || raw === "0") return "₹0.00";
    const neg = raw.startsWith("-");
    const clean = neg ? raw.slice(1) : raw;
    const wei = BigInt(clean || "0");
    if (wei === 0n) return "₹0.00";
    const rateScaled = BigInt(Math.max(1, Math.round(Number(ethInr || DEFAULT_ETH_INR) * 10000)));
    const microRupees = (wei * rateScaled) / (10n ** 18n);
    const sign = neg ? "-" : (plus ? "+" : "");
    if (microRupees === 0n) {
      return `${sign}< ₹0.01`;
    }
    const rupees = Number(microRupees) / 10000;
    if (rupees < 0.01) {
      return `${sign}₹${INR_FORMATTER_4.format(rupees)}`;
    }
    return `${sign}₹${INR_FORMATTER.format(rupees)}`;
  } catch {
    return "₹0.00";
  }
}

const $ = (selector) => document.querySelector(selector);
const shortHash = (value = "") => value.length > 18 ? `${value.slice(0, 10)}…${value.slice(-6)}` : value || "—";
const shortAddress = (value = "") => value ? `${value.slice(0, 7)}…${value.slice(-5)}` : "contract creation";

function labelKind(label = "") {
  const head = String(label || "").split(":")[0].trim().toLowerCase();
  return QUALITY_ORDER.includes(head) ? head : "other";
}

function decisionStatus(hit) {
  const st = hit && hit.decision && hit.decision.status ? String(hit.decision.status) : "not_applicable";
  if (st === "extractable") {
    const g = parsePositiveWei(hit?.decision?.gross_value_wei);
    const n = parsePositiveWei(hit?.decision?.net_profit_wei);
    if (g <= 0n && n <= 0n) return "not_applicable";
    return "extractable";
  }
  return Object.prototype.hasOwnProperty.call(PROBE_TEXT, st) ? st : "not_applicable";
}

const shown = (value) => value === null || value === undefined || value === "" ? "not recorded" : String(value);

function formatUs(us) {
  if (us === null || us === undefined || Number.isNaN(Number(us))) return "—";
  const n = Number(us);
  if (n < 1000) return `${Math.round(n)} µs`;
  return `${(n / 1000).toFixed(2)} ms`;
}

function formatWei(value, decimals = 18, places = 5, suffix = " ETH") {
  try {
    const raw = String(value ?? "0").trim();
    const neg = raw.startsWith("-");
    const clean = neg ? raw.slice(1) : raw;
    const amount = BigInt(clean || "0");
    const unit = 10n ** BigInt(decimals);
    const whole = amount / unit;
    const fraction = (amount % unit).toString().padStart(decimals, "0").slice(0, places).replace(/0+$/, "");
    return `${neg ? "-" : ""}${whole.toLocaleString()}${fraction ? `.${fraction}` : ""}${suffix}`;
  } catch {
    return "—";
  }
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

function matchesHitFilter(hit, filter) {
  if (filter === "ok") return Boolean(hit.sim?.ok);
  if (filter === "failed") return !Boolean(hit.sim?.ok);
  if (filter === "extractable") return decisionStatus(hit) === "extractable";
  return true;
}

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
  const bucket = Math.round(Math.max(0, Math.min(100, Number(share) || 0)) / 5) * 5;
  fill.className = `w-${bucket}`;
  track.append(fill);
  const value = document.createElement("em");
  value.textContent = `${count} · ${share === null || share === undefined ? "—" : `${share}%`}`;
  row.append(name, track, value);
  return row;
}

function renderPipelineTelemetry(data) {
  const pipe = data && data.pipeline ? data.pipeline : null;
  const lat = pipe && pipe.latency_us ? pipe.latency_us : {};
  const strip = $("#stage-telemetry-strip");
  const badge = $("#field-latency-badge");

  if (pipe && pipe.with_decision > 0) {
    strip.textContent = `INGEST ${formatUs(lat.median_ingest)} · DECODE ${formatUs(lat.median_decode)} · REVM SIM ${formatUs(lat.median_sim)} · PROBE ${formatUs(lat.median_probe)}`;
    badge.textContent = `MEDIAN TOTAL ${formatUs(lat.median_total)}`;
  } else {
    strip.textContent = "INGEST — · DECODE — · REVM SIM — · PROBE —";
    badge.textContent = "MEDIAN —";
  }

  const simMed = $("#fork-sim-median");
  if (simMed) {
    simMed.textContent = lat.median_sim !== null && lat.median_sim !== undefined ? formatUs(lat.median_sim) : "not recorded";
  }

  const frHost = $("#frontrun-rollup");
  if (frHost) {
    frHost.replaceChildren();
    if (pipe && pipe.with_decision > 0) {
      const counts = pipe.frontrun_counts || {};
      for (const key of ["extractable", "beneficiary_locked", "sender_protected"]) {
        const pill = document.createElement("div");
        pill.className = "fr-stat-pill";
        const chip = document.createElement("b");
        chip.className = "fk-chip";
        chip.dataset.kind = key;
        chip.textContent = key;
        const num = document.createElement("span");
        num.textContent = String(counts[key] ?? 0);
        pill.append(chip, num);
        frHost.append(pill);
      }
    }
  }
}

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
  $("#fork-checked").textContent = rollup && rollup.checked_share !== null && rollup.checked_share !== undefined
    ? `${rollup.checked_share}%`
    : "not recorded";

  renderPipelineTelemetry(data);

  if (!rollup || !rollup.available) {
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
  if (rollup.checked_share !== null && rollup.checked_share !== undefined) parts.push(`${rollup.checked_share}% ran stability check`);
  note.textContent = parts.join(" · ") + ".";
}

function buildEvidence(hit) {
  const fork = hit && hit.fork && typeof hit.fork === "object" && Object.keys(hit.fork).length ? hit.fork : null;
  const decision = hit && hit.decision && typeof hit.decision === "object" ? hit.decision : null;
  const decoded = hit && hit.decoded && typeof hit.decoded === "object" ? hit.decoded : null;
  const telemetry = hit && hit.telemetry && typeof hit.telemetry === "object" ? hit.telemetry : null;
  const maxLag = state.quality && state.quality.window && typeof state.quality.window.max_lag === "number"
    ? state.quality.window.max_lag
    : DEFAULT_MAX_LAG;

  const wrap = document.createElement("div");
  wrap.className = "fork-evidence";

  if (decoded && decoded.summary) {
    const abiBox = document.createElement("div");
    abiBox.className = "decoded-summary";
    abiBox.textContent = decoded.summary;
    wrap.append(abiBox);
  }

  if (decision) {
    const decHead = document.createElement("div");
    decHead.className = "fork-evidence-head";
    const decTitle = document.createElement("span");
    decTitle.textContent = "FRONT-RUN PROBE";
    const st = decisionStatus(hit);
    const decChip = document.createElement("b");
    decChip.className = "fk-chip";
    decChip.dataset.kind = st;
    decChip.textContent = st;
    decHead.append(decTitle, decChip);
    wrap.append(decHead);

    const verdict = document.createElement("p");
    verdict.className = "decision-verdict";
    if (st === "extractable" && decision.would_frontrun && decision.net_profit_wei && decision.net_profit_wei !== "0") {
      const modeLabel = decision.probe_mode || "probe";
      verdict.textContent = `READ-ONLY EXTRACTABLE (${modeLabel}) · counterfactual replay est. ${formatInrFromWei(decision.net_profit_wei, currentEthInr(), { plus: true })} net`;
    } else {
      verdict.textContent = decision.verdict || st;
    }
    wrap.append(verdict);
  }

  const head = document.createElement("div");
  head.className = "fork-evidence-head";
  const title = document.createElement("span");
  title.textContent = "REVM & FORK EVIDENCE";
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
      ? `${fork.head} · lag n/a`
      : `${fork.head} · lag ${lag}${stale ? " ⚠" : ""}`;
  row("HEAD / LAG", headText, stale);
  row("GAS USED", hit.sim && hit.sim.gas_used ? Number(hit.sim.gas_used).toLocaleString() : shown(hit.gas));
  row(
    "NET PROFIT EST",
    decision && decision.net_profit_wei && decision.net_profit_wei !== "0"
      ? formatInrFromWei(decision.net_profit_wei, currentEthInr(), { plus: true })
      : "₹0.00"
  );
  if (telemetry) {
    row("REVM SIM", formatUs(telemetry.sim_us));
    row("PROBE SIM", formatUs(telemetry.probe_us));
  } else {
    const checked = Array.isArray(fork.checked) ? fork.checked : [];
    row("STABILITY", checked.length ? `ran · ${checked.join(" + ")}` : "not run");
    row("CODE", fork.code_present === true ? "present" : fork.code_present === false ? "absent" : "n/a");
  }

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
  const visible = state.hits.filter((hit) => matchesHitFilter(hit, state.filter));
  $("#activity-count").textContent = String(state.hits.length);
  $("#shown-count").textContent = String(visible.length);
  if (!visible.length) {
    const row = document.createElement("tr"); row.className = "empty-row";
    const cell = document.createElement("td"); cell.colSpan = 9;
    const mark = document.createElement("span"); mark.className = "empty-mark"; mark.textContent = "⌁";
    const title = document.createElement("b"); title.textContent = state.hits.length ? "No observations match this filter" : "Waiting for the first selector match";
    const note = document.createElement("small"); note.textContent = state.hits.length ? "Cycle filter back to ALL MATCHES to view every recorded hit." : "Only actual observations are shown here. No sample transactions are inserted.";
    cell.append(mark, title, note); row.append(cell); body.append(row); return;
  }

  for (const hit of visible.slice(0, 50)) {
    const row = document.createElement("tr");

    const name = document.createElement("td"); name.className = "function-name";
    const nameTag = document.createElement("span"); nameTag.className = "function-chip";
    nameTag.textContent = hit.name || "selector";
    name.append(nameTag);
    if (hit.decoded && hit.decoded.summary) {
      const sub = document.createElement("small");
      sub.className = "decoded-cell-sub";
      sub.textContent = hit.decoded.summary;
      name.append(sub);
    }
    row.append(name);

    const hash = document.createElement("td");
    const link = document.createElement("a");
    link.className = "tx-link";
    link.href = `https://etherscan.io/tx/${encodeURIComponent(hit.hash || "")}`;
    link.target = "_blank";
    link.rel = "noopener noreferrer";
    link.textContent = shortHash(hit.hash);
    hash.append(link);
    row.append(hash);

    const gasDisplayed = hit.sim && hit.sim.gas_used ? Number(hit.sim.gas_used) : Number(hit.gas || 0);
    row.append(
      makeCell(shortAddress(hit.from), "address"),
      makeCell(shortAddress(hit.to), "address"),
      makeCell(formatInrFromWei(rowEffectiveWei(hit).toString())),
      makeCell(gasDisplayed.toLocaleString()),
    );

    const probeCell = document.createElement("td");
    const dStatus = decisionStatus(hit);
    const pChip = document.createElement("span");
    pChip.className = "fk-chip";
    pChip.dataset.kind = dStatus;
    let probeLabel = PROBE_TEXT[dStatus] || "N/A";
    if (dStatus === "extractable" && hit.decision && hit.decision.would_frontrun && hit.decision.net_profit_wei !== "0") {
      probeLabel = formatInrFromWei(hit.decision.net_profit_wei, currentEthInr(), { plus: true });
    }
    pChip.textContent = probeLabel;
    probeCell.append(pChip);
    row.append(probeCell);

    const result = document.createElement("td");
    const chip = document.createElement("span");
    const ok = Boolean(hit.sim?.ok);
    const kind = labelKind(hit.label);
    chip.className = `result-chip${ok ? "" : " failed"}`;
    chip.dataset.kind = kind;
    const dot = document.createElement("i");
    const label = document.createElement("span");
    label.textContent = KIND_TEXT[kind] || (ok ? "CALL OK" : "SIM FAILED");
    chip.append(dot, label);
    result.append(chip);
    row.append(result);

    const time = document.createElement("td");
    const utcStr = new Date(hit.ts).toLocaleTimeString("en-GB", { hour: "2-digit", minute: "2-digit", second: "2-digit", timeZone: "UTC" });
    time.textContent = hit.telemetry && hit.telemetry.sim_us ? `${utcStr} · ${formatUs(hit.telemetry.sim_us)}` : utcStr;
    row.append(time);

    body.append(row);
  }
}

function syncFilterButtons() {
  const filterChip = $("#clear-filter");
  if (filterChip && filterChip.firstChild) {
    const label = FILTER_LABELS[state.filter] || "ALL MATCHES";
    filterChip.firstChild.textContent = `${label} `;
  }
  const topBtn = $("#ext-bar-filter-btn");
  if (topBtn) {
    const active = state.filter === "extractable";
    topBtn.dataset.filtering = active ? "true" : "false";
    topBtn.textContent = active ? "SHOW ALL MATCHES" : "VIEW EXTRACTABLE";
  }
}

function renderExtractableBar() {
  const bar = $("#extractable-bar");
  if (!bar) return;

  const qExt = state.quality && state.quality.extractable ? state.quality.extractable : null;
  const ethInr = currentEthInr();
  const fxSrc = String(state.quality?.fx?.source || state.fxSource || "reference");
  const activeHits = state.hits.filter((h) => !h.closed);
  const openHits = activeHits.filter((h) => decisionStatus(h) === "extractable");

  let openCount = openHits.length;
  let grossWei = 0n;
  let netWei = 0n;
  let recentFeedWei = 0n;
  let tokenGains = 0;

  for (const h of activeHits) {
    recentFeedWei += rowEffectiveWei(h);
  }

  for (const h of openHits) {
    const g = parsePositiveWei(h.decision?.gross_value_wei);
    const n = parsePositiveWei(h.decision?.net_profit_wei);
    const effGross = g > 0n ? g : n;
    if (effGross > 0n) grossWei += effGross;
    if (n > 0n) netWei += n;
    tokenGains += Number(h.decision?.probe_token_gains || 0);
  }

  let activeCount = activeHits.length;
  if (qExt && activeHits.length === 0) {
    openCount = Number(qExt.open_count) || 0;
    activeCount = Number(qExt.active_tx_count) || 0;
    grossWei = parsePositiveWei(qExt.total_gross_wei);
    netWei = parsePositiveWei(qExt.total_net_wei);
    recentFeedWei = parsePositiveWei(qExt.recent_tx_value_wei);
    tokenGains = Number(qExt.token_transfers || 0);
  }

  const closedRemoved = qExt ? Number(qExt.closed_removed || 0) : 0;

  bar.dataset.active = openCount > 0 ? "true" : "false";
  $("#ext-bar-count").textContent = `${openCount.toLocaleString()} OPEN`;
  $("#ext-bar-total").textContent = formatInrFromWei(grossWei.toString(), ethInr);
  const rateNote = $("#ext-bar-rate");
  if (rateNote) {
    rateNote.textContent = fxSrc !== "reference" ? "LIVE INR" : "INR";
  }
  $("#ext-bar-net").textContent = netWei > 0n
    ? formatInrFromWei(netWei.toString(), ethInr, { plus: true })
    : "₹0.00";

  const feedValEl = $("#ext-bar-feed-val");
  if (feedValEl) {
    feedValEl.textContent = formatInrFromWei(recentFeedWei.toString(), ethInr);
  }
  const feedSubEl = $("#ext-bar-feed-sub");
  if (feedSubEl) {
    feedSubEl.textContent = `${activeCount.toLocaleString()} ACTIVE CALL${activeCount === 1 ? "" : "S"}`;
  }

  const tokenNote = $("#ext-bar-tokens");
  if (openCount === 0) {
    tokenNote.textContent = "0 LIVE OPPORTUNITIES";
  } else if (tokenGains > 0) {
    tokenNote.textContent = `${tokenGains} ERC-20 TRANSFER${tokenGains === 1 ? "" : "S"} CAPTURED`;
  } else {
    tokenNote.textContent = `${openCount} UNGUARDED CALL${openCount === 1 ? "" : "S"} PENDING`;
  }

  const purgedEl = $("#ext-bar-purged");
  if (purgedEl) {
    purgedEl.textContent = closedRemoved > 0
      ? `${closedRemoved.toLocaleString()} CLOSED / USER-SETTLED REMOVED`
      : "CLOSED / USER-SETTLED TXS AUTO-REMOVED";
  }
  syncFilterButtons();
}

function render() {
  const total = state.hits.length;
  const success = state.hits.filter((hit) => Boolean(hit.sim?.ok)).length;
  const extractable = state.hits.filter((hit) => decisionStatus(hit) === "extractable").length;
  const wouldFrontrun = state.hits.filter((hit) => Boolean(hit.decision?.would_frontrun)).length;

  $("#metric-total").textContent = total.toLocaleString();
  $("#metric-ok").textContent = success.toLocaleString();
  $("#metric-extractable").textContent = extractable.toLocaleString();
  $("#metric-extractable-sub").textContent = wouldFrontrun
    ? `${wouldFrontrun} positive net value in replay`
    : "closed & user-locked txs auto-removed";

  const latest = state.hits[0];
  $("#metric-last").textContent = latest ? relativeTime(latest.ts) : "—";
  $("#metric-last-sub").textContent = latest ? `${latest.name || "selector"} · ${shortHash(latest.hash)}` : "Waiting for a selector match";
  renderExtractableBar();
  renderLatest(latest);
  renderTable();
}

async function refreshQuality() {
  try {
    const response = await fetch(`/api/quality?limit=100&filter=${encodeURIComponent(state.filter)}`, { cache: "no-store", headers: { Accept: "application/json" } });
    if (!response.ok) throw new Error(`quality feed returned ${response.status}`);
    state.quality = await response.json();
    const qRate = Number(state.quality?.fx?.eth_inr || 0);
    if (Number.isFinite(qRate) && qRate > 0) {
      state.ethInr = qRate;
      state.fxSource = String(state.quality?.fx?.source || "reference");
    }
  } catch {
    state.quality = null;
  }
  renderExtractableBar();
  renderBanner(state.quality);
  renderRollup(state.quality);
  const latest = state.hits[0];
  if (latest) renderLatest(latest);
  renderTable();
}

async function refresh() {
  try {
    const response = await fetch("/api/hits?limit=100", { cache: "no-store", headers: { Accept: "application/json" } });
    if (!response.ok) throw new Error(`feed returned ${response.status}`);
    const payload = await response.json();
    if (!Array.isArray(payload.hits)) throw new Error("invalid feed response");
    if (payload.fx && Number(payload.fx.eth_inr) > 0) {
      state.ethInr = Number(payload.fx.eth_inr);
      state.fxSource = String(payload.fx.source || "reference");
    }
    state.hits = payload.hits.filter((h) => !h.closed && decisionStatus(h) !== "beneficiary_locked");
    setOnline(true);
    $("#updated-at").textContent = `UPDATED ${new Date().toLocaleTimeString("en-GB", { hour: "2-digit", minute: "2-digit", second: "2-digit", timeZone: "UTC" })} UTC`;
    render();
  } catch {
    setOnline(false, "API unavailable · retrying");
    $("#updated-at").textContent = "FEED RETRYING";
  }
  await refreshQuality();
}

$("#refresh-button").addEventListener("click", refresh);
$("#clear-filter").addEventListener("click", () => {
  const nextIdx = (FILTER_CYCLE.indexOf(state.filter) + 1) % FILTER_CYCLE.length;
  state.filter = FILTER_CYCLE[nextIdx];
  syncFilterButtons();
  renderTable();
  refreshQuality();
});
const topExtBtn = $("#ext-bar-filter-btn");
if (topExtBtn) {
  topExtBtn.addEventListener("click", () => {
    state.filter = state.filter === "extractable" ? "all" : "extractable";
    syncFilterButtons();
    renderTable();
    refreshQuality();
    const act = $("#activity");
    if (act && state.filter === "extractable") {
      act.scrollIntoView({ behavior: "smooth", block: "start" });
    }
  });
}

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
