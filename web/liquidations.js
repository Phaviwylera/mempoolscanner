(() => {
  'use strict';

  const POLL_MS = 8000;
  const FILTERS = ['all', 'at_risk', 'liquidatable'];
  const FILTER_LABELS = {
    all: 'ALL BORROWERS',
    at_risk: 'AT-RISK (HF < 1.20)',
    liquidatable: 'LIQUIDATABLE (HF < 1.00)',
  };

  const ALICE_PRESET = {
    label: 'Alice (5 ETH / 6,000 USDC)',
    isPreset: true,
    collateralUsd: 10000.0,
    debtUsd: 6000.0,
    liqThresholdPct: 80.0,
    ethBaseUsd: 2000.0,
  };

  const state = {
    filterIndex: 0,
    fx: {
      eth_inr: 260000.0,
      eth_usd: 2650.0,
      usd_inr: 86.0,
      source: 'reference',
    },
    baseFeeGwei: 1.0,
    positions: [],
    simTarget: { ...ALICE_PRESET },
    shockPct: -30,
    closeFactor: 0.5,
    // Wallet / execution state
    wallet: {
      connected: false,
      address: null,
      chainId: null,
    },
    execMode: false, // true when --execute is active on the server
    auditEntries: [],
  };

  const inrFmt = new Intl.NumberFormat('en-IN', {
    minimumFractionDigits: 2,
    maximumFractionDigits: 2,
  });
  const usdFmt = new Intl.NumberFormat('en-US', {
    minimumFractionDigits: 2,
    maximumFractionDigits: 2,
  });

  const $ = (id) => document.getElementById(id);

  function formatInrFromUsd(usd, { plus = false } = {}) {
    const n = Number.isFinite(usd) ? usd : 0;
    const inr = n * (state.fx.usd_inr || 86.0);
    const sign = inr < 0 ? '-' : plus && inr > 0 ? '+' : '';
    return `${sign}₹${inrFmt.format(Math.abs(inr))}`;
  }

  function formatUsd(usd, { plus = false } = {}) {
    const n = Number.isFinite(usd) ? usd : 0;
    const sign = n < 0 ? '-' : plus && n > 0 ? '+' : '';
    return `${sign}$${usdFmt.format(Math.abs(n))}`;
  }

  function shortHex(hex) {
    if (!hex || typeof hex !== 'string') return '—';
    if (hex.length <= 14) return hex;
    return `${hex.slice(0, 8)}…${hex.slice(-4)}`;
  }

  // --- Wallet connection (MetaMask / injected provider) ---
async function connectWallet() {
  if (!window.ethereum) {
    alert('MetaMask not detected. Please install MetaMask to connect a wallet.');
    return;
  }
  try {
    const accounts = await window.ethereum.request({ method: 'eth_requestAccounts' });
    const chainId = await window.ethereum.request({ method: 'eth_chainId' });
    state.wallet.connected = true;
    state.wallet.address = accounts[0];
    state.wallet.chainId = parseInt(chainId, 16);
    updateWalletUI();
    // Listen for account changes
    window.ethereum.on('accountsChanged', (accs) => {
      if (accs.length === 0) {
        disconnectWallet();
      } else {
        state.wallet.address = accs[0];
        updateWalletUI();
      }
    });
    window.ethereum.on('chainChanged', () => window.location.reload());
    infoLog('Wallet connected: ' + shortHex(state.wallet.address));
  } catch (err) {
    console.error('Wallet connect failed:', err);
    alert('Failed to connect wallet: ' + err.message);
  }
}

function disconnectWallet() {
  state.wallet.connected = false;
  state.wallet.address = null;
  state.wallet.chainId = null;
  updateWalletUI();
  infoLog('Wallet disconnected');
}

function updateWalletUI() {
  const connectBtn = $('wallet-connect');
  const disconnectBtn = $('wallet-disconnect');
  const walletBar = $('wallet-bar');
  const walletAddr = $('wallet-address');
  if (!connectBtn || !disconnectBtn || !walletBar || !walletAddr) return;

  if (state.wallet.connected && state.wallet.address) {
    connectBtn.style.display = 'none';
    walletBar.style.display = 'flex';
    walletAddr.textContent = shortHex(state.wallet.address);
    // Show chain
    const chainName = state.wallet.chainId === 1 ? 'ETHEREUM MAINNET' : 'CHAIN ' + state.wallet.chainId;
    walletAddr.title = chainName;
  } else {
    connectBtn.style.display = 'inline-block';
    walletBar.style.display = 'none';
    walletAddr.textContent = '—';
  }
}

function infoLog(msg) {
  const el = $('liq-feed-status');
  if (el) {
    const b = el.querySelector('b');
    if (b) b.textContent = msg;
  }
}

// --- Execution: call POST /api/execute ---
async function executeLiquidation(opportunityId) {
  if (!state.wallet.connected || !state.wallet.address) {
    alert('Please connect your wallet before executing.');
    return;
  }
  if (!confirm('Execute liquidation for ' + shortHex(opportunityId) + '?\nThis will sign and broadcast via Flashbots/MEV-Share using the operator key. Real funds at risk.')) {
    return;
  }
  try {
    const resp = await fetch('/api/execute', {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ opportunity_id: opportunityId }),
    });
    const data = await resp.json();
    if (data.success) {
      infoLog('EXECUTED: ' + shortHex(data.tx_hash || '—') + ' · ' + data.outcome);
      fetchAuditLogs();
    } else {
      alert('Execution failed: ' + (data.error || 'unknown error'));
    }
  } catch (err) {
    alert('Execution request failed: ' + err.message);
  }
}

