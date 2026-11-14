import { Receipt, Inbox } from "lucide-react";
import { formatUsd, shortAddr } from "../lib/format";
import type { RecentLiquidation } from "../lib/api";

export default function RecentLiquidations({
  items,
}: {
  items: RecentLiquidation[];
}) {
  return (
    <section className="rounded-lg border border-edge bg-card">
      <div className="flex items-center gap-2 border-b border-edge px-4 py-3">
        <Receipt size={13} className="text-muted" />
        <h2 className="text-[11px] uppercase tracking-widest text-muted">
          Recent on-chain LiquidationCall events
        </h2>
      </div>

      {items.length === 0 ? (
        // The backend currently hardcodes recent_liquidations: [] (src/api.rs).
        // Say so plainly instead of rendering placeholder rows that look real.
        <div className="flex items-start gap-2 px-4 py-6 text-[11px] text-muted">
          <Inbox size={14} className="mt-px shrink-0" />
          <span>
            No events. The backend returns an empty{" "}
            <code className="text-ink">recent_liquidations</code> array until the
            on-chain LiquidationCall watcher is wired up.
          </span>
        </div>
      ) : (
        <ul className="max-h-[260px] divide-y divide-edge/60 overflow-auto">
          {items.map((e, i) => {
            const borrower = e.borrower ?? e.user;
            return (
              <li key={e.tx_hash ?? i} className="px-4 py-2 text-[11px]">
                <div className="flex items-baseline justify-between gap-2">
                  <span className="text-muted">
                    block{" "}
                    <span className="tabular-nums text-ink">
                      {e.block?.toLocaleString("en-US") ?? "—"}
                    </span>
                  </span>
                  <span className="text-[10px] text-muted">
                    {shortAddr(e.tx_hash, 10, 6)}
                  </span>
                </div>
                <div className="mt-1 grid grid-cols-2 gap-x-3 gap-y-0.5">
                  <Pair k="borrower" v={shortAddr(borrower)} />
                  <Pair k="liquidator" v={shortAddr(e.liquidator)} />
                  <Pair
                    k="debt repaid"
                    v={
                      e.debt_repaid_usd === undefined
                        ? "—"
                        : `$${formatUsd(e.debt_repaid_usd)}`
                    }
                  />
                  <Pair
                    k="coll. seized"
                    v={
                      e.collateral_seized_usd === undefined
                        ? "—"
                        : `$${formatUsd(e.collateral_seized_usd)}`
                    }
                  />
                </div>
              </li>
            );
          })}
        </ul>
      )}
    </section>
  );
}

function Pair({ k, v }: { k: string; v: string }) {
  return (
    <div className="flex justify-between gap-2">
      <span className="text-muted">{k}</span>
      <span className="tabular-nums text-ink">{v}</span>
    </div>
  );
}
