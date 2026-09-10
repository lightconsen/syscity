import { useEffect, useRef, useState } from "react";
import { useTranslation } from "react-i18next";
import { Check, ChevronDown } from "lucide-react";
import type { ModelInfo, SyscityWebSocketTransport } from "@/SyscityWebSocketTransport";
import { modelLogoKey } from "@/lib/providerLogos";
import { ProviderLogo } from "@/components/ui/ProviderLogo";

interface ModelSelectorProps {
  transport: SyscityWebSocketTransport;
}

// One model row in the dropdown: vendor logo + bare model name. Cloud models
// (proxy provider) resolve their logo from the model id via modelLogoKey.
function ModelRow({
  m,
  selected,
  onSelect,
}: {
  m: ModelInfo;
  selected: boolean;
  onSelect: () => void;
}) {
  const { t } = useTranslation("chat");
  return (
    <button
      type="button"
      role="option"
      aria-selected={selected}
      onClick={onSelect}
      className={`w-full flex items-center gap-2 px-3 py-2 text-left transition ${
        selected
          ? "bg-primary-50 dark:bg-primary-900/20"
          : "hover:bg-black/[0.03] dark:hover:bg-white/[0.04]"
      }`}
    >
      <ProviderLogo
        provider={modelLogoKey(m.provider, m.name)}
        name={m.provider_name}
        className="w-4 h-4"
      />
      <span className="flex-1 min-w-0 text-sm text-primary">{m.name}</span>
      {m.provider === "cloud" && (
        <span className="text-[10px] text-secondary/70 shrink-0" title={t("ModelSelector.creditMultiplier")}>
          {m.credit_multiplier ?? 1}x
        </span>
      )}
      {selected && <Check className="w-4 h-4 text-primary shrink-0" />}
    </button>
  );
}

