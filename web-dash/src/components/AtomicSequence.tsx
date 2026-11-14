import {
  Banknote,
  Gavel,
  Repeat,
  Undo2,
  TrendingUp,
  CircleAlert,
  FlaskConical,
} from "lucide-react";
import type { ReactNode } from "react";
import { formatGwei, formatUsd, shortAddr, toBigInt, weiToEth } from "../lib/format";
import type { Row, SimulationResult } from "../lib/api";

const DASH = "—";

interface Step {
  icon: ReactNode;
  title: string;
  detail: string;
  amount: string;
  tone: string;
}

/** Raw token units are NOT wei of ETH, so they are never shown as "ETH". */
function rawUnits(hex: string | null | undefined): string {
  if (!hex) return DASH;
  return toBigInt(hex).toString();
}

function buildSteps(row: Row | null, sim: SimulationResult | null): Step[] {
  const p = row?.plan ?? null;
  const simulated = sim?.success === true;

  const debtToCover = p ? `$${formatUsd(p.debt_to_cover_usd)}` : DASH;
  const seized = p ? `$${formatUsd(p.seized_collateral_usd)}` : DASH;
  const flashFee = p ? `$${formatUsd(p.flash_loan_fee_usd)}` : DASH;
  const swapFee = p ? `$${formatUsd(p.dex_swap_fee_usd)}` : DASH;
  const gasUsd = p ? `$${formatUsd(p.est_gas_usd)}` : DASH;
  const netUsd = p ? `$${formatUsd(p.net_profit_usd)}` : DASH;

  const symbol = row?.last_reserve_symbol ?? "asset";
  const haveEvent = simulated && sim?.liquidation_call_event_found === true;

  return [
    {
      icon: <Banknote size={14} />,
      title: "1 · Flash Loan",
      detail: haveEvent
        ? `Simulated debt repaid, raw ${symbol} units from the LiquidationCall event`
        : p
          ? `Borrow ${debtToCover} of ${symbol} debt · fee ${p.flash_fee_pct}%`
          : "Borrow the debt asset from the Aave pool",
      amount: haveEvent ? rawUnits(sim?.debt_repaid) : debtToCover,
      tone: "text-ink",
    },
    {
      icon: <Gavel size={14} />,
      title: "2 · Liquidate",
      detail: haveEvent
        ? "Simulated collateral seized, raw token units from the event"
        : p
          ? `Repay at close factor ${p.close_factor_pct}% · bonus ${p.liquidation_bonus_pct}%`
          : "Call liquidationCall on the unhealthy position",
      amount: haveEvent ? rawUnits(sim?.collateral_seized) : seized,
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
      detail: simulated
        ? `Measured gas ${sim?.gas_used?.toLocaleString("en-US")} units · flash premium ${flashFee}`
        : p
          ? `Return principal + ${flashFee} premium · gas ${gasUsd}`
          : "Repay the flash loan plus its premium",
      amount: simulated ? `${weiToEth(sim?.gas_cost_wei, 6)} ETH` : flashFee,
      tone: "text-muted",
    },
    {
      icon: <TrendingUp size={14} />,
      title: "5 · Profit",
      detail: simulated
        ? (sim?.profit_basis ?? "Gross payout minus simulated gas cost")
        : p
          ? p.profitable_now
            ? "Atomic net profit, after all fees and gas"
            : `Below trigger of $${formatUsd(p.trigger_net_profit_usd)} — engine would not fire`
          : "Net profit after fees and gas",
      amount: simulated
        ? `${weiToEth(sim?.estimated_net_profit_wei, 6)} ETH`
        : netUsd,
      tone: simulated
        ? toBigInt(sim?.estimated_net_profit_wei) > 0n
          ? "text-profit"
          : "text-risk"
        : p?.profitable_now
          ? "text-profit"
          : "text-risk",
    },
  ];
}

export default function AtomicSequence({
  row,
  sim,
}: {
  row: Row | null;
  sim: SimulationResult | null;
}) {
  const steps = buildSteps(row, sim);
  const simulated = sim?.success === true;

  return (
    <section className="rounded-lg border border-edge bg-card">
      <div className="flex flex-wrap items-baseline justify-between gap-2 border-b border-edge px-4 py-3">
        <h2 className="flex items-center gap-2 text-[11px] uppercase tracking-widest text-muted">
          Atomic Sequence
          {simulated ? (
            <span className="flex items-center gap-1 rounded border border-profit/40 bg-profit/10 px-1.5 py-0.5 text-[9px] normal-case tracking-normal text-profit">
              <FlaskConical size={9} />
              Simulated
            </span>
          ) : null}
        </h2>
        <span className="text-[11px] text-muted">
          {row ? (
            <>
              <span className="text-ink">{shortAddr(row.user)}</span>
              {simulated && sim?.block_simulated ? (
                <span>
                  {" · block "}
                  {sim.block_simulated.toLocaleString("en-US")}
                </span>
              ) : null}
              {!simulated && row.bountyWeiHex ? (
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

      {/* A failed simulation is reported here as well as in the toast, so the
          reason stays on screen after the toast is dismissed. */}
      {sim && sim.success === false ? (
        <div className="mx-4 mt-3 flex items-start gap-2 rounded border border-risk/40 bg-risk/10 px-3 py-2 text-[11px] text-risk">
          <CircleAlert size={13} className="mt-px shrink-0" />
          <span>
            Simulation failed: {sim.error}
            {sim.gas_used ? (
              <span className="text-muted">
                {" "}
                (gas burned {sim.gas_used.toLocaleString("en-US")})
              </span>
            ) : null}
          </span>
        </div>
      ) : null}

      <ol className="px-4 py-3">
        {steps.map((s, i) => (
          <li key={s.title} className="relative flex gap-3 pb-4 last:pb-0">
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

      {simulated ? (
        <div className="grid grid-cols-2 gap-x-6 gap-y-1 border-t border-edge px-4 py-3 text-[11px] sm:grid-cols-4">
          <Fact k="Gas used" v={sim?.gas_used?.toLocaleString("en-US") ?? DASH} />
          <Fact k="Gas price" v={`${formatGwei(sim?.gas_price_wei)} gwei`} />
          <Fact k="Gas cost" v={`${weiToEth(sim?.gas_cost_wei, 6)} ETH`} />
          <Fact k="Broadcast" v={sim?.broadcast ? "yes" : "no"} />
          <Fact k="Event found" v={sim?.liquidation_call_event_found ? "yes" : "no"} />
          <Fact k="Collateral" v={shortAddr(sim?.collateral_asset)} />
          <Fact k="Debt asset" v={shortAddr(sim?.debt_asset)} />
          <Fact k="From" v={shortAddr(sim?.simulated_from)} />
        </div>
      ) : row?.plan ? (
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
            v={row.revm_gas_used ? row.revm_gas_used.toLocaleString("en-US") : DASH}
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
