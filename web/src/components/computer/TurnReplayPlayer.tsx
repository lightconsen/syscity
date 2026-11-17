import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { createPortal } from "react-dom";
import { useTranslation } from "react-i18next";
import {
  AlertTriangle,
  ChevronLeft,
  ChevronRight,
  Loader2,
  ShieldAlert,
  X,
} from "lucide-react";
import type {
  SyscityWebSocketTransport,
  TurnReplay,
  TurnReplayStep,
} from "@/SyscityWebSocketTransport";

interface TurnReplayPlayerProps {
  transport: SyscityWebSocketTransport;
  turnId: string;
  onClose: () => void;
}

/**
 * One turn's desktop actions, played back in order.
 *
 * This is the other half of the desktop-run panel: the panel answers "what is
 * happening on this machine now", this answers "what did that answer actually
 * do". The data is `turns.replay` — a turn's `computer`-tool steps and the
 * steps of every run it started, folded into one ordered list — and the frames
 * are the CAS digests each step carries, fetched one at a time through
 * `attachment.get`.
 *
 * Read-only, like the panel. Nothing here can act: a replay that could re-run
 * the turn would be a second way to drive a machine, and the point of this
 * screen is that a person can see what already happened.
 *
 * One frame is fetched at a time, for the step being looked at. A turn can
 * hold thirty frames; the player showing them all would pay for images nobody
 * has stepped to yet, and the CAS keeps one copy however many steps show it.
 */
