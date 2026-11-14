import { useMemo } from "react";
import { ArrowDownWideNarrow } from "lucide-react";
import {
  BAND_BAR,
  BAND_TEXT,
  formatHf,
  formatUsd,
  hfBand,
  hfPercent,
  shortAddr,
  toBigInt,
  weiToEth,
} from "../lib/format";
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
}: {
  rows: Row[];
  selected: string | null;
  onSelect: (user: string) => void;
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
            </tr>
          </thead>
          <tbody>
            {sorted.length === 0 ? (
              <tr>
                <td colSpan={7} className="px-4 py-8 text-center text-muted">
                  No positions in the snapshot yet.
                </td>
              </tr>
            ) : null}

            {sorted.map((r) => {
              const isSel = selected !== null && selected === r.user;
              return (
                <tr
                  key={r.user}
                  onClick={() => onSelect(r.user)}
                  className={`cursor-pointer border-t border-edge/60 transition-colors ${
                    isSel ? "bg-edge/50" : "hover:bg-edge/25"
                  }`}
                >
                  <td className="px-4 py-2">
                    <span className="text-ink">{shortAddr(r.user)}</span>
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
                    {r.bountyWeiHex ? (
                      <span className="text-profit">
                        {weiToEth(r.bountyWeiHex, 6)} ETH
                      </span>
                    ) : (
                      <span
                        className="text-muted"
                        title="Keeper has no live wei bounty for this borrower; showing the snapshot USD estimate"
                      >
                        ${formatUsd(r.plan?.gross_bounty_usd)}
                        <span className="ml-1 text-[9px] uppercase">usd</span>
                      </span>
                    )}
                  </td>
                  <td className="px-3 py-2">
                    <StatusPill row={r} />
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
