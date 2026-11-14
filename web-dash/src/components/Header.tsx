import { Users, HeartPulse, Siren, CircleDollarSign } from "lucide-react";
import type { ReactNode } from "react";
import { BAND_TEXT, formatHf, formatUsd, hfBand } from "../lib/format";
import type { Row } from "../lib/api";

function Card({
  icon,
  label,
  value,
  sub,
  tone = "text-ink",
}: {
  icon: ReactNode;
  label: string;
  value: string;
  sub?: string;
  tone?: string;
}) {
  return (
    <div className="flex-1 min-w-[180px] rounded-lg border border-edge bg-card px-4 py-3">
      <div className="flex items-center gap-2 text-[11px] uppercase tracking-widest text-muted">
        {icon}
        <span>{label}</span>
      </div>
      <div className={`mt-2 text-2xl leading-none tabular-nums ${tone}`}>{value}</div>
      {sub ? <div className="mt-1 text-[11px] text-muted">{sub}</div> : null}
    </div>
  );
}

export default function Header({
  rows,
  ethUsd,
  fxSource,
  updatedTs,
}: {
  rows: Row[];
  ethUsd: number | undefined;
  fxSource: string | undefined;
  updatedTs: string | undefined;
}) {
  const tracked = rows.length;

  const hfs = rows
    .map((r) => r.health_factor)
    .filter((h): h is number => typeof h === "number" && Number.isFinite(h));
  const lowest = hfs.length ? Math.min(...hfs) : null;

  const liquidatable = rows.filter(
    (r) => Number.isFinite(r.health_factor) && r.health_factor < 1.0,
  ).length;

  return (
    <div className="flex flex-wrap gap-3">
      <Card
        icon={<Users size={13} />}
        label="Tracked Borrowers"
        value={tracked.toString()}
        sub={updatedTs ? `snapshot ${updatedTs}` : undefined}
      />
      <Card
        icon={<HeartPulse size={13} />}
        label="Lowest Health Factor"
        value={formatHf(lowest)}
        tone={lowest === null ? "text-ink" : BAND_TEXT[hfBand(lowest)]}
        sub="liquidatable below 1.0000"
      />
      <Card
        icon={<Siren size={13} />}
        label="Liquidatable"
        value={liquidatable.toString()}
        tone={liquidatable > 0 ? "text-risk" : "text-profit"}
        sub={`of ${tracked} tracked`}
      />
      <Card
        icon={<CircleDollarSign size={13} />}
        label="Aave Oracle ETH"
        value={ethUsd === undefined ? "—" : `$${formatUsd(ethUsd)}`}
        sub={fxSource ? `source: ${fxSource}` : undefined}
      />
    </div>
  );
}