export function TurnReplayPlayer({ transport, turnId, onClose }: TurnReplayPlayerProps) {
  const { t } = useTranslation("computer");
  const [replay, setReplay] = useState<TurnReplay | null>(null);
  const [loading, setLoading] = useState(true);
  const [failed, setFailed] = useState(false);
  const [index, setIndex] = useState(0);

  const load = useCallback(async () => {
    setLoading(true);
    setFailed(false);
    const answer = await transport.turnsReplay(turnId);
    // `null` is "the gateway could not answer", not "the turn did nothing":
    // the two must not draw the same screen.
    setFailed(answer === null);
    setReplay(answer);
    setIndex(0);
    setLoading(false);
  }, [transport, turnId]);

  useEffect(() => {
    void load();
  }, [load]);

  const steps = replay?.steps ?? [];
  const current = steps[index] ?? null;
  const total = steps.length;

  const go = useCallback(
    (delta: number) => {
      setIndex((i) => Math.max(0, Math.min(steps.length - 1, i + delta)));
    },
    [steps.length]
  );

  // The list scrolls with the selection, so stepping with the arrows keeps the
  // current row in view even when the timeline is longer than the column.
  const activeRef = useRef<HTMLButtonElement | null>(null);
  useEffect(() => {
    activeRef.current?.scrollIntoView({ block: "nearest" });
  }, [index]);

  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") {
        onClose();
        return;
      }
      // Arrows step the timeline. They are the same two keys the buttons use,
      // so a keyboard reader never has to reach for the mouse.
      if (e.key === "ArrowLeft") go(-1);
      if (e.key === "ArrowRight") go(1);
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [go, onClose]);

  return createPortal(
    <div
      className="fixed inset-0 z-50 flex items-center justify-center bg-black/50 p-2 sm:p-6"
      role="dialog"
      aria-modal="true"
      aria-label={t("TurnReplay.aria")}
      data-testid="turn-replay-player"
    >
      <div className="w-full max-w-5xl max-h-full flex flex-col bg-card rounded-xl shadow-xl overflow-hidden">
        <div className="shrink-0 flex items-center gap-2 px-4 py-2.5 border-b border-subtle">
          <div className="min-w-0">
            <div className="text-sm font-medium text-primary truncate">
              {t("TurnReplay.title")}
            </div>
            {replay?.input && (
              <div className="text-[10px] text-secondary truncate" title={replay.input}>
                {t("TurnReplay.input")}: {replay.input}
              </div>
            )}
          </div>
          <button
            type="button"
            onClick={onClose}
            className="ml-auto p-1.5 rounded-md hover:bg-black/5 dark:hover:bg-white/5 text-secondary hover:text-primary transition"
            title={t("TurnReplay.closeEsc")}
            aria-label={t("TurnReplay.close")}
          >
            <X className="w-4 h-4" />
          </button>
        </div>

        {loading ? (
          <div className="flex items-center justify-center gap-2 px-6 py-16 text-xs text-secondary">
            <Loader2 className="w-4 h-4 animate-spin" />
            {t("TurnReplay.loading")}
          </div>
        ) : failed ? (
          <div className="flex flex-col items-center justify-center gap-2 px-6 py-16 text-xs text-secondary text-center">
            <AlertTriangle className="w-5 h-5 text-amber-500" />
            <span>{t("TurnReplay.failed")}</span>
            <button
              type="button"
              onClick={() => void load()}
              className="px-2 py-1 rounded-md border border-subtle hover:bg-black/5 dark:hover:bg-white/5"
            >
              {t("TurnReplay.retry")}
            </button>
          </div>
        ) : total === 0 ? (
          <div className="flex flex-col items-center justify-center gap-2 px-6 py-16 text-xs text-secondary text-center">
            <span className="text-primary">{t("TurnReplay.empty")}</span>
            <span className="opacity-70">{t("TurnReplay.emptyHint")}</span>
          </div>
        ) : (
          <div className="flex-1 min-h-0 flex flex-col md:flex-row">
            {/* The film: every step, in order, one click from its frame. */}
            <div className="md:w-72 shrink-0 md:border-r border-b md:border-b-0 border-subtle max-h-48 md:max-h-none overflow-y-auto">
              {steps.map((step, i) => (
                <ReplayStepRow
                  key={step.step}
                  step={step}
                  index={i}
                  active={i === index}
                  onSelect={() => setIndex(i)}
                  rowRef={i === index ? activeRef : undefined}
                />
              ))}
            </div>

            <div className="flex-1 min-w-0 flex flex-col">
              <div className="shrink-0 flex items-center gap-2 px-4 py-2 border-b border-subtle text-[11px]">
                <button
                  type="button"
                  onClick={() => go(-1)}
                  disabled={index === 0}
                  className="p-1 rounded-md hover:bg-black/5 dark:hover:bg-white/5 text-secondary hover:text-primary transition disabled:opacity-30"
                  title={t("TurnReplay.prev")}
                  aria-label={t("TurnReplay.prev")}
                  data-testid="turn-replay-prev"
                >
                  <ChevronLeft className="w-4 h-4" />
                </button>
                <button
                  type="button"
                  onClick={() => go(1)}
                  disabled={index >= total - 1}
                  className="p-1 rounded-md hover:bg-black/5 dark:hover:bg-white/5 text-secondary hover:text-primary transition disabled:opacity-30"
                  title={t("TurnReplay.next")}
                  aria-label={t("TurnReplay.next")}
                  data-testid="turn-replay-next"
                >
                  <ChevronRight className="w-4 h-4" />
                </button>
                <span className="text-secondary" data-testid="turn-replay-counter">
                  {t("TurnReplay.step", { n: index + 1, total })}
                </span>
                <span className="ml-auto text-secondary">{t("TurnReplay.keyboardHint")}</span>
              </div>

              {current && (
                <FrameView
                  /* One mount per step, so the before/after choice starts over
                     rather than carrying a selection into a step that may not
                     have that side. */
                  key={current.step}
                  transport={transport}
                  step={current}
                  stepNumber={index + 1}
                />
              )}

              {replay && replay.runs.length > 0 && <RunCosts replay={replay} />}
            </div>
          </div>
        )}
      </div>
    </div>,
    document.body
  );
}

