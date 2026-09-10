import { useRef, useState, useCallback, useEffect } from "react";
import {
  ThreadPrimitive,
  ComposerPrimitive,
  useComposerRuntime,
} from "@assistant-ui/react";
import { useVirtualizer } from "@tanstack/react-virtual";
import { MessageBubble } from "./MessageBubble";
import { CommandPalette } from "./CommandPalette";
import { ModelSelector } from "./ModelSelector";
import { getCommandCompletions, type CommandDef } from "@/slash-commands";
import { useChatStore } from "@/stores/chatStore";
import { Mic, Paperclip, Square, Send, ChevronDown } from "lucide-react";
import { MessageSkeleton } from "@/components/ui/Skeleton";
import { useTranslation } from "react-i18next";
import { NewSessionWelcome } from "./NewSessionWelcome";
import { useSpeechRecognition } from "@/hooks/useSpeechRecognition";
import { useTextToSpeech } from "@/hooks/useTextToSpeech";
import { resolveSpeechLang, useSpeechStore } from "@/stores/speechStore";
import type { SyscityWebSocketTransport } from "@/SyscityWebSocketTransport";

interface ChatContentProps {
  transport: SyscityWebSocketTransport;
}

export function ChatContent({ transport }: ChatContentProps) {
  const { t } = useTranslation("chat");
  const inputRef = useRef<HTMLTextAreaElement>(null);
  const scrollRef = useRef<HTMLDivElement>(null);
  const composer = useComposerRuntime();
  const pendingDraft = useChatStore((s) => s.pendingDraft);
  const setPendingDraft = useChatStore((s) => s.setPendingDraft);

  /* ── One-shot composer prefill (expert starter prompt / skill "try it") ── */
  useEffect(() => {
    if (pendingDraft === null) return;
    composer.setText(pendingDraft);
    setPendingDraft(null);
    setTimeout(() => inputRef.current?.focus(), 0);
  }, [pendingDraft, composer, setPendingDraft]);

  const [paletteOpen, setPaletteOpen] = useState(false);
  const [paletteIndex, setPaletteIndex] = useState(0);
  const [paletteCommands, setPaletteCommands] = useState<CommandDef[]>([]);

  const messages = useChatStore((s) => s.messages);
  const isRunning = useChatStore((s) => s.isRunning);
  const voiceMode = useChatStore((s) => s.voiceMode);
  const setVoiceMode = useChatStore((s) => s.setVoiceMode);
  const speechLang = resolveSpeechLang(useSpeechStore((s) => s.lang));
  const isLoadingHistory = useChatStore((s) => s.isLoadingHistory);
  const hasMoreHistory = useChatStore((s) => s.hasMoreHistory);
  const setIsLoadingHistory = useChatStore((s) => s.setIsLoadingHistory);
  const setHasMoreHistory = useChatStore((s) => s.setHasMoreHistory);
  const prependMessages = useChatStore((s) => s.prependMessages);

  /* ── TTS: auto-read AI replies in voice mode ── */
  const {
    isSpeaking,
    speak,
    stop: stopSpeaking,
    supported: ttsSupported,
  } = useTextToSpeech({
    lang: speechLang,
    onEnd: () => {
      // After speaking, re-start listening if still in voice mode
      if (useChatStore.getState().voiceMode) {
        setTimeout(() => startListening(), 400);
      }
    },
  });

  /* ── STT: voice input with auto-submit ── */
  const handleVoiceResult = useCallback(
    (text: string) => {
      const current = inputRef.current?.value || "";
      const next = current ? `${current} ${text}` : text;
      composer.setText(next);
      setTimeout(() => inputRef.current?.focus(), 0);
    },
    [composer]
  );

  const {
    isListening,
    start: startListening,
    stop: stopListening,
    supported: sttSupported,
  } = useSpeechRecognition({
    onResult: handleVoiceResult,
    onError: (err) => console.warn("Speech recognition error:", err),
    onSubmit: () => {
      // Auto-send when voice input completes (only in voice mode)
      if (useChatStore.getState().voiceMode) {
        composer.send?.();
      }
    },
    autoSubmit: true,
    lang: speechLang,
  });

  const voiceSupported = sttSupported && ttsSupported;

  /* ── Voice mode toggle ── */
  const toggleVoiceMode = useCallback(() => {
    const next = !voiceMode;
    setVoiceMode(next);
    if (next) {
      startListening();
    } else {
      stopListening();
      stopSpeaking();
    }
  }, [voiceMode, setVoiceMode, startListening, stopListening, stopSpeaking]);

  /* ── Auto-read when AI reply completes ── */
  const prevIsRunningRef = useRef(isRunning);
  useEffect(() => {
    const wasRunning = prevIsRunningRef.current;
    prevIsRunningRef.current = isRunning;

    if (wasRunning && !isRunning && voiceMode) {
      const msgs = useChatStore.getState().messages;
      const last = msgs[msgs.length - 1];
      if (last?.role === "assistant" && last.content) {
        speak(last.content);
      }
    }
  }, [isRunning, voiceMode, speak]);

  // Default new assistant messages to show their thinking/tools.
  const lastAutoShownAssistantIdRef = useRef<string | null>(null);
  useEffect(() => {
    const last = messages[messages.length - 1];
    if (
      last?.role === "assistant" &&
      last.id !== lastAutoShownAssistantIdRef.current
    ) {
      const hasInternals = last.parts?.some(
        (p) => p.type === "reasoning" || p.type === "tool-call"
      );
      if (hasInternals) {
        useChatStore.getState().setAiInternalsVisibility(last.id, true);
        lastAutoShownAssistantIdRef.current = last.id;
      }
    }
  }, [messages]);

  // While > 0 (a timestamp deadline), a programmatic bottom navigation is
  // still settling: re-align on every virtualizer change so late item
  // measurements can't leave the view short of the true bottom.
  const stickUntilRef = useRef(0);

  const virtualizer = useVirtualizer({
    count: messages.length,
    getScrollElement: () => scrollRef.current,
    estimateSize: () => 120,
    measureElement: (el) => el.getBoundingClientRect().height,
    overscan: 5,
    onChange: (instance) => {
      if (
        stickUntilRef.current > 0 &&
        performance.now() < stickUntilRef.current &&
        instance.options.count > 0
      ) {
        instance.scrollToIndex(instance.options.count - 1, { align: "end" });
      }
    },
  });

  // Sync run state into store
  useEffect(() => {
    return transport.onRunStateChange((running) => {
      useChatStore.getState().setIsRunning(running);
    });
  }, [transport]);

  const scrollToBottom = useCallback(() => {
    if (messages.length === 0) return;
    // Arm the settle window: late item measurements right after the jump are
    // re-aligned by the virtualizer's onChange loop (stickUntilRef above).
    stickUntilRef.current = performance.now() + 500;
    virtualizer.scrollToIndex(messages.length - 1, { align: "end" });
  }, [messages.length, virtualizer]);

  // Auto-scroll to bottom when new messages arrive (but not when prepending
  // history — a prepend leaves the last message unchanged).
  const prevMessagesLengthRef = useRef(messages.length);
  const prevLastIdRef = useRef(messages[messages.length - 1]?.id ?? null);
  useEffect(() => {
    const prevLength = prevMessagesLengthRef.current;
    const prevLastId = prevLastIdRef.current;
    prevMessagesLengthRef.current = messages.length;
    const lastId = messages[messages.length - 1]?.id ?? null;
    prevLastIdRef.current = lastId;
    if (messages.length > prevLength && lastId !== prevLastId) {
      requestAnimationFrame(() => scrollToBottom());
    }
  }, [messages.length, messages, scrollToBottom]);

  const [showScrollButton, setShowScrollButton] = useState(false);

  useEffect(() => {
    const el = scrollRef.current;
    if (!el) return;

    const checkScroll = () => {
      const threshold = 100;
      const atBottom = el.scrollHeight - el.scrollTop - el.clientHeight < threshold;
      setShowScrollButton(!atBottom);
    };

    el.addEventListener("scroll", checkScroll);
    checkScroll();
    return () => el.removeEventListener("scroll", checkScroll);
  }, [messages]);

  const isLoadingHistoryRef = useRef(isLoadingHistory);
  useEffect(() => {
    isLoadingHistoryRef.current = isLoadingHistory;
  }, [isLoadingHistory]);

  const hasMoreHistoryRef = useRef(hasMoreHistory);
  useEffect(() => {
    hasMoreHistoryRef.current = hasMoreHistory;
  }, [hasMoreHistory]);

  useEffect(() => {
    const el = scrollRef.current;
    if (!el) return;

    const handleScroll = () => {
      if (isLoadingHistoryRef.current || !hasMoreHistoryRef.current) return;
      if (el.scrollTop > 80) return;

      const first = messages[0];
      if (!first?.timestamp) return;

      setIsLoadingHistory(true);
      transport
        .loadMoreHistory(transport.getSessionId(), first.timestamp)
        .then(({ messages: older, hasMore }) => {
          if (older.length > 0) {
            prependMessages(older);
          }
          setHasMoreHistory(hasMore);
        })
        .catch(() => {
          setHasMoreHistory(false);
        })
        .finally(() => {
          setIsLoadingHistory(false);
        });
    };

    el.addEventListener("scroll", handleScroll);
    return () => el.removeEventListener("scroll", handleScroll);
  }, [messages, prependMessages, setHasMoreHistory, setIsLoadingHistory, transport]);

  const handleInput = useCallback(() => {
    const val = inputRef.current?.value || "";
    if (val.startsWith("/")) {
      const filter = val.slice(1).split(" ")[0] || "";
      const cmds = getCommandCompletions(filter);
      setPaletteCommands(cmds);
      setPaletteOpen(cmds.length > 0);
      setPaletteIndex(0);
    } else {
      setPaletteOpen(false);
    }
  }, []);

  const handleKeyDown = useCallback(
    (e: React.KeyboardEvent<HTMLTextAreaElement>) => {
      if (!paletteOpen) return;
      if (e.key === "ArrowDown") {
        e.preventDefault();
        setPaletteIndex((i) => Math.min(i + 1, paletteCommands.length - 1));
      } else if (e.key === "ArrowUp") {
        e.preventDefault();
        setPaletteIndex((i) => Math.max(i - 1, 0));
      } else if (e.key === "Enter" || e.key === "Tab") {
        e.preventDefault();
        const cmd = paletteCommands[paletteIndex];
        if (cmd) {
          composer.setText(`/${cmd.name} `);
          setPaletteOpen(false);
          setTimeout(() => inputRef.current?.focus(), 0);
        }
      } else if (e.key === "Escape") {
        setPaletteOpen(false);
      }
    },
    [paletteOpen, paletteCommands, paletteIndex, composer]
  );

  const handleSelectCommand = useCallback(
    (cmd: CommandDef) => {
      composer.setText(`/${cmd.name} `);
      setPaletteOpen(false);
      setTimeout(() => inputRef.current?.focus(), 0);
    },
    [composer]
  );

  const virtualItems = virtualizer.getVirtualItems();
  const totalHeight = virtualizer.getTotalSize();

  /* ── Mic button helpers ── */
  const micTitle = !voiceSupported
    ? t("ChatContent.voiceNotSupported")
    : voiceMode
      ? isListening
        ? t("ChatContent.stopListening")
        : isSpeaking
          ? t("ChatContent.speaking")
          : t("ChatContent.voiceModeOn")
      : t("ChatContent.voiceMode");

  const micClass = () => {
    if (!voiceSupported) {
      return "text-gray-300 dark:text-neutral-600 cursor-not-allowed";
    }
    if (voiceMode) {
      if (isListening) {
        return "text-red-500 bg-red-50 dark:bg-red-900/20 animate-pulse";
      }
      if (isSpeaking) {
        return "text-blue-500 bg-blue-50 dark:bg-blue-900/20";
      }
      return "text-orange-500 bg-orange-50 dark:bg-orange-900/20";
    }
    return "text-secondary hover:text-primary-600 dark:hover:text-primary-400 hover:bg-black/5 dark:hover:bg-white/5";
  };

  const isEmpty = messages.length === 0;

  // The composer is the chat area's own component; on the New Session welcome
  // page it renders centered below the greeting (same instance, one branch).
  const composerEl = (
    <ComposerPrimitive.Root className="max-w-[var(--message-list-max-width)] mx-auto w-full">
      <div className="relative flex flex-col rounded-2xl bg-card shadow-sm focus-within:ring-2 focus-within:ring-primary-500/20 transition">
        {/* Command palette */}
        {paletteOpen && (
          <CommandPalette
            commands={paletteCommands}
            selectedIndex={paletteIndex}
            onSelect={handleSelectCommand}
          />
        )}

        {/* Multiline input */}
        <ComposerPrimitive.Input
          ref={inputRef}
          onInput={handleInput}
          onKeyDown={handleKeyDown}
          className="w-full resize-none bg-transparent px-4 pt-3 pb-1 text-sm text-primary placeholder:text-secondary/60 focus:outline-none min-h-[60px] max-h-[200px]"
          placeholder={t("ChatContent.placeholder")}
          rows={1}
          aria-label={t("ChatContent.messageInput")}
        />

        {/* Bottom toolbar */}
        <div className="flex items-center justify-between px-2 pb-2 pt-1">
          <div className="flex items-center gap-1">
            <button
              type="button"
              title={micTitle}
              aria-label={micTitle}
              className={`p-2 rounded-lg transition ${micClass()}`}
              onClick={toggleVoiceMode}
              disabled={!voiceSupported}
            >
              <Mic className="w-5 h-5" />
            </button>
            <button
              type="button"
              title={t("ChatContent.attachFile")}
              aria-label={t("ChatContent.attachFile")}
              className="p-2 rounded-lg text-secondary hover:text-primary-600 dark:hover:text-primary-400 hover:bg-black/5 dark:hover:bg-white/5 transition"
              onClick={() => alert(t("ChatContent.attachmentsComingSoon"))}
            >
              <Paperclip className="w-5 h-5" />
            </button>
          </div>
          <div className="flex items-center gap-1">
            <ModelSelector transport={transport} />
            {isRunning ? (
              <button
                type="button"
                onClick={() => transport.abort(transport.getSessionId())}
                title={t("ChatContent.stopGenerating")}
                aria-label={t("ChatContent.stopGenerating")}
                className="shrink-0 p-2 rounded-lg bg-red-500 hover:bg-red-600 text-white transition shadow-sm"
              >
                <Square className="w-4 h-4 fill-current" />
              </button>
            ) : (
              <ComposerPrimitive.Send className="shrink-0 p-2 rounded-lg bg-gradient-to-r from-primary-500 to-primary-700 hover:from-primary-600 hover:to-primary-800 disabled:opacity-40 text-white transition shadow-sm">
                <Send className="w-4 h-4" />
              </ComposerPrimitive.Send>
            )}
          </div>
        </div>
      </div>
    </ComposerPrimitive.Root>
  );

  return (
    <ThreadPrimitive.Root className="flex-1 flex flex-col overflow-hidden relative">
      {isEmpty ? (
        /* New Session welcome page: greeting centered, composer right below.
           The first message sent here lazily creates the real session. */
        <div className="flex-1 flex flex-col items-center justify-center px-4 overflow-y-auto">
          <NewSessionWelcome />
          <div className="w-full mt-6">{composerEl}</div>
        </div>
      ) : (
        <>
          {/* Scrollable message area */}
          <div
            ref={scrollRef}
            className="flex-1 overflow-y-auto relative"
            role="log"
            aria-live="polite"
          >
            {messages.length > 0 && (
              <div style={{ height: `${totalHeight}px`, position: "relative" }}>
                {isLoadingHistory && (
                  <div className="py-3 text-center text-secondary text-sm">
                    {t("ChatContent.loadingOlder")}
                  </div>
                )}
                {virtualItems.map((virtualItem) => (
                  <div
                    key={virtualItem.key}
                    ref={virtualizer.measureElement}
                    data-index={virtualItem.index}
                    style={{
                      position: "absolute",
                      top: 0,
                      left: 0,
                      width: "100%",
                      transform: `translateY(${virtualItem.start}px)`,
                    }}
                  >
                    <MessageBubble
                      message={messages[virtualItem.index]}
                      transport={transport}
                      onEdit={(id, text) => transport.editUserMessage(id, text)}
                    />
                  </div>
                ))}
              </div>
            )}
            {isRunning && <MessageSkeleton />}
          </div>

          {showScrollButton && (
            <button
              type="button"
              onClick={scrollToBottom}
              aria-label={t("ChatContent.scrollToBottom")}
              className="absolute bottom-28 left-1/2 -translate-x-1/2 p-2 rounded-full bg-primary-500 text-white shadow-lg hover:bg-primary-600 transition-opacity animate-bounce z-10"
            >
              <ChevronDown className="w-5 h-5" />
            </button>
          )}

          <div className="bg-page px-4 py-3 shrink-0 relative">{composerEl}</div>
        </>
      )}
    </ThreadPrimitive.Root>
  );
}