// --- Audit log ---
async function fetchAuditLogs() {
  try {
    const resp = await fetch('/api/audit-logs', { cache: 'no-store' });
    if (!resp.ok) return;
    const data = await resp.json();
    state.auditEntries = Array.isArray(data.entries) ? data.entries : [];
    renderAuditLogs();
  } catch (_err) {
    // silent
  }
}

function renderAuditLogs() {
  const body = $('audit-body');
  const countEl = $('audit-count');
  if (!body || !countEl) return;
  body.replaceChildren();
  countEl.textContent = state.auditEntries.length + ' entries';

  for (const entry of state.auditEntries.slice(0, 50).reverse()) {
    const tr = document.createElement('tr');
    const ts = document.createElement('td');
    ts.textContent = entry.timestamp ? entry.timestamp.slice(11, 19) : '—';
    const id = document.createElement('td');
    id.textContent = shortHex(entry.opportunity_id);
    const chain = document.createElement('td');
    chain.textContent = entry.chain_id || '1';
    const profit = document.createElement('td');
    profit.textContent = entry.profit_estimate_wei ? '+' + entry.profit_estimate_wei + ' wei' : '—';
    const tx = document.createElement('td');
    if (entry.tx_hash) {
      const a = document.createElement('a');
      a.className = 'tx-link';
      a.href = 'https://etherscan.io/tx/' + encodeURIComponent(entry.tx_hash);
      a.target = '_blank';
      a.rel = 'noopener noreferrer';
      a.textContent = shortHex(entry.tx_hash);
      tx.appendChild(a);
    } else {
      tx.textContent = '—';
    }
    const outcome = document.createElement('td');
    outcome.textContent = entry.outcome || '—';
    outcome.className = entry.outcome === 'confirmed' ? 'liq-bounty-pos' : (entry.outcome === 'dry_run_signed' ? 'liq-bounty-proj' : '');
    tr.append(ts, id, chain, profit, tx, outcome);
    body.appendChild(tr);
  }
}

