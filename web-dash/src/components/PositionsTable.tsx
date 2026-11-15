import { useMemo } from "react";
import { ArrowDownWideNarrow, LoaderCircle, FlaskConical, FileSignature } from "lucide-react";
import {
  BAND_BAR,
  BAND_TEXT,
  estBountyUsd,
  formatHf,
  formatUsd,
  hfBand,
  hfPercent,
  toBigInt,
  weiToEth,
} from "../lib/format";
import CopyableAddress from "./CopyableAddress";
import type { Row } from "../lib/api";

function StatusPill({ row }: { row: Row }) {
  const band = hfBand(row.health_factor);
  const label = row.status || (band === "risk" ? "liquidatable" : "healthy");
  const cls =
    band === "risk"
      ? "border-risk/40 bg-risk/10 text-risk"
      : band === "warn"
        ? "border-warn/40 bg-warn/10 text-warn"
        : "border-profit/40 bg-profit/10 text-profit";
  return (
    <span className={`rounded border px-1.5 py-0.5 text-[10px] uppercase ${cls}`}>
      {label}
    </span>
  );
}

function HealthBar({ hf }: { hf: number }) {
  const band = hfBand(hf);
  return (
    <div className="flex items-center gap-2">
      <span className={`w-16 shrink-0 tabular-nums ${BAND_TEXT[band]}`}>
        {formatHf(hf)}
      </span>
      <span className="h-1.5 w-full max-w-[90px] overflow-hidden rounded-full bg-edge">
        <span
          className={`block h-full rounded-full ${BAND_BAR[band]}`}
          style={{ width: `${hfPercent(hf)}%` }}
        />
      </span>
    </div>
  );
}

