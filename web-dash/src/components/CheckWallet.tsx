import { useState } from "react";
import { Search, LoaderCircle } from "lucide-react";
import { inspectAddress, type Inspected, type Row } from "../lib/api";
import { formatHf, formatUsd, isAddress, shortAddr } from "../lib/format";

export default function CheckWallet({
  rows,
  onFound,
}: {
  rows: Row[];
  onFound: (user: string) => void;
}) {
  const [value, setValue] = useState("");
  const [busy, setBusy] = useState(false);
  const [local, setLocal] = useState<Row | null>(null);
  const [remote, setRemote] = useState<Inspected | null>(null);
  const [error, setError] = useState<string | null>(null);

  async function run() {
    const addr = value.trim();
    setError(null);
    setLocal(null);
    setRemote(null);

    if (!isAddress(addr)) {
      setError("Not a valid 20-byte hex address.");
      return;
    }

    // First: is this borrower already in the tracked snapshot? That answer is
    // free and complete, so prefer it over the backend round-trip.
    const hit = rows.find((r) => r.user.toLowerCase() === addr.toLowerCase());
    if (hit) {
      setLocal(hit);
      onFound(hit.user);
      return;
    }

    setBusy(true);
    try {
      setRemote(await inspectAddress(addr));
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    } finally {
      setBusy(false);
    }
  }

  return (
    <section className="rounded-lg border border-edge bg-card">
      <div className="border-b border-edge px-4 py-3">
        <h2 className="text-[11px] uppercase tracking-widest text-muted">
          Check any borrower wallet
        </h2>
      </div>

      <div className="px-4 py-3">
        <div className="flex gap-2">
          <input
            value={value}
            onChange={(e) => setValue(e.target.value)}
            onKeyDown={(e) => {
              if (e.key === "Enter") void run();
            }}
            spellCheck={false}
            placeholder="0x…"
            className="min-w-0 flex-1 rounded border border-edge bg-canvas px-2 py-1.5 text-xs text-ink placeholder:text-muted/60 focus:border-muted focus:outline-none"
          />
          <button
            onClick={() => void run()}
            disabled={busy}
            className="flex items-center gap-1 rounded border border-edge bg-edge/40 px-3 py-1.5 text-xs text-ink transition-colors hover:bg-edge disabled:opacity-50"
          >
            {busy ? (
              <LoaderCircle size={12} className="animate-spin" />
            ) : (
              <Search size={12} />
            )}
            Inspect
          </button>
        </div>

        {error ? (
          <p className="mt-2 text-[11px] text-risk">{error}</p>
        ) : null}

        {local ? (
          <div className="mt-3 space-y-1 rounded border border-edge bg-canvas px-3 py-2 text-[11px]">
            <div className="flex justify-between">
              <span className="text-muted">tracked</span>
              <span className="text-ink">{shortAddr(local.user)}</span>
            </div>
            <div className="flex justify-between">
              <span className="text-muted">health factor</span>
              <span className="tabular-nums text-ink">
                {formatHf(local.health_factor)}
              </span>
            </div>
            <div className="flex justify-between">
              <span className="text-muted">collateral / debt</span>
              <span className="tabular-nums text-ink">
                ${formatUsd(local.collateral_usd)} / ${formatUsd(local.debt_usd)}
              </span>
            </div>
            <p className="pt-1 text-muted">
              Loaded into the Atomic Sequence panel.
            </p>
          </div>
        ) : null}

        {remote ? (
          <div className="mt-3 rounded border border-edge bg-canvas px-3 py-2 text-[11px]">
            <div className="flex justify-between">
              <span className="text-muted">queried</span>
              <span className="text-ink">{shortAddr(remote.user)}</span>
            </div>
            {/* The backend currently answers ok:false for any address outside
                the snapshot. Surface its own message rather than inventing a
                health factor the server never sent. */}
            <p className={`mt-1 ${remote.ok ? "text-profit" : "text-warn"}`}>
              {remote.ok
                ? "Backend returned a live result."
                : (remote.error ?? "Not tracked and no live watcher available.")}
            </p>
          </div>
        ) : null}
      </div>
    </section>
  );
}
