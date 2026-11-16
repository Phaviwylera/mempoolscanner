import { useState } from "react";
import { Download, Loader2, FileClock } from "lucide-react";
import { fetchAuditLogs } from "../lib/api";

/**
 * "Audit Logs" download button for the dashboard header.
 *
 * Clicking it fetches `exec_audit.jsonl` from the Rust API. If the dry run has
 * written nothing yet, the file comes back empty and we show "No dry run logs
 * yet" inline instead of handing the operator an empty file. Otherwise it
 * triggers a browser download of the real file.
 */
export default function AuditLogButton() {
  const [state, setState] = useState<"idle" | "loading" | "empty">("idle");

  const onClick = async () => {
    setState("loading");
    try {
      const text = await fetchAuditLogs();
      if (text.trim().length === 0) {
        setState("empty");
        // Clear the hint after a few seconds so the button returns to normal.
        setTimeout(() => setState("idle"), 4000);
        return;
      }
      // Trigger a client-side download of the audit log.
      const blob = new Blob([text], { type: "application/jsonl" });
      const url = URL.createObjectURL(blob);
      const a = document.createElement("a");
      a.href = url;
      a.download = "exec_audit.jsonl";
      document.body.appendChild(a);
      a.click();
      document.body.removeChild(a);
      URL.revokeObjectURL(url);
      setState("idle");
    } catch {
      // On a transport error just reset; the operator can retry.
      setState("idle");
    }
  };

  return (
    <div className="flex items-center gap-2">
      <button
        onClick={onClick}
        disabled={state === "loading"}
        title="Download exec_audit.jsonl (dry-run / execution audit log)"
        className="inline-flex items-center gap-1.5 rounded-lg border border-edge bg-card px-3 py-2 text-xs text-ink transition-colors hover:border-ink/40 hover:bg-edge/30 disabled:opacity-60"
      >
        {state === "loading" ? (
          <Loader2 size={13} className="animate-spin" />
        ) : (
          <Download size={13} />
        )}
        <span>Audit Logs</span>
      </button>
      {state === "empty" ? (
        <span className="inline-flex items-center gap-1 text-[11px] text-muted">
          <FileClock size={12} />
          No dry run logs yet
        </span>
      ) : null}
    </div>
  );
}