export default function PositionsTable({
  rows,
  selected,
  onSelect,
  onSimulate,
  simulatingUser,
  onBuildTx,
  buildingUser,
}: {
  rows: Row[];
  selected: string | null;
  onSelect: (user: string) => void;
  onSimulate: (user: string) => void;
  /** Address currently being simulated, for the per-row spinner. */
  simulatingUser: string | null;
  /** Open the manual tx builder for a borrower. */
  onBuildTx: (user: string) => void;
  /** Address currently having a payload built, for the per-row spinner. */
  buildingUser: string | null;
}) {
  // Sort by highest bounty, per spec. Keeper wei bounty (exact, BigInt) ranks
  // above the snapshot's USD estimate; ties fall back to lowest health factor.
  const sorted = useMemo(() => {
    return [...rows].sort((a, b) => {
      const aw = toBigInt(a.bountyWeiHex);
      const bw = toBigInt(b.bountyWeiHex);
      if (aw !== bw) return bw > aw ? 1 : -1;

      const au = a.plan?.gross_bounty_usd ?? 0;
      const bu = b.plan?.gross_bounty_usd ?? 0;
      if (au !== bu) return bu - au;

      return (a.health_factor ?? 0) - (b.health_factor ?? 0);
    });
  }, [rows]);

  return (
    <section className="rounded-lg border border-edge bg-card">
      <div className="flex items-baseline justify-between border-b border-edge px-4 py-3">
        <h2 className="text-[11px] uppercase tracking-widest text-muted">
          Tracked Aave V3 Positions
        </h2>
        <span className="flex items-center gap-1 text-[11px] text-muted">
          <ArrowDownWideNarrow size={12} />
          {sorted.length} rows · sorted by bounty
        </span>
      </div>

      <div className="max-h-[460px] overflow-auto">
        <table className="w-full border-collapse text-left text-xs">
          <thead className="sticky top-0 z-10 bg-card">
            <tr className="text-[10px] uppercase tracking-wider text-muted">
              <th className="px-4 py-2 font-normal">Borrower</th>
              <th className="px-3 py-2 font-normal">Health Factor</th>
              <th className="px-3 py-2 text-right font-normal">Collateral</th>
              <th className="px-3 py-2 text-right font-normal">Debt</th>
              <th className="px-3 py-2 text-right font-normal">Liq. Thr.</th>
              <th className="px-3 py-2 text-right font-normal">Est. Flash Bounty</th>
              <th className="px-3 py-2 font-normal">Status</th>
              <th className="px-3 py-2 text-right font-normal">Actions</th>
            </tr>
          </thead>
          <tbody>
            {sorted.length === 0 ? (
              <tr>
                <td colSpan={8} className="px-4 py-8 text-center text-muted">
                  No positions in the snapshot yet.
                </td>
              </tr>
            ) : null}

            {sorted.map((r) => {
              const isSel = selected !== null && selected === r.user;
              const busy = simulatingUser === r.user;
              const building = buildingUser === r.user;
              return (
                <tr
                  key={r.user}
                  onClick={() => onSelect(r.user)}
                  className={`cursor-pointer border-t border-edge/60 transition-colors ${
                    isSel ? "bg-edge/50" : "hover:bg-edge/25"
                  }`}
                >
                  <td className="px-4 py-2">
                    <CopyableAddress address={r.user} className="text-ink" />
                    <span className="ml-2 text-[10px] text-muted">
                      {r.last_reserve_symbol}
                    </span>
                  </td>
                  <td className="px-3 py-2">
                    <HealthBar hf={r.health_factor} />
                  </td>
                  <td className="px-3 py-2 text-right tabular-nums text-ink">
                    ${formatUsd(r.collateral_usd)}
                  </td>
                  <td className="px-3 py-2 text-right tabular-nums text-ink">
                    ${formatUsd(r.debt_usd)}
                  </td>
                  <td className="px-3 py-2 text-right tabular-nums text-muted">
                    {r.liq_threshold_pct?.toFixed(2)}%
                  </td>
                  <td className="px-3 py-2 text-right tabular-nums">
                    {r.bountyWeiHex && toBigInt(r.bountyWeiHex) > 0n ? (
                      <span
                        className="text-profit"
                        title="Exact live bounty from the keeper (wei)"
                      >
                        {weiToEth(r.bountyWeiHex, 6)} ETH
                      </span>
                    ) : (
                      (() => {
                        // Prefer the snapshot plan's computed bounty; otherwise
                        // estimate from the 5% Aave liquidation bonus on the
                        // seizable collateral (min of debt/collateral).
                        const plan = r.plan?.gross_bounty_usd;
                        const usd =
                          typeof plan === "number" && plan > 0
                            ? plan
                            : estBountyUsd(r.debt_usd, r.collateral_usd);
                        return (
                          <span
                            className="text-profit"
                            title="Estimated flash bounty: 5% Aave liquidation bonus on seizable collateral. The exact figure comes from the keeper once the borrower is tracked."
                          >
                            ${formatUsd(usd)}
                            <span className="ml-1 text-[9px] uppercase text-muted">
                              est
                            </span>
                          </span>
                        );
                      })()
                    )}
                  </td>
                  <td className="px-3 py-2">
                    <StatusPill row={r} />
                  </td>
                  <td className="px-3 py-2">
                    <div className="flex items-center justify-end gap-1.5">
                      <button
                        onClick={(e) => {
                          // Do not let the click also re-trigger row selection.
                          e.stopPropagation();
                          onSimulate(r.user);
                        }}
                        disabled={busy}
                        title="Run a revm fork simulation for this borrower"
                        className="inline-flex items-center gap-1 rounded border border-edge bg-edge/40 px-2 py-1 text-[10px] text-ink transition-colors hover:bg-edge disabled:opacity-50"
                      >
                        {busy ? (
                          <LoaderCircle size={11} className="animate-spin" />
                        ) : (
                          <FlaskConical size={11} />
                        )}
                        {busy ? "Simulating" : "Simulate"}
                      </button>
                      <button
                        onClick={(e) => {
                          e.stopPropagation();
                          onBuildTx(r.user);
                        }}
                        disabled={building}
                        title="Build & sign a liquidation payload offline (not broadcast)"
                        className="inline-flex items-center gap-1 rounded border border-edge bg-edge/40 px-2 py-1 text-[10px] text-ink transition-colors hover:bg-edge disabled:opacity-50"
                      >
                        {building ? (
                          <LoaderCircle size={11} className="animate-spin" />
                        ) : (
                          <FileSignature size={11} />
                        )}
                        {building ? "Building" : "Generate Payload"}
                      </button>
                    </div>
                  </td>
                </tr>
              );
            })}
          </tbody>
        </table>
      </div>
    </section>
  );
}
