import { Cpu } from "lucide-react";
import { formatHf, formatUsd, shortAddr } from "../lib/format";
import type { Row } from "../lib/api";

/** AAVE_V3_POOL, mirrored from src/keeper.rs. */
const POOL = "0x87870bca3f3fd6335c3f4ce8392d69350b4fa4e2";

export default function EnginePanel({
  row,
  baseFeeGwei,
}: {
  row: Row | null;
  baseFeeGwei: number | undefined;
}) {
  // HF = (collateral * liquidationThreshold) / debt.
  // Recomputed here only to SHOW the identity; the authoritative number is
  // row.health_factor from the backend. If the two ever disagree, the
  // backend wins and the delta is displayed rather than hidden.
  const derived =
    row && row.debt_usd > 0
      ? (row.collateral_usd * (row.liq_threshold_pct / 100)) / row.debt_usd
      : null;

  const delta =
    derived !== null && Number.isFinite(row?.health_factor ?? NaN)
      ? Math.abs(derived - (row as Row).health_factor)
      : null;

  return (
    <section className="rounded-lg border border-edge bg-card">
      <div className="flex items-center gap-2 border-b border-edge px-4 py-3">
        <Cpu size={13} className="text-muted" />
        <h2 className="text-[11px] uppercase tracking-widest text-muted">
          Health factor &amp; engine
        </h2>
      </div>

      <div className="px-4 py-3">
        <div className="rounded border border-edge bg-canvas px-3 py-2 text-[11px] leading-relaxed text-muted">
          <span className="text-ink">HF</span> = (collateral &times; liquidation
          threshold) &divide; debt
          <br />
          liquidatable when <span className="text-risk">HF &lt; 1.0</span>
        </div>

        <dl className="mt-3 space-y-1 text-[11px]">
          <Fact k="Pool" v={shortAddr(POOL, 10, 6)} mono />
          <Fact k="Chain" v="Ethereum mainnet (1)" />
          <Fact
            k="Base fee"
            v={baseFeeGwei === undefined ? "—" : `${baseFeeGwei} gwei`}
          />

          {row ? (
            <>
              <div className="my-2 border-t border-edge" />
              <Fact k="Selected" v={shortAddr(row.user)} mono />
              <Fact k="Collateral" v={`$${formatUsd(row.collateral_usd)}`} />
              <Fact k="Debt" v={`$${formatUsd(row.debt_usd)}`} />
              <Fact k="Liq. threshold" v={`${row.liq_threshold_pct?.toFixed(2)}%`} />
              <Fact k="LTV" v={`${row.ltv_pct?.toFixed(2)}%`} />
              <Fact k="HF (backend)" v={formatHf(row.health_factor)} />
              <Fact
                k="HF (recomputed)"
                v={derived === null ? "—" : derived.toFixed(4)}
              />
              {delta !== null && delta > 0.01 ? (
                <p className="pt-1 text-warn">
                  Recomputed HF differs by {delta.toFixed(4)} — backend value is
                  authoritative.
                </p>
              ) : null}
              <Fact
                k="Price drop to liq."
                v={`${row.price_drop_pct_to_liq?.toFixed(2)}%`}
              />
              <Fact
                k="ETH liq. price"
                v={`$${formatUsd(row.eth_liq_price_usd)}`}
              />
              <Fact k="Last seen block" v={row.last_seen_block?.toLocaleString("en-US")} />
            </>
          ) : (
            <p className="pt-2 text-muted">Select a position to see its inputs.</p>
          )}
        </dl>
      </div>
    </section>
  );
}

function Fact({ k, v, mono }: { k: string; v: string; mono?: boolean }) {
  return (
    <div className="flex items-baseline justify-between gap-3">
      <dt className="text-muted">{k}</dt>
      <dd className={`tabular-nums text-ink ${mono ? "text-[10px]" : ""}`}>{v}</dd>
    </div>
  );
}
