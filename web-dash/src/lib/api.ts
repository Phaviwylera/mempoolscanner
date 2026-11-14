/**
 * Typed bindings for the Rust backend.
 *
 * Shapes mirror src/api.rs exactly:
 *  - /api/opportunities  -> live in-memory keeper map (U256 as HEX STRINGS)
 *  - /api/liquidations   -> on-disk logs/liquidations.json snapshot (f64 USD)
 */

export const API_BASE = import.meta.env.DEV ? "" : "";

/** One entry of the keeper's recent_opportunities map (src/exec/mod.rs). */
export interface Opportunity {
  id: string;
  chain_id: number;
  category: string;
  to: string;
  from: string;
  value: string; // U256 hex
  gas_limit: number;
  nonce: number;
  calldata: string;
  gross_profit_wei: string; // U256 hex
  borrower: string | null;
  collateral: string | null;
  debt_asset: string | null;
  repay_amount: string | null; // U256 hex
  selector: string;
  summary: string;
}

export interface OpportunitiesResponse {
  opportunities: Opportunity[];
  count: number;
  total: number;
  liquidatable_count: number;
  status: string;
}

/** The atomic-sequence math, as written into logs/liquidations.json. */
export interface Plan {
  close_factor_pct: number;
  liquidation_bonus_pct: number;
  flash_fee_pct: number;
  debt_to_cover_usd: number;
  seized_collateral_usd: number;
  gross_bounty_usd: number;
  flash_loan_fee_usd: number;
  dex_swap_fee_usd: number;
  est_gas_usd: number;
  net_profit_usd: number;
  trigger_net_profit_usd: number;
  profitable_now: boolean;
}

export interface Position {
  user: string;
  collateral_usd: number;
  debt_usd: number;
  available_borrows_usd: number;
  liq_threshold_bps: number;
  liq_threshold_pct: number;
  ltv_bps: number;
  ltv_pct: number;
  health_factor: number;
  status: string;
  price_drop_pct_to_liq: number;
  eth_liq_price_usd: number;
  last_reserve_symbol: string;
  last_seen_block: number;
  revm_verified: boolean;
  revm_gas_used: number;
  revm_sim_ms: number;
  plan: Plan | null;
}

export interface Fx {
  eth_inr?: number;
  eth_usd?: number;
  usd_inr?: number;
  /** "chainlink", "chainlink (stale)" or "reference". */
  source?: string;
  stale?: boolean;
  feed_updated_at?: number | null;
  feed?: string;
}

/** A row of `recent_liquidations` (on-chain LiquidationCall events). */
export interface RecentLiquidation {
  block?: number;
  tx_hash?: string;
  borrower?: string;
  user?: string;
  liquidator?: string;
  debt_repaid_usd?: number;
  collateral_seized_usd?: number;
  debt_asset?: string;
  collateral_asset?: string;
}

export interface LiquidationsResponse {
  positions: Position[];
  summary: Record<string, unknown>;
  fx: Fx;
  base_fee_gwei: number;
  status: string;
  discovered_borrowers_total: number;
  scanned_blocks_total: number;
  head_block: number | null;
  updated_ts: string;
  recent_liquidations: RecentLiquidation[];
}

export interface Inspected {
  user: string;
  ok: boolean;
  error?: string;
  [k: string]: unknown;
}

async function getJson<T>(path: string, signal?: AbortSignal): Promise<T> {
  const res = await fetch(`${API_BASE}${path}`, {
    signal,
    headers: { accept: "application/json" },
  });
  if (!res.ok) {
    throw new Error(`${path} -> HTTP ${res.status} ${res.statusText}`);
  }
  return (await res.json()) as T;
}

export function fetchOpportunities(signal?: AbortSignal) {
  return getJson<OpportunitiesResponse>("/api/opportunities", signal);
}

export function fetchLiquidations(signal?: AbortSignal) {
  return getJson<LiquidationsResponse>("/api/liquidations", signal);
}

/** GET /api/liquidations?inspect=0x… — returns { inspected } only. */
export async function inspectAddress(addr: string, signal?: AbortSignal) {
  const r = await getJson<{ inspected: Inspected }>(
    `/api/liquidations?inspect=${encodeURIComponent(addr)}`,
    signal,
  );
  return r.inspected;
}

/** Response from POST /api/simulate/{address}. Wei fields are U256 hex. */
export interface SimulationResult {
  success: boolean;
  error?: string;
  address?: string;
  opportunity_id?: string;
  block_simulated?: number;
  gas_used?: number;
  gas_price_wei?: string;
  gas_cost_wei?: string;
  collateral_seized?: string | null;
  debt_repaid?: string | null;
  liquidation_call_event_found?: boolean;
  estimated_net_profit_wei?: string;
  gross_profit_wei?: string;
  profit_basis?: string;
  collateral_asset?: string;
  debt_asset?: string;
  receive_a_token?: boolean;
  simulated_from?: string;
  broadcast?: boolean;
  /** Present when liquidationCall could not be encoded. */
  missing?: string[];
}

/** Optional overrides; the keeper records collateral/debt_asset as null. */
export interface SimulateOverrides {
  collateral_asset?: string;
  debt_asset?: string;
  debt_to_cover?: string;
  from?: string;
  receive_a_token?: boolean;
}

/**
 * POST /api/simulate/:address — runs a revm fork simulation.
 * Never throws on a failed simulation: the backend answers 200 with
 * success:false and a revert reason, which the caller renders.
 */
export async function simulate(
  address: string,
  overrides: SimulateOverrides = {},
  signal?: AbortSignal,
): Promise<SimulationResult> {
  const res = await fetch(`${API_BASE}/api/simulate/${address}`, {
    method: "POST",
    signal,
    headers: { "content-type": "application/json", accept: "application/json" },
    body: JSON.stringify(overrides),
  });
  if (!res.ok) {
    return { success: false, error: `HTTP ${res.status} ${res.statusText}` };
  }
  return (await res.json()) as SimulationResult;
}

/**
 * Joined view: a position plus the keeper's live wei bounty for that borrower,
 * when the keeper has one. The two endpoints are keyed differently
 * (Position.user vs Opportunity.borrower), so match on lowercased address.
 */
export interface Row extends Position {
  bountyWeiHex: string | null;
  opportunityId: string | null;
}

export function joinRows(
  positions: Position[],
  opportunities: Opportunity[],
): Row[] {
  const byBorrower = new Map<string, Opportunity>();
  for (const o of opportunities) {
    const key = (o.borrower ?? "").toLowerCase();
    if (key) byBorrower.set(key, o);
  }

  return positions.map((p) => {
    const match = byBorrower.get((p.user ?? "").toLowerCase());
    return {
      ...p,
      bountyWeiHex: match ? match.gross_profit_wei : null,
      opportunityId: match ? match.id : null,
    };
  });
}
