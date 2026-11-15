import { useState } from "react";
import { Copy, Check } from "lucide-react";
import { shortAddr } from "../lib/format";

/**
 * A truncated address that shows the full value on hover (`title`) and copies
 * the full `0x…` string to the clipboard on click of the copy icon.
 */
export default function CopyableAddress({
  address,
  lead = 6,
  tail = 4,
  className = "",
}: {
  address: string | null | undefined;
  lead?: number;
  tail?: number;
  className?: string;
}) {
  const [copied, setCopied] = useState(false);

  if (!address) return <span className="text-muted">—</span>;

  const copy = async (e: React.MouseEvent) => {
    // Never let a copy click also trigger row selection or navigation.
    e.stopPropagation();
    try {
      await navigator.clipboard.writeText(address);
      setCopied(true);
      setTimeout(() => setCopied(false), 1200);
    } catch {
      // Clipboard can be blocked (non-secure context); the title still shows it.
    }
  };

  return (
    <span className={`inline-flex items-center gap-1 ${className}`}>
      <span title={address} className="tabular-nums">
        {shortAddr(address, lead, tail)}
      </span>
      <button
        onClick={copy}
        title={copied ? "Copied" : `Copy ${address}`}
        aria-label="Copy address"
        className="shrink-0 text-muted transition-colors hover:text-ink"
      >
        {copied ? <Check size={11} className="text-profit" /> : <Copy size={11} />}
      </button>
    </span>
  );
}
