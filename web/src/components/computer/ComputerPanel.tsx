import { useCallback, useEffect, useRef, useState } from "react";
import { useTranslation } from "react-i18next";
import {
  AlertTriangle,
  CheckCircle2,
  ChevronDown,
  ChevronRight,
  Loader2,
  Monitor,
  ShieldAlert,
  X,
  XCircle,
} from "lucide-react";
import type { SyscityWebSocketTransport } from "@/SyscityWebSocketTransport";
import { useComputerStore, type ComputerRunEntry, type ComputerStepView } from "@/stores/computerStore";

interface ComputerPanelProps {
  transport: SyscityWebSocketTransport;
  /**
   * The conversation on screen. The transport owns this, not a store copy: a
   * failed first render is exactly what happens when the panel reads a field
   * nobody writes.
   */
  sessionId: string | null;
  onClose: () => void;
}

/**
 * Right-pane view of a desktop run: what it did, in order, with the screen
 * before and after each action.
 *
 * Read-only by design. The panel exists because a person could not *see* a run
 * that was already driving their machine — not to be a second way to drive it.
 *
 * Two sources, one shape: the live `computer.step` events (and `computer.status`
 * on open) describe a run while the gateway is up, and `computer.runs` reads the
 * checkpoints it leaves behind. A run started in this conversation is shown
 * either way, which is the point — "the run finished while you were reloading"
 * must not read as "there was never a run".
 */
export function ComputerPanel({ transport, sessionId, onClose }: ComputerPanelProps) {
  const { t } = useTranslation("computer");
  const runs = useComputerStore((s) => s.runs);
  const order = useComputerStore((s) => s.order);
  const selectedRunId = useComputerStore((s) => s.selectedRunId);
  const selectRun = useComputerStore((s) => s.selectRun);
  const bindSession = useComputerStore((s) => s.bindSession);
  const mergeRecord = useComputerStore((s) => s.mergeRecord);
  const mergeStatus = useComputerStore((s) => s.mergeStatus);

  const [loading, setLoading] = useState(true);
  const [failed, setFailed] = useState(false);

  useEffect(() => {
    const handler = (e: KeyboardEvent) => {
      if (e.key === "Escape") onClose();
    };
    window.addEventListener("keydown", handler);
    return () => window.removeEventListener("keydown", handler);
  }, [onClose]);

  // A run belongs to the conversation that asked for it; switching sessions
  // must not carry yesterday's run across.
  useEffect(() => {
    bindSession(sessionId || null);
  }, [sessionId, bindSession]);

  /**
   * Load what the disk knows about this conversation's runs.
   *
   * Only this session's: the file records `session_id` precisely so a panel on
   * a shared gateway does not show every run on the machine — including other
   * people's.
   */
  const load = useCallback(async () => {
    setLoading(true);
    setFailed(false);
    try {
      const [{ runs: records }, live] = await Promise.all([
        transport.computerRuns(true),
        transport.computerStatus(),
      ]);
      for (const record of records) {
        // A run that names another conversation is that conversation's, not
        // this one's. A run with *no* session is kept: it was started outside
        // any conversation (the CLI, a scheduled task) and its events broadcast
        // to every client, so showing it here matches how it announced itself.
        if (record.session_id && record.session_id !== sessionId) continue;
        mergeRecord(record);
      }
      // The live list is every run this gateway has; the same ownership rule
      // as the disk records applies. A run with `session_id: null` started by a
      // bare `computer.run` announced itself to every client, so it is kept.
      const liveRuns = live?.runs ?? [];
      for (const view of liveRuns) {
        if (view.finished) continue;
        if (view.session_id && view.session_id !== sessionId) continue;
        mergeStatus(view, sessionId || null);
      }
      setFailed(records.length === 0 && liveRuns.length === 0 && live === null);
    } finally {
      setLoading(false);
    }
  }, [transport, sessionId, mergeRecord, mergeStatus]);

  useEffect(() => {
    void load();
  }, [load]);

  const ordered = order.map((id) => runs[id]).filter(Boolean) as ComputerRunEntry[];
  const current =
    (selectedRunId ? runs[selectedRunId] : undefined) ??
    ordered[ordered.length - 1] ??
    null;

  return (
    <div
      className="flex-1 min-w-0 border-l border-subtle bg-page flex flex-col overflow-hidden"
      role="complementary"
      aria-label={t("ComputerPanel.aria")}
      data-testid="computer-panel"
    >
      <div className="shrink-0 h-14 flex items-center justify-between px-4 border-b border-subtle">
        <div className="flex items-center gap-2 min-w-0">
          <Monitor className="w-5 h-5 text-primary-500 shrink-0" />
          <div className="min-w-0">
            <div className="text-sm font-medium text-primary truncate">
              {t("ComputerPanel.title")}
            </div>
            <div className="text-[10px] text-secondary truncate" title={current?.goal}>
              {current?.goal || t("ComputerPanel.empty")}
            </div>
          </div>
        </div>
        <button
          type="button"
          onClick={onClose}
          className="p-1.5 rounded-md hover:bg-black/5 dark:hover:bg-white/5 text-secondary hover:text-primary transition"
          title={t("ComputerPanel.closeEsc")}
          aria-label={t("ComputerPanel.close")}
        >
          <X className="w-4 h-4" />
        </button>
      </div>

      {/* More than one run in this conversation: let the reader pick. One run:
          the list is noise. */}
      {ordered.length > 1 && (
        <div className="shrink-0 flex gap-1 px-3 py-2 overflow-x-auto border-b border-subtle">
          {ordered.map((run) => (
            <button
              key={run.runId}
              type="button"
              onClick={() => selectRun(run.runId)}
              data-testid="computer-run-tab"
              data-run-id={run.runId}
              data-active={run.runId === current?.runId}
              className={`shrink-0 max-w-[12rem] truncate text-[11px] px-2 py-1 rounded-md transition ${
                run.runId === current?.runId
                  ? "bg-primary-500/10 text-primary-600 dark:text-primary-400"
                  : "text-secondary hover:bg-black/5 dark:hover:bg-white/5"
              }`}
              title={run.goal}
            >
              {run.goal || run.runId.slice(0, 8)}
            </button>
          ))}
        </div>
      )}

      {loading && !current ? (
        <div className="flex-1 flex items-center justify-center text-xs text-secondary gap-2">
          <Loader2 className="w-4 h-4 animate-spin" />
          {t("ComputerPanel.loading")}
        </div>
      ) : failed ? (
        <div className="flex-1 flex flex-col items-center justify-center text-xs text-secondary gap-2 px-6 text-center">
          <AlertTriangle className="w-5 h-5 text-amber-500" />
          <span>{t("ComputerPanel.failed")}</span>
          <button
            type="button"
            onClick={() => void load()}
            className="px-2 py-1 rounded-md border border-subtle hover:bg-black/5 dark:hover:bg-white/5"
          >
            {t("ComputerPanel.retry")}
          </button>
        </div>
      ) : !current ? (
        <div className="flex-1 flex flex-col items-center justify-center text-xs text-secondary gap-2 px-6 text-center">
          <Monitor className="w-6 h-6 opacity-40" />
          <span className="text-primary">{t("ComputerPanel.empty")}</span>
          <span className="opacity-70">{t("ComputerPanel.emptyHint")}</span>
        </div>
      ) : (
        <RunBody transport={transport} run={current} />
      )}
    </div>
  );
}

