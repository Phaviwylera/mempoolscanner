import { useEffect, useState } from "react";
import {
  X,
  Copy,
  Check,
  ShieldAlert,
  LoaderCircle,
  TriangleAlert,
  CircleCheck,
} from "lucide-react";
import type { BuildTxResult } from "../lib/api";
import { shortAddr } from "../lib/format";

function Field({ label, value, mono = true }: { label: string; value?: string | number; mono?: boolean }) {
  if (value === undefined || value === null || value === "") return null;
  return (
    <div className="flex items-baseline justify-between gap-3 py-0.5">
      <span className="text-[10px] uppercase tracking-wider text-muted">{label}</span>
      <span className={`text-[11px] text-ink ${mono ? "tabular-nums break-all" : ""}`}>
        {value}
      </span>
    </div>
  );
}

/**
 * Modal showing the manually-built, signed liquidation payload.
 *
 * The transaction is built and signed OFFLINE by the backend; it is NOT
 * broadcast. The operator copies the raw hex and broadcasts it themselves
 * (e.g. via Flashbots).
 */
export default function PayloadModal({
  user,
  result,
  loading,
  onClose,
}: {
  user: string;
  result: BuildTxResult | null;
  loading: boolean;
  onClose: () => void;
}) {
  const [copied, setCopied] = useState(false);

  // Close on Escape.
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => e.key === "Escape" && onClose();
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [onClose]);

  const copy = async () => {
    if (!result?.raw_tx_hex) return;
    try {
      await navigator.clipboard.writeText(result.raw_tx_hex);
      setCopied(true);
      setTimeout(() => setCopied(false), 1500);
    } catch {
      // Clipboard can be blocked; the hex stays selectable in the <pre>.
    }
  };

  return (
    <div
      className="fixed inset-0 z-50 flex items-center justify-center bg-black/60 p-4"
      onClick={onClose}
    >
      <div
        className="max-h-[85vh] w-full max-w-2xl overflow-auto rounded-lg border border-edge bg-card shadow-xl"
        onClick={(e) => e.stopPropagation()}
      >
        {/* Header */}
        <div className="flex items-center justify-between border-b border-edge px-4 py-3">
          <h2 className="text-[11px] uppercase tracking-widest text-ink">
            Signed Payload · {shortAddr(user)}
          </h2>
          <button
            onClick={onClose}
            aria-label="Close"
            className="text-muted transition-colors hover:text-ink"
          >
            <X size={15} />
          </button>
        </div>

        <div className="space-y-3 p-4">
          {/* Safety banner */}
          <div className="flex items-start gap-2 rounded border border-warn/40 bg-warn/10 px-3 py-2 text-[11px] text-warn">
            <ShieldAlert size={14} className="mt-px shrink-0" />
            <span>
              Built and signed <strong>offline</strong>. This transaction is{" "}
              <strong>not broadcast</strong>. Inspect it, then broadcast it
              yourself (e.g. via Flashbots).
            </span>
          </div>

          {loading ? (
            <div className="flex items-center gap-2 py-8 text-[12px] text-muted">
              <LoaderCircle size={14} className="animate-spin" />
              Resolving opportunity, running pre-flight, and signing…
            </div>
          ) : !result ? null : !result.success ? (
            <div className="flex items-start gap-2 rounded border border-risk/40 bg-risk/10 px-3 py-3 text-[11px] text-risk">
              <TriangleAlert size={14} className="mt-px shrink-0" />
              <span className="break-words">{result.error ?? "Failed to build payload."}</span>
            </div>
          ) : (
            <>
              {/* Simulation verdict */}
              <div
                className={`flex items-start gap-2 rounded border px-3 py-2 text-[11px] ${
                  result.simulated_ok
                    ? "border-profit/40 bg-profit/10 text-profit"
                    : "border-risk/40 bg-risk/10 text-risk"
                }`}
              >
                {result.simulated_ok ? (
                  <CircleCheck size={14} className="mt-px shrink-0" />
                ) : (
                  <TriangleAlert size={14} className="mt-px shrink-0" />
                )}
                <span className="break-words">
                  {result.simulated_ok
                    ? `revm pre-flight passed · gas ${result.sim_gas_used}`
                    : `revm pre-flight would REVERT: ${result.sim_revert_reason ?? "unknown"} — do not broadcast as built.`}
                </span>
              </div>

              {/* Decoded fields */}
              <div className="rounded border border-edge bg-canvas px-3 py-2">
                <Field label="from" value={result.from} />
                <Field label="to (liquidator)" value={result.to} />
                <Field label="user" value={result.user} />
                <Field label="collateral" value={result.collateral_asset} />
                <Field label="debt asset" value={result.debt_asset} />
                <Field label="debtToCover" value={result.debt_to_cover} />
                <Field label="swapFee" value={result.swap_fee} />
                <Field label="minProfit" value={result.min_profit} />
                <Field label="nonce" value={result.nonce} />
                <Field label="chainId" value={result.chain_id} />
                <Field label="gasLimit" value={result.gas_limit} />
                <Field label="maxFeePerGas" value={result.max_fee_per_gas} />
                <Field label="maxPriorityFee" value={result.max_priority_fee_per_gas} />
                <Field label="txHash" value={result.tx_hash} />
              </div>

              {/* Raw hex + copy */}
              <div>
                <div className="mb-1 flex items-center justify-between">
                  <span className="text-[10px] uppercase tracking-wider text-muted">
                    raw signed transaction
                  </span>
                  <button
                    onClick={copy}
                    className="inline-flex items-center gap-1 rounded border border-edge bg-edge/40 px-2 py-1 text-[10px] text-ink transition-colors hover:bg-edge"
                  >
                    {copied ? <Check size={11} /> : <Copy size={11} />}
                    {copied ? "Copied" : "Copy"}
                  </button>
                </div>
                <pre className="max-h-40 overflow-auto rounded border border-edge bg-canvas p-2 text-[10px] leading-relaxed text-ink">
                  <code className="break-all">{result.raw_tx_hex}</code>
                </pre>
              </div>
            </>
          )}
        </div>
      </div>
    </div>
  );
}