// Compact model picker for the chat composer. Shows the effective model for
// the active session (explicit session pin -> bound agent's model -> global
// default) and persists a session-level pin via sessions.set_model on change.
export function ModelSelector({ transport }: ModelSelectorProps) {
  const { t } = useTranslation("chat");
  const [sessionId, setSessionId] = useState(() => transport.getSessionId());
  const [models, setModels] = useState<ModelInfo[]>([]);
  const [defaultModel, setDefaultModel] = useState("");
  const [agentModels, setAgentModels] = useState<Record<string, string>>({});
  const [sessionModel, setSessionModel] = useState<string | null>(null);
  const [sessionAgentId, setSessionAgentId] = useState("");
  const [open, setOpen] = useState(false);
  const rootRef = useRef<HTMLDivElement>(null);

  // Track the active session.
  useEffect(
    () => transport.onSessionChange(() => setSessionId(transport.getSessionId())),
    [transport]
  );

  // Load the model list and per-agent bindings once.
  useEffect(() => {
    let cancelled = false;
    transport
      .listModels()
      .then((r) => {
        if (cancelled) return;
        setModels(r.models);
        setDefaultModel(r.default_model);
      })
      .catch(() => {});
    transport
      .getConfig()
      .then((c) => {
        if (cancelled) return;
        setAgentModels((c.agent_models as Record<string, string>) || {});
      })
      .catch(() => {});
    return () => {
      cancelled = true;
    };
  }, [transport]);

  // Load the current session's pin + bound agent whenever the session changes.
  useEffect(() => {
    let cancelled = false;
    transport
      .listSessions()
      .then((list) => {
        if (cancelled) return;
        const s = list.find((x) => x.id === sessionId);
        setSessionModel(s?.model ?? null);
        setSessionAgentId(s?.agent_id ?? "");
      })
      .catch(() => {});
    return () => {
      cancelled = true;
    };
  }, [transport, sessionId]);

  // React to model changes for this session (from this or other clients).
  useEffect(
    () =>
      transport.onEvent((evt) => {
        if (evt.event !== "session.model_changed") return;
        const p = evt.payload as { session_id?: string; model?: string | null } | undefined;
        if (p?.session_id === sessionId) {
          setSessionModel(p.model ?? null);
        }
      }),
    [transport, sessionId]
  );

  // Close the dropdown on outside click or Escape.
  useEffect(() => {
    if (!open) return;
    const onPointerDown = (e: PointerEvent) => {
      if (rootRef.current && !rootRef.current.contains(e.target as Node)) {
        setOpen(false);
      }
    };
    const onKeyDown = (e: KeyboardEvent) => {
      if (e.key === "Escape") setOpen(false);
    };
    document.addEventListener("pointerdown", onPointerDown);
    document.addEventListener("keydown", onKeyDown);
    return () => {
      document.removeEventListener("pointerdown", onPointerDown);
      document.removeEventListener("keydown", onKeyDown);
    };
  }, [open]);

  const effective = sessionModel ?? agentModels[sessionAgentId] ?? defaultModel;
  // What the model would be if the session pin were cleared (agent binding ->
  // global default). Shown on the "clear pin" option so it labels what you get
  // by switching back, not the currently pinned model.
  const fallback = agentModels[sessionAgentId] ?? defaultModel;
  const effectiveModel = models.find((m) => m.id === effective) ?? null;

  // Option list: cloud (proxied) models first — flat, vendor logo per model,
  // no section title — then a dashed divider, then local models grouped by
  // provider.
  const cloudModels = models.filter((m) => m.provider === "cloud");
  const localByProvider = new Map<string, ModelInfo[]>();
  for (const m of models) {
    if (m.provider === "cloud") continue;
    const list = localByProvider.get(m.provider) || [];
    list.push(m);
    localByProvider.set(m.provider, list);
  }

  const handleChange = (value: string) => {
    const m = value === "" ? null : value;
    setSessionModel(m); // optimistic; the model_changed event reconciles
    transport.setSessionModel(sessionId, m).catch(() => {});
    setOpen(false);
  };

  if (models.length === 0) return null;

  return (
    <div ref={rootRef} className="relative self-center">
      <button
        type="button"
        onClick={() => setOpen((v) => !v)}
        // Don't take focus on mouse click so the composer's focus-within ring
        // (and the button's own ring) never highlight when picking a model.
        onMouseDown={(e) => e.preventDefault()}
        title={`${t("ModelSelector.modelTitle", {
          model: effective || t("ModelSelector.default"),
        })}${sessionModel ? "" : t("ModelSelector.defaultSuffix")}`}
        aria-label={t("ModelSelector.selectModel")}
        aria-haspopup="listbox"
        aria-expanded={open}
        className="flex items-center gap-1.5 max-w-[10rem] rounded-lg px-2 py-1.5 text-xs text-secondary hover:text-primary hover:bg-black/[0.04] dark:hover:bg-white/[0.06] focus:outline-none transition"
      >
        {effectiveModel ? (
          <ProviderLogo
            provider={modelLogoKey(effectiveModel.provider, effectiveModel.name)}
            name={effectiveModel.provider_name}
            className="w-4 h-4"
          />
        ) : (
          <span className="w-4 h-4 shrink-0 rounded bg-sidebar flex items-center justify-center text-[9px] font-semibold text-secondary">
            {(effective || "D").charAt(0).toUpperCase()}
          </span>
        )}
        <span className="truncate">
          {effectiveModel
            ? effectiveModel.name
            : effective || t("ModelSelector.defaultModel")}
        </span>
        <ChevronDown
          className={`w-3 h-3 shrink-0 text-secondary/60 transition-transform ${open ? "rotate-180" : ""}`}
        />
      </button>

      {open && (
        <div
          role="listbox"
          className="absolute bottom-full left-0 mb-1.5 w-max min-w-64 max-w-[min(26rem,calc(100vw-4rem))] bg-card rounded-xl shadow-xl border border-subtle overflow-hidden z-50"
        >
          <div className="max-h-72 overflow-y-auto py-1">
            {/* Clear-pin option */}
            <button
              type="button"
              role="option"
              aria-selected={sessionModel === null}
              onClick={() => handleChange("")}
              className={`w-full flex items-center gap-2 px-3 py-2 text-left transition ${
                sessionModel === null
                  ? "bg-primary-50 dark:bg-primary-900/20"
                  : "hover:bg-black/[0.03] dark:hover:bg-white/[0.04]"
              }`}
            >
              <span className="w-4 h-4 shrink-0" />
              <span className="flex-1 min-w-0">
                <span className="block text-sm text-primary">
                  {t("ModelSelector.defaultModel")}
                </span>
                {fallback && (
                  <span className="block text-xs text-secondary truncate">
                    {t("ModelSelector.uses", { model: fallback })}
                  </span>
                )}
              </span>
              {sessionModel === null && <Check className="w-4 h-4 text-primary shrink-0" />}
            </button>

            {/* Cloud (proxied) models: vendor logo + model name, no title */}
            {cloudModels.map((m) => (
              <ModelRow
                key={m.id}
                m={m}
                selected={sessionModel === m.id}
                onSelect={() => handleChange(m.id)}
              />
            ))}

            {/* Labeled divider marking the start of the local-model section */}
            {cloudModels.length > 0 && (
              <div className="my-1 flex items-center gap-2 px-3">
                <span className="flex-1 border-t border-dashed border-subtle" />
                <span className="text-[10px] text-secondary/70">
                  {t("ModelSelector.localModels")}
                </span>
                <span className="flex-1 border-t border-dashed border-subtle" />
              </div>
            )}

            {Array.from(localByProvider.entries()).map(([provider, ms]) => (
              <div key={provider}>
                <div className="px-3 pt-2 pb-1 text-[10px] uppercase tracking-wide text-secondary/70">
                  {ms[0]?.provider_name || provider}
                </div>
                {ms.map((m) => (
                  <ModelRow
                    key={m.id}
                    m={m}
                    selected={sessionModel === m.id}
                    onSelect={() => handleChange(m.id)}
                  />
                ))}
              </div>
            ))}
          </div>
        </div>
      )}
    </div>
  );
}