/** Verification, as a row draws it: "yes" / "no" / "elsewhere" / "unknown". */
function verdict(step: TurnReplayStep): "yes" | "no" | "elsewhere" | "unknown" {
  if (step.verified === true) return "yes";
  if (step.verified === false) return "no";
  // A null verdict has two meanings and only one of them is actionable: the
  // screen changed, just not where the step aimed. Drawing both as
  // "Unverified" hides the miss behind "nobody checked".
  if (step.aim_missed) return "elsewhere";
  return "unknown";
}

function ReplayStepRow({
  step,
  index,
  active,
  onSelect,
  rowRef,
}: {
  step: TurnReplayStep;
  index: number;
  active: boolean;
  onSelect: () => void;
  rowRef?: React.RefObject<HTMLButtonElement | null>;
}) {
  const { t } = useTranslation("computer");
  const status = verdict(step);
  const statusColor =
    status === "yes"
      ? "text-green-600 dark:text-green-400"
      : status === "no"
        ? "text-red-600 dark:text-red-400"
        : status === "elsewhere"
          ? "text-amber-600 dark:text-amber-400"
          : "text-secondary";

  return (
    <button
      type="button"
      ref={rowRef}
      onClick={onSelect}
      data-testid="turn-replay-step"
      data-step={step.step}
      data-source={step.source}
      data-active={active}
      aria-current={active ? "step" : undefined}
      className={`w-full text-left px-3 py-2 border-b border-subtle last:border-b-0 transition ${
        active
          ? "bg-primary-500/10"
          : "hover:bg-black/[0.03] dark:hover:bg-white/[0.03]"
      }`}
    >
      <div className="flex items-center gap-1.5">
        <span className="text-[10px] text-secondary shrink-0">
          {t("ComputerPanel.step", { n: index + 1 })}
        </span>
        {/* Which half of the turn wrote the row: the computer tool inside it,
            or a run it started. A reader chasing a step needs to know which
            file to open. */}
        <span
          className="text-[9px] px-1 rounded bg-black/5 dark:bg-white/10 text-secondary shrink-0"
          title={
            step.source === "run"
              ? t("TurnReplay.runOf", { id: (step.run_id || "").slice(0, 8) })
              : undefined
          }
        >
          {t(`TurnReplay.source.${step.source}`)}
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
        <span className={`ml-auto text-[10px] shrink-0 ${statusColor}`}>
          {t(`ComputerPanel.verified.${status}`)}
        </span>
      </div>
      <div className="text-xs text-primary break-words mt-0.5">
        {step.action || step.result || "—"}
      </div>
      {/* What the adapter said happened, when it is more than the action
          restated: a run step's `press 3` and its `typed 3` are two facts. */}
      {step.result && step.result !== step.action && (
        <div className="text-[10px] text-secondary break-words mt-0.5">{step.result}</div>
      )}
    </button>
  );
}

/**
 * The frame for the step being looked at.
 *
 * `before` and `after` are two readings of the same screen, taken either side
 * of the action; a step that has both can toggle between them, which is how a
 * reader sees what changed rather than only what the action claimed.
 */
