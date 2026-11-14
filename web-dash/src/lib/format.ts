/**
 * Integer-only formatting helpers.
 *
 * The Rust backend serializes alloy `U256` as a HEX STRING ("0xde0b6b3a7640000"),
 * not a decimal number. parseFloat on those values silently yields garbage
 * (and loses precision above 2^53), so every wei value goes through BigInt.
 */

const WEI_PER_ETH = 10n ** 18n;

/** Parse a U256 JSON value ("0x..", decimal string, or number) into a bigint. */
export function toBigInt(v: string | number | null | undefined): bigint {
  if (v === null || v === undefined) return 0n;
  if (typeof v === "number") return Number.isFinite(v) ? BigInt(Math.trunc(v)) : 0n;
  const s = v.trim();
  if (s === "") return 0n;
  try {
    // BigInt() natively accepts both "0x…" and plain decimal strings.
    return BigInt(s);
  } catch {
    return 0n;
  }
}

/**
 * wei -> ETH string, using only integer arithmetic (never float division).
 * formatEth(10n ** 18n) === "1.000000"
 */
export function formatEth(wei: bigint, decimals = 6): string {
  const negative = wei < 0n;
  const abs = negative ? -wei : wei;

  const whole = abs / WEI_PER_ETH;
  const frac = abs % WEI_PER_ETH;

  let out: string;
  if (decimals <= 0) {
    out = whole.toString();
  } else {
    const fracStr = frac.toString().padStart(18, "0").slice(0, decimals);
    out = `${whole.toString()}.${fracStr}`;
  }
  return negative ? `-${out}` : out;
}

/** Convenience: raw U256 JSON value straight to an ETH string. */
export function weiToEth(v: string | number | null | undefined, decimals = 6): string {
  return formatEth(toBigInt(v), decimals);
}

/** USD with thousands separators. Used for the plan math, which is f64 USD. */
export function formatUsd(n: number | null | undefined, decimals = 2): string {
  if (n === null || n === undefined || !Number.isFinite(n)) return "—";
  return n.toLocaleString("en-US", {
    minimumFractionDigits: decimals,
    maximumFractionDigits: decimals,
  });
}

/** 0x1234…cdef */
export function shortAddr(a: string | null | undefined, lead = 6, tail = 4): string {
  if (!a) return "—";
  if (a.length <= lead + tail + 2) return a;
  return `${a.slice(0, lead)}…${a.slice(-tail)}`;
}

export function isAddress(s: string): boolean {
  return /^0x[0-9a-fA-F]{40}$/.test(s.trim());
}

/** Health-factor risk band. Thresholds per spec: >1.5 healthy, 1.0–1.5 warn, <1.0 liquidatable. */
export type Band = "healthy" | "warn" | "risk";

export function hfBand(hf: number | null | undefined): Band {
  if (hf === null || hf === undefined || !Number.isFinite(hf)) return "warn";
  if (hf < 1.0) return "risk";
  if (hf <= 1.5) return "warn";
  return "healthy";
}

export const BAND_TEXT: Record<Band, string> = {
  healthy: "text-profit",
  warn: "text-warn",
  risk: "text-risk",
};

export const BAND_BAR: Record<Band, string> = {
  healthy: "bg-profit",
  warn: "bg-warn",
  risk: "bg-risk",
};

/** Map HF onto a 0–100% bar, saturating at 2.0 so the bar stays readable. */
export function hfPercent(hf: number | null | undefined): number {
  if (hf === null || hf === undefined || !Number.isFinite(hf)) return 0;
  return Math.max(2, Math.min(100, (hf / 2) * 100));
}

export function formatHf(hf: number | null | undefined): string {
  if (hf === null || hf === undefined || !Number.isFinite(hf)) return "—";
  if (hf > 1e6) return "∞";
  return hf.toFixed(4);
}
