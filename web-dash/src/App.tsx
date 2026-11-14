import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { Activity, CircleAlert, X } from "lucide-react";
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
  simulate,
  type LiquidationsResponse,
  type OpportunitiesResponse,
  type Row,
  type SimulationResult,
} from "./lib/api";

const POLL_MS = 5000;

export default function App() {
  const [liq, setLiq] = useState<LiquidationsResponse | null>(null);
  const [opps, setOpps] = useState<OpportunitiesResponse | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [lastOk, setLastOk] = useState<Date | null>(null);
  const [selected, setSelected] = useState<string | null>(null);

  // Simulation results keyed by borrower, so switching rows keeps each result.
  const [sims, setSims] = useState<Record<string, SimulationResult>>({});
  const [simulatingUser, setSimulatingUser] = useState<string | null>(null);
  const [toast, setToast] = useState<string | null>(null);

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

  const onSimulate = useCallback(async (user: string) => {
    setSelected(user);
    setSimulatingUser(user);
    setToast(null);
    try {
      const result = await simulate(user);
      setSims((prev) => ({ ...prev, [user]: result }));
      if (!result.success) {
        setToast(result.error ?? "Simulation failed for an unknown reason.");
      }
    } catch (e) {
      const msg = e instanceof Error ? e.message : String(e);
      setSims((prev) => ({ ...prev, [user]: { success: false, error: msg } }));
      setToast(msg);
    } finally {
      setSimulatingUser(null);
    }
  }, []);

  const rows: Row[] = useMemo(
    () => joinRows(liq?.positions ?? [], opps?.opportunities ?? []),
    [liq, opps],
  );

  const selectedRow = useMemo(
    () => rows.find((r) => r.user === selected) ?? null,
    [rows, selected],
  );

  const selectedSim = selected ? (sims[selected] ?? null) : null;
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
          <AtomicSequence row={selectedRow} sim={selectedSim} />
          <PositionsTable
            rows={rows}
            selected={selected}
            onSelect={setSelected}
            onSimulate={(u) => void onSimulate(u)}
            simulatingUser={simulatingUser}
          />
        </div>

        <div className="flex flex-col gap-4 lg:col-span-2">
          <CheckWallet rows={rows} onFound={setSelected} />
          <EnginePanel row={selectedRow} baseFeeGwei={liq?.base_fee_gwei} />
          <RecentLiquidations items={liq?.recent_liquidations ?? []} />
        </div>
      </div>

      {toast ? (
        <div className="fixed bottom-4 right-4 z-50 flex max-w-md items-start gap-2 rounded-lg border border-risk/50 bg-card px-4 py-3 text-[11px] text-risk shadow-lg">
          <CircleAlert size={14} className="mt-px shrink-0" />
          <span className="flex-1 break-words">{toast}</span>
          <button
            onClick={() => setToast(null)}
            aria-label="Dismiss"
            className="shrink-0 text-muted transition-colors hover:text-ink"
          >
            <X size={13} />
          </button>
        </div>
      ) : null}

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