function FrameView({
  transport,
  step,
  stepNumber,
}: {
  transport: SyscityWebSocketTransport;
  step: TurnReplayStep;
  stepNumber: number;
}) {
  const { t } = useTranslation("computer");
  const [side, setSide] = useState<"before" | "after">("after");
  const [url, setUrl] = useState<string | null>(null);
  const [loading, setLoading] = useState(false);

  const before = step.before_ref ?? null;
  const after = step.after_ref ?? null;
  const digest = side === "before" ? before : after;
  // A step with only one frame shows that one, whichever side is selected.
  const effective = digest ?? after ?? before;

  useEffect(() => {
    setUrl(null);
    if (!effective) return;
    let alive = true;
    setLoading(true);
    transport
      .computerFrameUrl(effective)
      .then((u) => {
        if (alive) setUrl(u);
      })
      .finally(() => {
        if (alive) setLoading(false);
      });
    return () => {
      alive = false;
    };
  }, [transport, effective]);

  if (!before && !after) {
    return (
      <div className="flex-1 flex items-center justify-center px-6 text-xs text-secondary">
        {t("TurnReplay.noFrame")}
      </div>
    );
  }

  return (
    <div className="flex-1 min-h-0 flex flex-col overflow-y-auto p-4 gap-2">
      <div className="flex items-center gap-2 text-[10px]">
        {step.effect && (
          <span className="text-secondary">
            {t(`ComputerPanel.effect.${step.effect}`, { defaultValue: step.effect })}
          </span>
        )}
        {step.settle_ms != null && (
          <span className="text-secondary">{t("TurnReplay.settle", { ms: step.settle_ms })}</span>
        )}
        {before && after && (
          <span className="ml-auto inline-flex rounded-md border border-subtle overflow-hidden">
            {(["before", "after"] as const).map((s) => (
              <button
                key={s}
                type="button"
                onClick={() => setSide(s)}
                data-testid={`turn-replay-side-${s}`}
                aria-pressed={side === s}
                className={`px-2 py-0.5 transition ${
                  side === s
                    ? "bg-primary-500/15 text-primary-600 dark:text-primary-400"
                    : "text-secondary hover:bg-black/5 dark:hover:bg-white/5"
                }`}
              >
                {t(`ComputerPanel.${s}`)}
              </button>
            ))}
          </span>
        )}
      </div>

      {step.recovery && (
        <div className="text-[10px] text-amber-600 dark:text-amber-400" data-testid="turn-replay-recovery">
          {step.recovery}
        </div>
      )}

      {loading && !url ? (
        <div className="flex-1 flex items-center justify-center gap-2 text-xs text-secondary">
          <Loader2 className="w-4 h-4 animate-spin" />
          {t("TurnReplay.frameLoading")}
        </div>
      ) : url ? (
        <figure className="m-0 flex flex-col gap-1">
          <figcaption className="text-[9px] uppercase tracking-wide text-secondary">
            {t("TurnReplay.frame", { n: stepNumber })}
          </figcaption>
          <img
            src={url}
            alt={t("TurnReplay.frame", { n: stepNumber })}
            className="w-full rounded border border-subtle"
            data-testid="turn-replay-frame"
            data-digest={effective ?? undefined}
          />
        </figure>
      ) : (
        <div className="flex-1 flex items-center justify-center text-xs text-secondary">
          {t("TurnReplay.frameGone")}
        </div>
      )}
    </div>
  );
}

/** The run cost lines behind a timeline, one per run. */
function RunCosts({ replay }: { replay: TurnReplay }) {
  const { t } = useTranslation("computer");
  const totals = useMemo(
    () =>
      replay.runs.reduce(
        (acc, run) => ({
          steps: acc.steps + run.steps,
          calls: acc.calls + run.model_calls,
          images: acc.images + run.images_attached,
          tokens: acc.tokens + run.tokens_in + run.tokens_out,
        }),
        { steps: 0, calls: 0, images: 0, tokens: 0 }
      ),
    [replay.runs]
  );

  return (
    <div
      className="shrink-0 px-4 py-2 border-t border-subtle text-[10px] text-secondary"
      data-testid="turn-replay-cost"
      data-tokens={totals.tokens}
    >
      <div>
        {t("ComputerPanel.cost", {
          steps: totals.steps,
          calls: totals.calls,
          images: totals.images,
          tokens: totals.tokens,
        })}
      </div>
      <div className="opacity-70">
        {replay.runs.map((run) => (
          <div key={run.run_id} className="truncate" title={run.goal}>
            {t("TurnReplay.runLine", {
              id: run.run_id.slice(0, 8),
              goal: run.goal,
              stop: run.stop_reason ?? "—",
            })}
          </div>
        ))}
      </div>
    </div>
  );
}
