import { useState, useEffect, useRef } from "react";
import { useTranslation } from "react-i18next";

function BrainIcon({ className }: { className?: string }) {
  return (
    <svg className={className} viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round">
      <path d="M9.5 2A2.5 2.5 0 0 1 12 4.5v15a2.5 2.5 0 0 1-4.96.44 2.5 2.5 0 0 1-2.96-3.08 3 3 0 0 1-.34-5.58 2.5 2.5 0 0 1 1.32-4.24 2.5 2.5 0 0 1 1.98-3A2.5 2.5 0 0 1 9.5 2Z" />
      <path d="M14.5 2A2.5 2.5 0 0 0 12 4.5v15a2.5 2.5 0 0 0 4.96.44 2.5 2.5 0 0 0 2.96-3.08 3 3 0 0 0 .34-5.58 2.5 2.5 0 0 0-1.32-4.24 2.5 2.5 0 0 0-1.98-3A2.5 2.5 0 0 0 14.5 2Z" />
    </svg>
  );
}

export function ReasoningPart({ text, nonCollapsible }: { text: string; nonCollapsible?: boolean }) {
  const { t } = useTranslation("common");
  const [expanded, setExpanded] = useState(true);
  const [displayedText, setDisplayedText] = useState("");
  const [done, setDone] = useState(false);
  const targetRef = useRef("");
  const animatingRef = useRef(false);
  const doneTimerRef = useRef<ReturnType<typeof setTimeout> | null>(null);
  const prevLengthRef = useRef(0);
  const isHistoryRef = useRef(false);

  const isWaiting = text.length === 0;

  // Detect streaming vs history: big jump = history (show immediately)
  useEffect(() => {
    if (isWaiting) {
      prevLengthRef.current = 0;
      isHistoryRef.current = false;
      return;
    }

    if (prevLengthRef.current === 0 && text.length > 20) {
      // Text arrived all at once — history view, no animation
      isHistoryRef.current = true;
      setDisplayedText(text);
      setDone(true);
    }

    prevLengthRef.current = text.length;
  }, [text, isWaiting]);

  // Detect completion: text stops growing for 1.5s (streaming only)
  useEffect(() => {
    if (isWaiting || isHistoryRef.current) return;
    targetRef.current = text;

    if (doneTimerRef.current) {
      clearTimeout(doneTimerRef.current);
    }

    if (displayedText.length >= text.length) {
      doneTimerRef.current = setTimeout(() => {
        setDone(true);
      }, 1500);
    }

    return () => {
      if (doneTimerRef.current) clearTimeout(doneTimerRef.current);
    };
  }, [text, displayedText.length, isWaiting]);

  // Smooth typing animation (streaming only)
  useEffect(() => {
    if (isWaiting || done || isHistoryRef.current) return;
    if (displayedText.length >= targetRef.current.length) return;
    if (animatingRef.current) return;

    animatingRef.current = true;
    let frameId: number;

    const animate = () => {
      setDisplayedText((prev) => {
        const target = targetRef.current;
        if (prev.length < target.length) {
          const gap = target.length - prev.length;
          const chunkSize = gap > 30 ? 4 : gap > 10 ? 2 : 1;
          frameId = requestAnimationFrame(animate);
          return target.slice(0, prev.length + chunkSize);
        }
        animatingRef.current = false;
        return prev;
      });
    };

    frameId = requestAnimationFrame(animate);
    return () => {
      cancelAnimationFrame(frameId);
      animatingRef.current = false;
    };
  }, [text, displayedText, isWaiting, done]);

  // ── Waiting state ───────────────────────────────────────────────
  // Don't show anything while waiting for reasoning content to arrive.
  if (isWaiting) {
    return null;
  }

  // ── Non-collapsible: always show full content ───────────────────
  if (nonCollapsible) {
    return (
      <div className="my-3">
        <div className="flex items-center gap-2 text-[11px] font-medium text-secondary mb-2">
          <BrainIcon className="w-3.5 h-3.5" />
          <span>{t("ReasoningPart.thinking")}</span>
        </div>
        <hr className="border-subtle mb-2" />
        <div className="text-[11px] text-secondary font-mono whitespace-pre-wrap leading-relaxed">
          {text}
        </div>
      </div>
    );
  }

  // ── Done (collapsed) ────────────────────────────────────────────
  if (done && !expanded) {
    return (
      <button
        onClick={() => setExpanded(true)}
        className="flex items-center gap-2 my-2 px-3 py-2 rounded-lg bg-sidebar text-secondary hover:bg-black/[0.04] dark:hover:bg-white/[0.05] text-xs transition"
      >
        <BrainIcon className="w-3.5 h-3.5" />
        <span className="font-medium">{t("ReasoningPart.thinking")}</span>
        <span className="text-secondary/60">({t("ReasoningPart.chars", { count: text.length })})</span>
        <svg className="w-3 h-3 ml-auto" fill="none" stroke="currentColor" viewBox="0 0 24 24">
          <path strokeLinecap="round" strokeLinejoin="round" strokeWidth={2} d="M19 9l-7 7-7-7" />
        </svg>
      </button>
    );
  }

  // ── Typing or expanded done ─────────────────────────────────────
  const isTyping = displayedText.length < text.length && !done;

  return (
    <div className="my-2 rounded-lg border-l-2 border-primary-300 dark:border-primary-700 bg-sidebar overflow-hidden">
      {/* Header */}
      <button
        onClick={() => setExpanded(!expanded)}
        className="w-full flex items-center gap-2 px-3 py-2 text-[11px] font-medium text-secondary hover:bg-black/[0.03] dark:hover:bg-white/[0.04] transition"
      >
        <BrainIcon className="w-3.5 h-3.5" />
        <span>{t("ReasoningPart.thinking")}</span>
        {isTyping && (
          <span className="ml-auto inline-flex gap-0.5 items-center">
            <span className="w-1 h-1 rounded-full bg-primary-400 dark:bg-primary-500 animate-bounce [animation-delay:0ms]" />
            <span className="w-1 h-1 rounded-full bg-primary-400 dark:bg-primary-500 animate-bounce [animation-delay:120ms]" />
            <span className="w-1 h-1 rounded-full bg-primary-400 dark:bg-primary-500 animate-bounce [animation-delay:240ms]" />
          </span>
        )}
        {!isTyping && done && (
          <span className="ml-auto flex items-center gap-1 text-secondary/60">
            <svg className="w-3 h-3" fill="none" stroke="currentColor" viewBox="0 0 24 24">
              <path strokeLinecap="round" strokeLinejoin="round" strokeWidth={2} d="M5 13l4 4L19 7" />
            </svg>
            {t("ReasoningPart.done")}
          </span>
        )}
      </button>

      {/* Content */}
      {expanded && (
        <div className="px-3.5 py-2.5 text-[11px] text-secondary font-mono whitespace-pre-wrap leading-relaxed border-t border-subtle">
          {displayedText}
          {isTyping && (
            <span className="inline-block w-1.5 h-3.5 ml-0.5 align-text-bottom bg-primary-500 dark:bg-primary-400 animate-pulse rounded-sm" />
          )}
        </div>
      )}
    </div>
  );
}
