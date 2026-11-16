import { useState } from "react";
import { Copy, Check } from "lucide-react";
import { shortAddr } from "../lib/format";

/**
 * Copy text to the clipboard, working in BOTH secure (HTTPS/localhost) and
 * insecure (plain-HTTP, e.g. the dashboard served from an EC2 IP) contexts.
 *
 * `navigator.clipboard` only exists in a secure context, so on http:// it is
 * undefined and the modern path silently fails. The textarea + execCommand
 * fallback is deprecated but still works everywhere and needs no HTTPS.
 */
async function writeClipboard(text: string): Promise<boolean> {
  if (navigator.clipboard && window.isSecureContext) {
    try {
      await navigator.clipboard.writeText(text);
      return true;
    } catch {
      // fall through to the legacy path
    }
  }

  try {
    const ta = document.createElement("textarea");
    ta.value = text;
    // Keep it out of view and unaffected by layout/scroll.
    ta.style.position = "fixed";
    ta.style.top = "-9999px";
    ta.style.left = "-9999px";
    ta.setAttribute("readonly", "");
    document.body.appendChild(ta);
    ta.select();
    ta.setSelectionRange(0, text.length);
    const ok = document.execCommand("copy");
    document.body.removeChild(ta);
    return ok;
  } catch {
    return false;
  }
}

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

    const ok = await writeClipboard(address);
    if (ok) {
      setCopied(true);
      setTimeout(() => setCopied(false), 1200);
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