function hfBand(hf) {
    if (!Number.isFinite(hf)) return 'healthy';
    if (hf < 1.0) return 'liquidatable';
    if (hf < 1.05) return 'critical';
    if (hf < 1.2) return 'at_risk';
    return 'healthy';
  }

  function hfWidthClass(hf) {
    // Lower HF = closer to liquidation = fuller urgency bar, or map 0.8..2.0 cleanly:
    // Map safety headroom from HF 0.80 -> 15% up to HF >= 2.0 -> 100%.
    if (!Number.isFinite(hf)) return 'w-100';
    if (hf < 1.0) return 'w-100';
    const pct = Math.max(10, Math.min(100, Math.round(((Math.min(hf, 2.2) - 0.8) / 1.4) * 20) * 5));
    return `w-${pct}`;
  }

  function setText(id, val) {
    const el = $(id);
    if (el) el.textContent = val;
  }

  function renderSimulator() {
    const target = state.simTarget;
    const shockMult = Math.max(0.05, 1 + state.shockPct / 100);
    const baseEthUsd = target.isPreset
      ? target.ethBaseUsd
      : state.fx.eth_usd > 100
        ? state.fx.eth_usd
        : 2650.0;

    const shockedEthUsd = baseEthUsd * shockMult;
    const shockedColUsd = target.collateralUsd * shockMult;
    const debtUsd = target.debtUsd;
    const ltRate = (target.liqThresholdPct || 80.0) / 100.0;
    const hf = debtUsd > 0 ? (shockedColUsd * ltRate) / debtUsd : 99.99;
    const triggerEthUsd =
      debtUsd > 0 && target.collateralUsd > 0 && ltRate > 0
        ? baseEthUsd * (debtUsd / (target.collateralUsd * ltRate))
        : 0;

    setText('sim-mode-label', target.isPreset ? 'ALICE PRESET' : 'MAINNET BORROWER');
    setText('sim-target-label', target.label);
    setText('sim-target-lt', `LIQ. THRESHOLD ${target.liqThresholdPct.toFixed(1)}%`);
    setText('sim-col-inr', formatInrFromUsd(shockedColUsd));
    setText('sim-col-usd', `${formatUsd(shockedColUsd)} (${state.shockPct >= 0 ? '+' : ''}${state.shockPct}%)`);
    setText('sim-debt-inr', formatInrFromUsd(debtUsd));
    setText('sim-debt-usd', formatUsd(debtUsd));

    const hfStr = hf >= 50 ? '∞ (NO DEBT)' : hf.toFixed(4);
    const hfEl = $('sim-hf-val');
    if (hfEl) {
      hfEl.textContent = hf < 1.0 ? `${hfStr} · LIQUIDATABLE` : hf <= 1.005 ? `${hfStr} · BORDERLINE` : `${hfStr} · SAFE`;
      hfEl.className = 'liq-hf-num';
      hfEl.dataset.band = hfBand(hf);
    }
    setText(
      'sim-trigger-note',
      triggerEthUsd > 0
        ? `CROSSES 1.00 AT ${formatInrFromUsd(triggerEthUsd)} (${formatUsd(triggerEthUsd)})`
        : 'NO ACTIVE DEBT'
    );

    const sign = state.shockPct > 0 ? '+' : '';
    setText(
      'sim-shock-readout',
      `${sign}${state.shockPct}% · ETH ${formatInrFromUsd(shockedEthUsd)} (${formatUsd(shockedEthUsd)})`
    );

    const bonusRate = 0.05;
    const flashFeeRate = 0.0005;
    const dexFeeRate = 0.001;
    const gasEth = (380000 * Math.max(0.1, state.baseFeeGwei)) / 1e9;
    const gasUsd = gasEth * shockedEthUsd;

    const cfBtn = $('sim-cf-btn');
    if (cfBtn) {
      cfBtn.textContent =
        state.closeFactor >= 0.99
          ? 'CLOSE FACTOR: 100% (HF < 0.95 MAX)'
          : 'CLOSE FACTOR: 50% (AAVE STD)';
      cfBtn.dataset.active = state.closeFactor >= 0.99 ? 'true' : 'false';
    }

    const canLiq = debtUsd > 0 && hf < 1.0;
    const effectiveCf = state.closeFactor;
    const maxByCf = debtUsd * effectiveCf;
    const maxByCol = shockedColUsd / (1 + bonusRate);
    const debtToCover = canLiq ? Math.max(0, Math.min(maxByCf, maxByCol)) : 0;
    const seizedUsd = canLiq ? Math.min(shockedColUsd, debtToCover * (1 + bonusRate)) : 0;
    const grossBountyUsd = canLiq ? Math.max(0, seizedUsd - debtToCover) : 0;
    const dexFeeUsd = canLiq ? seizedUsd * dexFeeRate : 0;
    const flashFeeUsd = canLiq ? debtToCover * flashFeeRate : 0;
    const netProfitUsd = canLiq ? grossBountyUsd - flashFeeUsd - dexFeeUsd - gasUsd : 0;

    setText('sim-step1-inr', formatInrFromUsd(debtToCover));
    setText(
      'sim-step1-usd',
      canLiq
        ? `${formatUsd(debtToCover)} flash loan (${Math.round(effectiveCf * 100)}% close factor)`
        : 'Waiting for HF < 1.0000'
    );

    setText('sim-step2-inr', formatInrFromUsd(seizedUsd));
    setText(
      'sim-step2-usd',
      canLiq
        ? `${formatUsd(seizedUsd)} seized (${formatUsd(grossBountyUsd, { plus: true })} 5% bonus)`
        : 'Pool.liquidationCall reverts while HF >= 1.0'
    );

    setText('sim-step3-inr', formatInrFromUsd(seizedUsd - dexFeeUsd));
    setText(
      'sim-step3-usd',
      canLiq
        ? `Swap proceeds after ${formatUsd(dexFeeUsd)} Uniswap fee`
        : '0.10% DEX pool fee + slippage model'
    );

    setText('sim-step4-inr', formatInrFromUsd(debtToCover + flashFeeUsd));
    setText(
      'sim-step4-usd',
      canLiq
        ? `${formatUsd(debtToCover)} + ${formatUsd(flashFeeUsd)} fee (0.05%)`
        : '0.05% Aave V3 flash-loan premium'
    );

    const step5Row = $('sim-step5-row');
    if (!canLiq) {
      if (step5Row) step5Row.dataset.profitable = 'false';
      setText('sim-step5-title', 'Loan is currently overcollateralized (HF >= 1.0)');
      setText(
        'sim-step5-desc',
        triggerEthUsd > 0
          ? `Drag the slider left until ETH crosses ${formatUsd(triggerEthUsd)} (${formatInrFromUsd(triggerEthUsd)}) to trigger liquidation.`
          : 'Select a borrower with active debt to model liquidation.'
      );
      setText('sim-step5-inr', '₹0.00');
      setText('sim-step5-usd', 'Not liquidatable at this price');
    } else if (netProfitUsd > 0) {
      if (step5Row) step5Row.dataset.profitable = 'true';
      setText('sim-step5-title', 'Atomic flash-loan liquidation profitable (Read-Only)');
      setText(
        'sim-step5-desc',
        `Gross bounty ${formatInrFromUsd(grossBountyUsd, { plus: true })} minus flash fee, DEX swap, and ${formatInrFromUsd(gasUsd)} (${formatUsd(gasUsd)}) mainnet gas.`
      );
      setText('sim-step5-inr', formatInrFromUsd(netProfitUsd, { plus: true }));
      setText('sim-step5-usd', `${formatUsd(netProfitUsd, { plus: true })} net residual`);
    } else {
      if (step5Row) step5Row.dataset.profitable = 'false';
      setText('sim-step5-title', 'Unprofitable after gas & swap fees — atomic tx would revert');
      setText(
        'sim-step5-desc',
        `Position is below HF 1.0, but gas (${formatUsd(gasUsd)}) + swap fees exceed the 5% bounty (${formatUsd(grossBountyUsd)}).`
      );
      setText('sim-step5-inr', formatInrFromUsd(netProfitUsd));
      setText('sim-step5-usd', `${formatUsd(netProfitUsd)} (reverts safely)`);
    }
  }

  function loadPositionIntoSim(pos, shockOverride = 0) {
    if (!pos) return;
    state.simTarget = {
      label: shortHex(pos.user),
      isPreset: false,
      collateralUsd: Number(pos.collateral_usd) || 0,
      debtUsd: Number(pos.debt_usd) || 0,
      liqThresholdPct: Number(pos.liq_threshold_pct) || 80.0,
      ethBaseUsd: state.fx.eth_usd || 2650.0,
    };
    state.shockPct = shockOverride;
    const slider = $('sim-shock-range');
    if (slider) slider.value = String(shockOverride);

    const liveBtn = $('sim-load-live-btn');
    const aliceBtn = $('sim-load-alice-btn');
    if (liveBtn) liveBtn.dataset.active = 'true';
    if (aliceBtn) aliceBtn.dataset.active = 'false';

    renderSimulator();
    const simSec = $('simulator');
    if (simSec) simSec.scrollIntoView({ behavior: 'smooth', block: 'start' });
  }

  function buildPositionsRows(positions) {
    const tbody = $('liq-positions-body');
    if (!tbody) return;
    tbody.replaceChildren();

    if (!Array.isArray(positions) || positions.length === 0) {
      const tr = document.createElement('tr');
      tr.className = 'empty-row';
      const td = document.createElement('td');
      td.colSpan = 9;
      const mark = document.createElement('span');
      mark.className = 'empty-mark';
      mark.textContent = '⌁';
      const b = document.createElement('b');
      const curFilter = FILTERS[state.filterIndex];
      b.textContent =
        curFilter === 'liquidatable'
          ? '0 borrowers currently below HF 1.0000 on Aave V3 Mainnet'
          : curFilter === 'at_risk'
            ? 'No tracked borrowers currently below HF 1.20'
            : 'Scanning Aave V3 Ethereum Mainnet Pool logs for active borrowers…';
      const sm = document.createElement('small');
      sm.textContent =
        curFilter !== 'all'
          ? 'Switch filter to ALL BORROWERS to see all enumerated positions and simulate price drops.'
          : 'Active borrowers appear automatically within seconds as Borrow and Repay logs are indexed.';
      td.append(mark, b, sm);
      tr.appendChild(td);
      tbody.appendChild(tr);
      return;
    }

    for (const pos of positions) {
      const tr = document.createElement('tr');
      const hf = Number(pos.health_factor) || 99.99;
      const band = hfBand(hf);
      const plan = pos.plan || {};

      // 1. Borrower + reserve
      const tdUser = document.createElement('td');
      const link = document.createElement('a');
      link.className = 'tx-link';
      link.href = `https://etherscan.io/address/${encodeURIComponent(pos.user || '')}`;
      link.target = '_blank';
      link.rel = 'noopener noreferrer';
      link.textContent = shortHex(pos.user);
      tdUser.appendChild(link);
      if (pos.revm_verified) {
        const rtag = document.createElement('span');
        rtag.className = 'liq-revm-tag';
        rtag.textContent = 'REVM';
        rtag.title = `Verified in-process via Revm (${pos.revm_sim_ms || 0}ms)`;
        tdUser.appendChild(rtag);
      }
      const subRes = document.createElement('span');
      subRes.className = 'liq-sub';
      subRes.textContent = `${pos.last_reserve_symbol || 'AAVE V3'} · BLK ${pos.last_seen_block || '—'}`;
      tdUser.appendChild(subRes);

      // 2. Health Factor + bar
      const tdHf = document.createElement('td');
      const hfWrap = document.createElement('div');
      hfWrap.className = 'liq-hf-wrap';
      const hfTop = document.createElement('div');
      hfTop.className = 'liq-hf-top';
      const hfNum = document.createElement('span');
      hfNum.className = 'liq-hf-num';
      hfNum.dataset.band = band;
      hfNum.textContent = hf >= 50 ? '>50.0' : hf.toFixed(4);
      hfTop.appendChild(hfNum);
      const track = document.createElement('span');
      track.className = 'liq-bar-track';
      const fill = document.createElement('b');
      fill.className = `liq-bar-fill ${hfWidthClass(hf)}`;
      fill.dataset.band = band;
      track.appendChild(fill);
      hfWrap.append(hfTop, track);
      tdHf.appendChild(hfWrap);

      // 3. Collateral (INR + USD)
      const tdCol = document.createElement('td');
      const colInr = document.createElement('span');
      colInr.className = 'liq-val-inr';
      colInr.textContent = pos.collateral_inr || formatInrFromUsd(pos.collateral_usd || 0);
      const colUsd = document.createElement('span');
      colUsd.className = 'liq-sub';
      colUsd.textContent = formatUsd(Number(pos.collateral_usd) || 0);
      tdCol.append(colInr, colUsd);

      // 4. Debt (INR + USD)
      const tdDebt = document.createElement('td');
      const debtInr = document.createElement('span');
      debtInr.className = 'liq-val-inr';
      debtInr.textContent = pos.debt_inr || formatInrFromUsd(pos.debt_usd || 0);
      const debtUsd = document.createElement('span');
      debtUsd.className = 'liq-sub';
      debtUsd.textContent = formatUsd(Number(pos.debt_usd) || 0);
      tdDebt.append(debtInr, debtUsd);

      // 5. Liquidation threshold
      const tdLt = document.createElement('td');
      tdLt.textContent = `${Number(pos.liq_threshold_pct || 0).toFixed(2)}%`;
      const ltvSub = document.createElement('span');
      ltvSub.className = 'liq-sub';
      ltvSub.textContent = `MAX LTV ${Number(pos.ltv_pct || 0).toFixed(1)}%`;
      tdLt.appendChild(ltvSub);

      // 6. ETH Trigger / Drop %
      const tdTrig = document.createElement('td');
      if (hf < 1.0) {
        const crossed = document.createElement('span');
        crossed.className = 'liq-bounty-pos';
        crossed.textContent = 'CROSSED (< 1.00)';
        const trSub = document.createElement('span');
        trSub.className = 'liq-sub';
        trSub.textContent = pos.eth_liq_price_inr || 'LIQUIDATABLE NOW';
        tdTrig.append(crossed, trSub);
      } else {
        const dropSpan = document.createElement('span');
        dropSpan.textContent = `-${Number(pos.price_drop_pct_to_liq || 0).toFixed(2)}% DROP`;
        const trSub = document.createElement('span');
        trSub.className = 'liq-sub';
        trSub.textContent =
          Number(pos.eth_liq_price_usd) > 0
            ? `ETH @ ${pos.eth_liq_price_inr} (${formatUsd(pos.eth_liq_price_usd)})`
            : 'HIGH BUFFER';
        tdTrig.append(dropSpan, trSub);
      }

      // 7. Debt to cover (50%)
      const tdCover = document.createElement('td');
      const halfDebtUsd = (Number(pos.debt_usd) || 0) * 0.5;
      tdCover.textContent =
        hf < 1.0
          ? plan.debt_to_cover_inr || formatInrFromUsd(plan.debt_to_cover_usd || 0)
          : formatInrFromUsd(halfDebtUsd);
      const covSub = document.createElement('span');
      covSub.className = 'liq-sub';
      covSub.textContent =
        hf < 1.0 ? 'ACTIVE 50% CAP' : `${formatUsd(halfDebtUsd)} AT CROSSING`;
      tdCover.appendChild(covSub);

      // 8. Est. Flash Bounty (INR)
      const tdBounty = document.createElement('td');
      if (hf < 1.0) {
        const bVal = document.createElement('span');
        bVal.className = 'liq-bounty-pos';
        bVal.textContent = plan.net_profit_inr || '₹0.00';
        const bSub = document.createElement('span');
        bSub.className = 'liq-sub';
        bSub.textContent = `GROSS ${plan.gross_bounty_inr || '₹0.00'}`;
        tdBounty.append(bVal, bSub);
      } else {
        const pVal = document.createElement('span');
        pVal.className = 'liq-bounty-proj';
        pVal.textContent = plan.trigger_net_profit_inr || '₹0.00';
        const pSub = document.createElement('span');
        pSub.className = 'liq-sub';
        pSub.textContent = 'PROJECTED @ HF 0.99';
        tdBounty.append(pVal, pSub);
      }

      // 9. Status chip + Simulate button
      const tdAct = document.createElement('td');
      const chip = document.createElement('span');
      chip.className = 'liq-status-chip';
      chip.dataset.band = band;
      chip.textContent = band.replace('_', ' ');
      const simBtn = document.createElement('button');
      simBtn.type = 'button';
      simBtn.className = 'liq-sim-btn';
      simBtn.textContent = 'SIMULATE ↗';
      simBtn.addEventListener('click', () => {
        const autoShock =
          hf < 1.0
            ? 0
            : -Math.min(45, Math.ceil(Number(pos.price_drop_pct_to_liq || 5) + 1));
        loadPositionIntoSim(pos, autoShock);
      });
      tdAct.append(chip, simBtn);

      // 10. REALRUN button — only when liquidatable (HF < 1.0) and wallet connected
      const tdRun = document.createElement('td');
      const runBtn = document.createElement('button');
      runBtn.type = 'button';
      runBtn.className = 'liq-realrun-btn';
      const canRun = hf < 1.0 && state.wallet.connected;
      runBtn.disabled = !canRun;
      runBtn.textContent = canRun ? 'REALRUN' : state.wallet.connected ? 'NOT LIQUIDATABLE' : 'CONNECT WALLET';
      runBtn.title = canRun
        ? 'Sign and broadcast liquidation via Flashbots/MEV-Share (real funds at risk)'
        : hf < 1.0 ? 'Connect wallet to enable execution' : 'Position not liquidatable (HF >= 1.0)';
      runBtn.addEventListener('click', () => {
        if (canRun) {
          executeLiquidation(pos.user || '');
        }
      });
      tdRun.appendChild(runBtn);

      tr.append(tdUser, tdHf, tdCol, tdDebt, tdLt, tdTrig, tdCover, tdBounty, tdAct, tdRun);
      tbody.appendChild(tr);
    }
  }

  function buildEventsRows(events) {
    const tbody = $('liq-events-body');
    if (!tbody) return;
    tbody.replaceChildren();
    setText('liq-events-count', String(Array.isArray(events) ? events.length : 0));

    if (!Array.isArray(events) || events.length === 0) {
      const tr = document.createElement('tr');
      tr.className = 'empty-row';
      const td = document.createElement('td');
      td.colSpan = 7;
      const mark = document.createElement('span');
      mark.className = 'empty-mark';
      mark.textContent = '◆';
      const b = document.createElement('b');
      b.textContent = 'No LiquidationCall logs in the scanned Mainnet block window';
      const sm = document.createElement('small');
      sm.textContent =
        'Real on-chain Aave V3 LiquidationCall events appear here whenever a borrower crosses HF < 1.0 on Ethereum Mainnet.';
      td.append(mark, b, sm);
      tr.appendChild(td);
      tbody.appendChild(tr);
      return;
    }

    for (const ev of events) {
      const tr = document.createElement('tr');

      const tdBlk = document.createElement('td');
      tdBlk.textContent = `#${ev.block_number || '—'}`;

      const tdTx = document.createElement('td');
      if (ev.tx_hash) {
        const a = document.createElement('a');
        a.className = 'tx-link';
        a.href = `https://etherscan.io/tx/${encodeURIComponent(ev.tx_hash)}`;
        a.target = '_blank';
        a.rel = 'noopener noreferrer';
        a.textContent = shortHex(ev.tx_hash);
        tdTx.appendChild(a);
      } else {
        tdTx.textContent = '—';
      }

      const tdUser = document.createElement('td');
      const ua = document.createElement('a');
      ua.className = 'tx-link';
      ua.href = `https://etherscan.io/address/${encodeURIComponent(ev.user || '')}`;
      ua.target = '_blank';
      ua.rel = 'noopener noreferrer';
      ua.textContent = shortHex(ev.user);
      tdUser.appendChild(ua);

      const tdLiq = document.createElement('td');
      const la = document.createElement('a');
      la.className = 'address tx-link';
      la.href = `https://etherscan.io/address/${encodeURIComponent(ev.liquidator || '')}`;
      la.target = '_blank';
      la.rel = 'noopener noreferrer';
      la.textContent = shortHex(ev.liquidator);
      tdLiq.appendChild(la);

      const tdDebt = document.createElement('td');
      tdDebt.textContent = ev.debt_to_cover_fmt || '—';

      const tdCol = document.createElement('td');
      tdCol.textContent = ev.liquidated_collateral_fmt || '—';

      const tdInr = document.createElement('td');
      tdInr.className = 'liq-val-inr';
      tdInr.textContent = ev.debt_to_cover_inr || '—';

      tr.append(tdBlk, tdTx, tdUser, tdLiq, tdDebt, tdCol, tdInr);
      tbody.appendChild(tr);
    }
  }

  function applyPayload(data) {
    if (!data || typeof data !== 'object') return;
    if (data.fx && typeof data.fx === 'object') {
      state.fx = {
        eth_inr: Number(data.fx.eth_inr) || 260000.0,
        eth_usd: Number(data.fx.eth_usd) || 2650.0,
        usd_inr: Number(data.fx.usd_inr) || 86.0,
        eth_inr_formatted: data.fx.eth_inr_formatted || '—',
        source: data.fx.source || 'reference',
      };
    }
    state.baseFeeGwei = Number(data.base_fee_gwei) || 1.0;
    state.positions = Array.isArray(data.positions) ? data.positions : [];

    const sum = data.summary || {};
    const liqOpen = Number(sum.liquidatable_count) || 0;
    const atRisk = Number(sum.at_risk_count) || 0;
    const tracked = Number(sum.tracked_count) || 0;

    // Top status bar
    const bar = $('liq-status-bar');
    if (bar) bar.dataset.active = liqOpen > 0 ? 'true' : 'false';
    setText('liq-bar-open', `${liqOpen} OPEN`);
    setText('liq-bar-bounty', sum.total_liquidatable_bounty_inr || '₹0.00');
    setText(
      'liq-bar-net',
      liqOpen > 0
        ? `NET ${sum.total_liquidatable_net_inr || '₹0.00'} AFTER FEES`
        : 'WAITING FOR HF < 1.0000'
    );
    setText('liq-bar-atrisk', sum.at_risk_debt_inr || '₹0.00');
    setText('liq-bar-atrisk-sub', `${atRisk} WATCHED POSITIONS`);
    setText('liq-bar-debt', sum.total_tracked_debt_inr || '₹0.00');
    setText(
      'liq-bar-oracle',
      `1 ETH = ${state.fx.eth_inr_formatted || formatInrFromUsd(state.fx.eth_usd)} (${formatUsd(state.fx.eth_usd)})`
    );

    // Feed status + sync banner
    const feedText = $('liq-feed-status')?.querySelector('b');
    if (feedText) {
      feedText.textContent =
        data.status === 'live'
          ? 'AAVE V3 LIVE'
          : data.status === 'backfilling'
            ? 'BACKFILLING LOGS'
            : 'SYNCING POOL';
    }

    const banner = $('liq-sync-banner');
    if (banner) {
      banner.dataset.state =
        tracked > 0 ? (liqOpen > 0 ? 'amber' : 'green') : 'unknown';
    }
    setText(
      'liq-banner-state',
      data.status === 'live'
        ? 'AAVE V3 POOL: LIVE'
        : data.status === 'backfilling'
          ? 'AAVE V3 POOL: BACKFILLING'
          : 'AAVE V3 POOL: SYNCING'
    );
    setText(
      'liq-banner-text',
      tracked > 0
        ? `Watching ${tracked} active debt positions (discovered ${data.discovered_borrowers_total || tracked} wallets across ${data.scanned_blocks_total || 0} blocks).`
        : 'Scanning Ethereum Mainnet Aave V3 Borrow, Repay, and LiquidationCall events…'
    );
    setText(
      'liq-banner-meta',
      data.head_block ? `HEAD #${data.head_block} · BASE ${state.baseFeeGwei.toFixed(2)} GWEI` : 'CONNECTING'
    );

    // 4 Metric cards
    setText('liq-metric-tracked', String(tracked));
    setText(
      'liq-metric-tracked-sub',
      `collateral ${sum.total_tracked_collateral_inr || '₹0.00'}`
    );
    const lowHf = sum.lowest_hf;
    setText(
      'liq-metric-lowest-hf',
      lowHf !== null && lowHf !== undefined ? Number(lowHf).toFixed(4) : '—'
    );
    if (state.positions.length > 0 && lowHf !== null && lowHf !== undefined) {
      const first = state.positions[0];
      setText(
        'liq-metric-lowest-sub',
        Number(lowHf) < 1.0
          ? `${shortHex(first.user)} is liquidatable now`
          : `${shortHex(first.user)} · -${Number(first.price_drop_pct_to_liq || 0).toFixed(2)}% to HF 1.0`
      );
    }
    setText('liq-metric-candidates', `${liqOpen} / ${atRisk}`);
    setText(
      'liq-metric-candidates-sub',
      `${liqOpen} liquidatable (<1.0) · ${atRisk} under 1.20`
    );
    setText(
      'liq-metric-eth',
      state.fx.eth_inr_formatted || formatInrFromUsd(state.fx.eth_usd)
    );
    setText(
      'liq-metric-eth-sub',
      `Oracle ${formatUsd(state.fx.eth_usd)} · FX ${state.fx.source.toUpperCase()}`
    );

    // Positions & Events tables
    setText('liq-pos-count', String(tracked));
    setText('liq-shown-count', String(state.positions.length));
    if (data.updated_ts) {
      setText('liq-updated-at', `UPDATED ${data.updated_ts.slice(11, 19)} UTC`);
    }
    if (data.scanned_blocks_total) {
      setText('liq-events-range', `SCANNED ${data.scanned_blocks_total} MAINNET BLOCKS`);
    }

    buildPositionsRows(state.positions);
    buildEventsRows(data.recent_liquidations || []);
    renderSimulator();
  }

  async function refreshData() {
    const filter = FILTERS[state.filterIndex];
    try {
      const resp = await fetch(`/api/liquidations?limit=80&filter=${encodeURIComponent(filter)}`, {
        cache: 'no-store',
      });
      if (!resp.ok) throw new Error(`HTTP ${resp.status}`);
      const json = await resp.json();
      applyPayload(json);
    } catch (_err) {
      const feedText = $('liq-feed-status')?.querySelector('b');
      if (feedText) feedText.textContent = 'API RETRYING';
    }
  }

  function renderInspectResult(ins) {
    const box = $('liq-inspect-output');
    if (!box) return;
    box.replaceChildren();

    if (!ins || !ins.ok) {
      box.textContent = (ins && ins.error) || 'Unable to query address on Aave V3 Mainnet.';
      return;
    }

    const title = document.createElement('div');
    title.textContent = `WALLET ${shortHex(ins.user)} · STATUS: ${String(ins.status || '').toUpperCase()}`;

    const line1 = document.createElement('div');
    line1.textContent = `COLLATERAL: ${ins.collateral_inr} (${formatUsd(ins.collateral_usd)}) · DEBT: ${ins.debt_inr} (${formatUsd(ins.debt_usd)})`;

    const line2 = document.createElement('div');
    const hfText = ins.has_debt ? Number(ins.health_factor).toFixed(4) : '∞ (NO ACTIVE DEBT)';
    line2.textContent = `HEALTH FACTOR: ${hfText} · LIQ THRESHOLD: ${Number(ins.liq_threshold_pct || 0).toFixed(2)}%`;

    box.append(title, line1, line2);

    if (ins.collateral_usd > 0 || ins.debt_usd > 0) {
      const btn = document.createElement('button');
      btn.type = 'button';
      btn.className = 'liq-sim-btn';
      btn.textContent = 'LOAD WALLET IN SIMULATOR ↗';
      btn.addEventListener('click', () => {
        const autoShock =
          ins.has_debt && ins.health_factor >= 1.0
            ? -Math.min(45, Math.ceil(Number(ins.price_drop_pct_to_liq || 5) + 1))
            : 0;
        loadPositionIntoSim(ins, autoShock);
      });
      box.appendChild(btn);
    }
  }

  function cycleFilter() {
    state.filterIndex = (state.filterIndex + 1) % FILTERS.length;
    const key = FILTERS[state.filterIndex];
    const label = FILTER_LABELS[key];
    setText('liq-filter-btn', `FILTER: ${label}`);
    const tblBtn = $('liq-table-filter-btn');
    if (tblBtn) {
      tblBtn.replaceChildren();
      tblBtn.append(document.createTextNode(`${label} `));
      const sp = document.createElement('span');
      sp.textContent = '⌄';
      tblBtn.appendChild(sp);
    }
    refreshData();
  }

  function wireControls() {
    $('liq-refresh-btn')?.addEventListener('click', () => refreshData());
    $('liq-filter-btn')?.addEventListener('click', cycleFilter);
    $('liq-table-filter-btn')?.addEventListener('click', cycleFilter);

    // Wallet connect / disconnect
    $('wallet-connect')?.addEventListener('click', connectWallet);
    $('wallet-disconnect')?.addEventListener('click', disconnectWallet);

    // Audit log toggle
    const auditToggle = $('audit-toggle');
    const auditPanel = $('audit-panel');
    if (auditToggle && auditPanel) {
      auditToggle.addEventListener('click', () => {
        const showing = auditPanel.style.display !== 'none';
        auditPanel.style.display = showing ? 'none' : 'block';
        if (!showing) fetchAuditLogs();
        auditToggle.textContent = showing ? 'AUDIT ↓' : 'AUDIT ↑';
      });
    }

    // Audit log download
    $('audit-download')?.addEventListener('click', () => {
      window.location.href = '/api/audit-logs/download';
    });

    // Poll audit logs every 10 seconds
    setInterval(fetchAuditLogs, 10000);

    const slider = $('sim-shock-range');
    slider?.addEventListener('input', () => {
      state.shockPct = Number(slider.value) || 0;
      renderSimulator();
    });

    for (const btn of document.querySelectorAll('.sim-shock-preset')) {
      btn.addEventListener('click', () => {
        const v = Number(btn.getAttribute('data-shock')) || 0;
        state.shockPct = v;
        if (slider) slider.value = String(v);
        renderSimulator();
      });
    }

    $('sim-load-alice-btn')?.addEventListener('click', () => {
      state.simTarget = { ...ALICE_PRESET };
      state.shockPct = -30;
      if (slider) slider.value = '-30';
      const liveBtn = $('sim-load-live-btn');
      const aliceBtn = $('sim-load-alice-btn');
      if (liveBtn) liveBtn.dataset.active = 'false';
      if (aliceBtn) aliceBtn.dataset.active = 'true';
      renderSimulator();
    });

    $('sim-load-live-btn')?.addEventListener('click', () => {
      if (state.positions.length > 0) {
        const top = state.positions[0];
        const autoShock =
          Number(top.health_factor) < 1.0
            ? 0
            : -Math.min(45, Math.ceil(Number(top.price_drop_pct_to_liq || 5) + 1));
        loadPositionIntoSim(top, autoShock);
      }
    });

    $('sim-cf-btn')?.addEventListener('click', () => {
      state.closeFactor = state.closeFactor >= 0.99 ? 0.5 : 1.0;
      renderSimulator();
    });

    $('liq-inspect-form')?.addEventListener('submit', async (ev) => {
      ev.preventDefault();
      const input = $('liq-inspect-input');
      const addr = (input?.value || '').trim();
      if (!addr) return;
      const box = $('liq-inspect-output');
      if (box) box.textContent = `Querying Aave V3 Pool on Ethereum Mainnet for ${shortHex(addr)}…`;
      try {
        const resp = await fetch(`/api/liquidations?limit=5&inspect=${encodeURIComponent(addr)}`, {
          cache: 'no-store',
        });
        const data = await resp.json();
        renderInspectResult(data.inspected);
      } catch (_err) {
        if (box) box.textContent = 'Request failed — check network connection.';
      }
    });
  }

  wireControls();
  renderSimulator();
  refreshData();
  window.setInterval(refreshData, POLL_MS);
})();
