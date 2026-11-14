import { Banknote, Gavel, Repeat, Undo2, TrendingUp, CircleAlert } from "lucide-react";
import type { ReactNode } from "react";
import { formatUsd, shortAddr, weiToEth } from "../lib/format";
import type { Row } from "../lib/api";

interface Step {
  icon: ReactNode;
  title: string;
  detail: string;
  amount: string;
  tone: string;
}

function buildSteps(row: Row | null): Step[] {
  const p = row?.plan ?? null;
  const dash = "—";

  // Every figure below comes from the backend's `plan` object. Nothing is
  // recomputed in the browser, so the UI can never disagree with the engine.
  const debtToCover = p ? `$${formatUsd(p.debt_to_cover_usd)}` : dash;
  const seized = p ? `$${formatUsd(p.seized_collateral_usd)}` : dash;
  const flashFee = p ? `$${formatUsd(p.flash_loan_fee_usd)}` : dash;
  const swapFee = p ? `$${formatUsd(p.dex_swap_fee_usd)}` : dash;
  const gas = p ? `$${formatUsd(p.est_gas_usd)}` : dash;
  const net = p ? `$${formatUsd(p.net_profit_usd)}` : dash;

  const symbol = row?.last_reserve_symbol ?? "asset";

  return [
    {
      icon: <Banknote size={14} />,
      title: "1 · Flash Loan",
      detail: p
        ? `Borrow ${debtToCover} of ${symbol} debt · fee ${p.flash_fee_pct}%`
        : "Borrow the debt asset from the Aave pool",
      amount: debtToCover,
      tone: "text-ink",
    },
    {
      icon: <Gavel size={14} />,
      title: "2 · Liquidate",
      detail: p
        ? `Repay at close factor ${p.close_factor_pct}% · bonus ${p.liquidation_bonus_pct}%`
        : "Call liquidationCall on the unhealthy position",
      amount: seized,
      tone: "text-ink",
    },
    {
      icon: <Repeat size={14} />,
      title: "3 · Swap",
      detail: p
        ? `Seized collateral → debt asset · DEX fee ${swapFee}`
        : "Swap seized collateral back to the debt asset",
      amount: swapFee,
      tone: "text-muted",
    },
    {
      icon: <Undo2 size={14} />,
      title: "4 · Repay",
      detail: p
        ? `Return principal + ${flashFee} premium · gas ${gas}`
        : "Repay the flash loan plus its premium",
      amount: flashFee,
      tone: "text-muted",
    },
    {
      icon: <TrendingUp size={14} />,
      title: "5 · Profit",
      detail: p
        ? p.profitable_now
          ? "Atomic net profit, after all fees and gas"
          : `Below trigger of $${formatUsd(p.trigger_net_profit_usd)} — engine would not fire`
        : "Net profit after fees and gas",
      amount: net,
      tone: p?.profitable_now ? "text-profit" : "text-risk",
    },
  ];
}

export default function AtomicSequence({ row }: { row: Row | null }) {
  const steps = buildSteps(row);

  return (
    <section className="rounded-lg border border-edge bg-card">
      <div className="flex items-baseline justify-between border-b border-edge px-4 py-3">
        <h2 className="text-[11px] uppercase tracking-widest text-muted">
          Atomic Sequence
        </h2>
        <span className="text-[11px] text-muted">
          {row ? (
            <>
              <span className="text-ink">{shortAddr(row.user)}</span>
              {row.bountyWeiHex ? (
                <span className="text-profit">
                  {" · keeper bounty "}
                  {weiToEth(row.bountyWeiHex, 6)} ETH
                </span>
              ) : null}
            </>
          ) : (
            "select a position below"
          )}
        </span>
      </div>

      {row === null ? (
        <div className="flex items-center gap-2 px-4 py-8 text-xs text-muted">
          <CircleAlert size={14} />
          No position selected. Click a row in the table to load its exact math.
        </div>
      ) : null}

      <ol className="px-4 py-3">
        {steps.map((s, i) => (
          <li key={s.title} className="relative flex gap-3 pb-4 last:pb-0">
            {/* vertical rail */}
            {i < steps.length - 1 ? (
              <span className="absolute left-[11px] top-6 h-full w-px bg-edge" />
            ) : null}
            <span className="relative z-10 mt-0.5 flex h-6 w-6 shrink-0 items-center justify-center rounded-full border border-edge bg-canvas text-muted">
              {s.icon}
            </span>
            <div className="flex-1">
              <div className="flex items-baseline justify-between gap-3">
                <span className="text-xs text-ink">{s.title}</span>
                <span className={`text-sm tabular-nums ${s.tone}`}>{s.amount}</span>
              </div>
              <div className="mt-0.5 text-[11px] leading-relaxed text-muted">
                {s.detail}
              </div>
            </div>
          </li>
        ))}
      </ol>

      {row?.plan ? (
        <div className="grid grid-cols-2 gap-x-6 gap-y-1 border-t border-edge px-4 py-3 text-[11px] sm:grid-cols-4">
          <Fact k="Close factor" v={`${row.plan.close_factor_pct}%`} />
          <Fact k="Liq. bonus" v={`${row.plan.liquidation_bonus_pct}%`} />
          <Fact k="Flash fee" v={`${row.plan.flash_fee_pct}%`} />
          <Fact k="Est. gas" v={`$${formatUsd(row.plan.est_gas_usd)}`} />
          <Fact k="Gross bounty" v={`$${formatUsd(row.plan.gross_bounty_usd)}`} />
          <Fact k="Trigger floor" v={`$${formatUsd(row.plan.trigger_net_profit_usd)}`} />
          <Fact k="revm verified" v={row.revm_verified ? "yes" : "no"} />
          <Fact
            k="revm gas"
            v={row.revm_gas_used ? row.revm_gas_used.toLocaleString("en-US") : "—"}
          />
        </div>
      ) : null}
    </section>
  );
}

function Fact({ k, v }: { k: string; v: string }) {
  return (
    <div className="flex justify-between gap-2">
      <span className="text-muted">{k}</span>
      <span className="tabular-nums text-ink">{v}</span>
    </div>
  );
}
