import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { Activity, CircleAlert } from "lucide-react";
import Header from "./components/Header";
import AtomicSequence from "./components/AtomicSequence";
import PositionsTable from "./components/PositionsTable";
import CheckWallet from "./components/CheckWallet";
import EnginePanel from "./components/EnginePanel";
import RecentLiquidations from "./components/RecentLiquidations";
import {
  fetchLiquidations,
  fetchOpportunities,
  joinRows,
  type LiquidationsResponse,
  type OpportunitiesResponse,
  type Row,
} from "./lib/api";

const POLL_MS = 5000;

export default function App() {
  const [liq, setLiq] = useState<LiquidationsResponse | null>(null);
  const [opps, setOpps] = useState<OpportunitiesResponse | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [lastOk, setLastOk] = useState<Date | null>(null);
  const [selected, setSelected] = useState<string | null>(null);

  // Keeps the poll from stacking requests if the backend is slow.
  const inFlight = useRef(false);

  const poll = useCallback(async (signal?: AbortSignal) => {
    if (inFlight.current) return;
    inFlight.current = true;
    try {
      // Both endpoints are independent; fetch concurrently.
      const [l, o] = await Promise.all([
        fetchLiquidations(signal),
        fetchOpportunities(signal),
      ]);
      setLiq(l);
      setOpps(o);
      setError(null);
      setLastOk(new Date());
    } catch (e) {
      if ((e as Error)?.name === "AbortError") return;
      setError(e instanceof Error ? e.message : String(e));
    } finally {
      inFlight.current = false;
    }
  }, []);

  useEffect(() => {
    const ac = new AbortController();
    void poll(ac.signal);
    const id = setInterval(() => void poll(ac.signal), POLL_MS);
    return () => {
      clearInterval(id);
      ac.abort();
    };
  }, [poll]);

  const rows: Row[] = useMemo(
    () => joinRows(liq?.positions ?? [], opps?.opportunities ?? []),
    [liq, opps],
  );

  const selectedRow = useMemo(
    () => rows.find((r) => r.user === selected) ?? null,
    [rows, selected],
  );

  const live = error === null && liq !== null;

  return (
    <div className="min-h-full bg-canvas px-4 py-4 text-ink lg:px-6">
      {/* Title bar */}
      <div className="mb-4 flex flex-wrap items-center justify-between gap-2">
        <div className="flex items-baseline gap-3">
          <h1 className="text-sm tracking-wide text-ink">
            AAVE V3 KEEPER
            <span className="ml-2 text-muted">liquidation terminal</span>
          </h1>
        </div>
        <div className="flex items-center gap-2 text-[11px]">
          <Activity
            size={12}
            className={live ? "text-profit" : "text-risk"}
          />
          <span className={live ? "text-profit" : "text-risk"}>
            {live ? "live" : "disconnected"}
          </span>
          {lastOk ? (
            <span className="text-muted">
              · updated {lastOk.toLocaleTimeString("en-US", { hour12: false })}
            </span>
          ) : null}
          <span className="text-muted">· poll {POLL_MS / 1000}s</span>
        </div>
      </div>

      {error ? (
        <div className="mb-4 flex items-start gap-2 rounded-lg border border-risk/40 bg-risk/10 px-4 py-3 text-[11px] text-risk">
          <CircleAlert size={14} className="mt-px shrink-0" />
          <span>
            {error}
            <br />
            <span className="text-muted">
              Is the Rust backend running on http://localhost:3000 ?
            </span>
          </span>
        </div>
      ) : null}

      <Header
        rows={rows}
        ethUsd={liq?.fx?.eth_usd}
        fxSource={liq?.fx?.source}
        updatedTs={liq?.updated_ts}
      />

      {/* 60 / 40 split */}
      <div className="mt-4 grid grid-cols-1 gap-4 lg:grid-cols-5">
        <div className="flex flex-col gap-4 lg:col-span-3">
          <AtomicSequence row={selectedRow} />
          <PositionsTable
            rows={rows}
            selected={selected}
            onSelect={setSelected}
          />
        </div>

        <div className="flex flex-col gap-4 lg:col-span-2">
          <CheckWallet rows={rows} onFound={setSelected} />
          <EnginePanel row={selectedRow} baseFeeGwei={liq?.base_fee_gwei} />
          <RecentLiquidations items={liq?.recent_liquidations ?? []} />
        </div>
      </div>

      <p className="mt-4 text-[10px] leading-relaxed text-muted">
        Positions, health factors and plan math come from{" "}
        <code className="text-ink">/api/liquidations</code> (on-disk snapshot).
        Live wei bounties come from{" "}
        <code className="text-ink">/api/opportunities</code> (keeper memory) and
        are parsed as BigInt — U256 arrives as a hex string.
      </p>
    </div>
  );
}