function phaseIcon(phase: ComputerRunEntry["phase"]) {
  switch (phase) {
    case "running":
    case "stepping":
      return <Loader2 className="w-3.5 h-3.5 animate-spin text-blue-500" />;
    case "done":
      return <CheckCircle2 className="w-3.5 h-3.5 text-green-500" />;
    case "paused":
      return <Loader2 className="w-3.5 h-3.5 text-amber-500" />;
    case "cancelled":
    case "failed":
      return <XCircle className="w-3.5 h-3.5 text-red-500" />;
  }
}

function RunBody({
  transport,
  run,
}: {
  transport: SyscityWebSocketTransport;
  run: ComputerRunEntry;
}) {
  const { t } = useTranslation("computer");
  const summary = run.summary;
  const tokens = summary ? summary.tokens_in + summary.tokens_out : 0;

  return (
    <div className="flex-1 min-h-0 flex flex-col">
      <div className="shrink-0 flex items-center gap-2 px-4 py-2 border-b border-subtle text-xs">
        {phaseIcon(run.phase)}
        <span className="font-medium text-primary">
          {t(`ComputerPanel.phase.${run.phase}`)}
        </span>
        {run.interrupted && (
          <span className="text-amber-600 dark:text-amber-400 truncate">
            {t("ComputerPanel.interrupted")}
          </span>
        )}
        <span className="ml-auto text-secondary truncate" title={run.runId}>
          {t("ComputerPanel.runOf", { id: run.runId.slice(0, 8) })}
        </span>
      </div>

      {/* Why the run stopped, when it did: a run that ended "asking for a
          person" must not look like one that ran out of steps. */}
      {run.message && (
        <div
          className="shrink-0 px-4 py-2 text-[11px] text-secondary border-b border-subtle"
          data-testid="computer-run-message"
        >
          {run.message}
        </div>
      )}

      {run.vision && (
        <div className="shrink-0 px-4 py-1.5 text-[10px] text-secondary border-b border-subtle">
          {t("ComputerPanel.vision", { verdict: run.vision })}
        </div>
      )}

      <div className="flex-1 min-h-0 overflow-y-auto">
        {run.steps.length === 0 ? (
          <div className="px-4 py-6 text-xs text-secondary">{t("ComputerPanel.loading")}</div>
        ) : (
          run.steps.map((step) => (
            <StepRow key={step.step} transport={transport} step={step} />
          ))
        )}
      </div>

      {summary && (
        <div
          className="shrink-0 px-4 py-2 border-t border-subtle text-[10px] text-secondary"
          data-testid="computer-run-cost"
          data-tokens={tokens}
        >
          <div>
            {t("ComputerPanel.cost", {
              steps: summary.steps,
              calls: summary.model_calls,
              images: summary.images_attached,
              tokens,
            })}
          </div>
          <div className="opacity-70">
            {t("ComputerPanel.costTokens", {
              in: summary.tokens_in,
              out: summary.tokens_out,
            })}
          </div>
        </div>
      )}
    </div>
  );
}

function StepRow({
  transport,
  step,
}: {
  transport: SyscityWebSocketTransport;
  step: ComputerStepView;
}) {
  const { t } = useTranslation("computer");
  const [open, setOpen] = useState(false);

  const hasFrame = Boolean(step.beforeRef || step.afterRef);
  const status = step.verified === true ? "yes" : step.verified === false ? "no" : "unknown";
  const statusColor =
    status === "yes"
      ? "text-green-600 dark:text-green-400"
      : status === "no"
        ? "text-red-600 dark:text-red-400"
        : "text-secondary";

  return (
    <div className="border-b border-subtle last:border-b-0" data-testid="computer-step" data-step={step.step}>
      <button
        type="button"
        onClick={() => hasFrame && setOpen((v) => !v)}
        className="w-full flex items-start gap-2 px-4 py-2 text-left hover:bg-black/[0.03] dark:hover:bg-white/[0.03]"
      >
        <span className="shrink-0 w-4 text-[10px] text-secondary pt-0.5">
          {hasFrame ? (
            open ? (
              <ChevronDown className="w-3.5 h-3.5" />
            ) : (
              <ChevronRight className="w-3.5 h-3.5" />
            )
          ) : (
            <span className="inline-block w-3.5" />
          )}
        </span>
        <span className="min-w-0 flex-1">
          <span className="flex items-center gap-1.5">
            <span className="text-[10px] text-secondary shrink-0">
              {t("ComputerPanel.step", { n: step.step + 1 })}
            </span>
            {step.sensitive && (
              <ShieldAlert
                className="w-3 h-3 text-amber-500 shrink-0"
                aria-label={t("ComputerPanel.sensitive")}
              />
            )}
            {step.rung && (
              <span className="text-[9px] px-1 rounded bg-black/5 dark:bg-white/10 text-secondary shrink-0">
                {step.rung}
              </span>
            )}
          </span>
          <span className="block text-xs text-primary break-words mt-0.5">{step.action}</span>
          {step.result && step.result !== step.action && (
            <span className="block text-[10px] text-secondary break-words mt-0.5">
              {step.result}
            </span>
          )}
        </span>
        <span className={`shrink-0 text-[10px] ${statusColor} pt-0.5`}>
          {t(`ComputerPanel.verified.${status}`)}
        </span>
      </button>

      {open && (
        <div className="px-4 pb-3 flex flex-col gap-2">
          {step.effect && (
            <div className="text-[10px] text-secondary">
              {t(`ComputerPanel.effect.${step.effect}`, {
                defaultValue: step.effect,
              })}
            </div>
          )}
          <FramePair
            transport={transport}
            beforeRef={step.beforeRef}
            afterRef={step.afterRef}
          />
        </div>
      )}
    </div>
  );
}

/**
 * The two frames a step is drawn against, fetched lazily.
 *
 * Lazy because a run can finish with thirty steps: fetching thirty screenshots
 * to draw one collapsed list would be the panel paying for images nobody asked
 * to see. `attachment.get` is by digest, and the CAS keeps one copy of a frame
 * however many steps show it.
 */
function FramePair({
  transport,
  beforeRef,
  afterRef,
}: {
  transport: SyscityWebSocketTransport;
  beforeRef?: string | null;
  afterRef?: string | null;
}) {
  const { t } = useTranslation("computer");
  const [before, setBefore] = useState<string | null>(null);
  const [after, setAfter] = useState<string | null>(null);
  const loaded = useRef(false);

  useEffect(() => {
    if (loaded.current) return;
    loaded.current = true;
    const load = async () => {
      if (beforeRef) setBefore(await transport.computerFrameUrl(beforeRef));
      if (afterRef) setAfter(await transport.computerFrameUrl(afterRef));
    };
    void load();
  }, [transport, beforeRef, afterRef]);

  if (!before && !after) {
    return <div className="text-[10px] text-secondary">{t("ComputerPanel.noFrame")}</div>;
  }

  return (
    <div className="flex flex-col gap-2">
      {before && (
        <figure className="m-0">
          <figcaption className="text-[9px] uppercase tracking-wide text-secondary mb-1">
            {t("ComputerPanel.before")}
          </figcaption>
          <img
            src={before}
            alt={t("ComputerPanel.before")}
            className="w-full rounded border border-subtle"
            data-testid="computer-frame-before"
          />
        </figure>
      )}
      {after && (
        <figure className="m-0">
          <figcaption className="text-[9px] uppercase tracking-wide text-secondary mb-1">
            {t("ComputerPanel.after")}
          </figcaption>
          <img
            src={after}
            alt={t("ComputerPanel.after")}
            className="w-full rounded border border-subtle"
            data-testid="computer-frame-after"
          />
        </figure>
      )}
    </div>
  );
}
