import { useEffect, useRef, useState } from "react";
import { useTranslation } from "react-i18next";
import { Plus, Check } from "lucide-react";
import type { SyscityWebSocketTransport } from "@/SyscityWebSocketTransport";
import { useChatStore, type ComposerChip } from "@/stores/chatStore";

interface EntityPickerProps {
  transport: SyscityWebSocketTransport;
}

interface AgentRow {
  id: string;
  display_name: string;
  emoji: string;
  is_valid: boolean;
}

interface SkillRow {
  name: string;
  description?: string;
}

interface ConnectorRow {
  id: string;
  display_name?: string;
  description?: string;
  state?: string;
}

function PickerRow({
  emoji,
  label,
  description,
  hint,
  stateDot,
  attached,
  onClick,
}: {
  emoji: string;
  label: string;
  description: string;
  hint?: string;
  stateDot?: React.ReactNode;
  attached: boolean;
  onClick: () => void;
}) {
  return (
    <button
      type="button"
      role="option"
      aria-selected={attached}
      onClick={onClick}
      className={`w-full flex items-center gap-2 px-3 py-2 text-left transition ${
        attached
          ? "bg-primary-50 dark:bg-primary-900/20"
          : "hover:bg-black/[0.03] dark:hover:bg-white/[0.04]"
      }`}
    >
      <span className="w-5 text-center shrink-0 text-sm">{emoji}</span>
      <span className="flex-1 min-w-0">
        <span className="flex items-center gap-1.5">
          <span className="text-sm text-primary truncate">{label}</span>
          {stateDot}
          {hint && (
            <span className="text-[10px] text-secondary/70 truncate shrink-0">
              {hint}
            </span>
          )}
        </span>
        {description && (
          <span className="block text-xs text-secondary truncate">
            {description}
          </span>
        )}
      </span>
      {attached && <Check className="w-4 h-4 text-primary shrink-0" />}
    </button>
  );
}

/** Composer "+" picker: attach expert / skill / connector chips to the next
 *  message. Chips are UI references only — consumed by the transport at send
 *  time (skills/agents become `mentions`; connectors are enabled pre-send).
 *  Clicking an attached row toggles it off; the panel stays open for
 *  multi-select and closes on Escape / outside pointerdown. */
export function EntityPicker({ transport }: EntityPickerProps) {
  const { t } = useTranslation("chat");
  const [open, setOpen] = useState(false);
  const [agents, setAgents] = useState<AgentRow[]>([]);
  const [skills, setSkills] = useState<SkillRow[]>([]);
  const [connectors, setConnectors] = useState<ConnectorRow[]>([]);
  const [agentsOk, setAgentsOk] = useState(true);
  const [skillsOk, setSkillsOk] = useState(true);
  const [connectorsOk, setConnectorsOk] = useState(true);
  const chips = useChatStore((s) => s.pendingChips);
  const rootRef = useRef<HTMLDivElement>(null);

  // Fetch all three lists in parallel each time the panel opens (lists are
  // small; freshness matters more than caching). A failed fetch hides its
  // whole section; an empty list shows a muted empty row instead.
  useEffect(() => {
    if (!open) return;
    let cancelled = false;
    transport
      .listAgentRegistry()
      .then((list) => {
        if (cancelled) return;
        setAgents(
          list
            .filter((a) => a.is_valid && a.id !== "default")
            .map((a) => ({
              id: a.id,
              display_name: a.display_name,
              emoji: a.emoji || "🤖",
              is_valid: a.is_valid,
            }))
        );
        setAgentsOk(true);
      })
      .catch(() => !cancelled && setAgentsOk(false));
    transport
      .listSkills()
      .then((res) => {
        if (cancelled) return;
        setSkills(
          (res.skills || []).map((s) => ({
            name: String(s.name ?? ""),
            description: typeof s.description === "string" ? s.description : undefined,
          }))
        );
        setSkillsOk(true);
      })
      .catch(() => !cancelled && setSkillsOk(false));
    transport
      .listConnectors()
      .then((list) => {
        if (cancelled) return;
        setConnectors(
          (list || []).map((c) => ({
            id: String(c.id ?? ""),
            display_name: typeof c.display_name === "string" ? c.display_name : undefined,
            description: typeof c.description === "string" ? c.description : undefined,
            state: typeof c.state === "string" ? c.state : undefined,
          }))
        );
        setConnectorsOk(true);
      })
      .catch(() => !cancelled && setConnectorsOk(false));
    return () => {
      cancelled = true;
    };
  }, [open, transport]);

  // Close on outside pointerdown or Escape.
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

  const isAttached = (kind: ComposerChip["kind"], id: string) =>
    chips.some((c) => c.kind === kind && c.id === id);
  const toggle = (chip: ComposerChip) => {
    useChatStore.getState().addComposerChip(chip);
  };

  const connectorStateDot = (c: ConnectorRow) => {
    if (c.state === "enabled") {
      return (
        <span
          className="w-2 h-2 rounded-full bg-green-500 shrink-0"
          title={t("EntityPicker.stateEnabled")}
        />
      );
    }
    if (c.state === "error") {
      return (
        <span
          className="w-2 h-2 rounded-full bg-red-500 shrink-0"
          title={t("EntityPicker.stateError")}
        />
      );
    }
    return (
      <span
        className="w-2 h-2 rounded-full bg-gray-400/60 dark:bg-gray-600 shrink-0"
        title={t("EntityPicker.connectOnSend")}
      />
    );
  };

  return (
    <div ref={rootRef} className="relative">
      <button
        type="button"
        onClick={() => setOpen((v) => !v)}
        // Don't take focus on mouse click so the composer's focus-within ring
        // stays on the text input while picking entities.
        onMouseDown={(e) => e.preventDefault()}
        aria-label={t("EntityPicker.attach")}
        aria-haspopup="listbox"
        aria-expanded={open}
        className="flex items-center rounded-lg p-2 text-secondary hover:text-primary hover:bg-black/[0.04] dark:hover:bg-white/[0.06] focus:outline-none transition"
      >
        <Plus className="w-4 h-4" />
      </button>

      {open && (
        <div
          role="listbox"
          className="absolute bottom-full left-0 mb-1.5 w-max min-w-72 max-w-[min(26rem,calc(100vw-4rem))] bg-card rounded-xl shadow-xl border border-subtle overflow-hidden z-50"
        >
          <div className="max-h-80 overflow-y-auto py-1">
            {/* Experts */}
            {agentsOk && (
              <>
                <div className="px-3 pt-2 pb-1 text-[10px] uppercase tracking-wide text-secondary/70">
                  {t("EntityPicker.experts")}
                </div>
                {agents.length === 0 ? (
                  <div className="px-3 py-1.5 text-xs text-secondary/70">
                    {t("EntityPicker.emptyExperts")}
                  </div>
                ) : (
                  agents.map((a) => (
                    <PickerRow
                      key={`agent-${a.id}`}
                      emoji={a.emoji || "🤖"}
                      label={a.display_name}
                      description=""
                      attached={isAttached("agent", a.id)}
                      onClick={() =>
                        toggle({
                          kind: "agent",
                          id: a.id,
                          label: a.display_name,
                          emoji: a.emoji || "🤖",
                        })
                      }
                    />
                  ))
                )}
              </>
            )}

            {/* Skills */}
            {skillsOk && (
              <>
                <div className="px-3 pt-2 pb-1 text-[10px] uppercase tracking-wide text-secondary/70">
                  {t("EntityPicker.skills")}
                </div>
                {skills.length === 0 ? (
                  <div className="px-3 py-1.5 text-xs text-secondary/70">
                    {t("EntityPicker.emptySkills")}
                  </div>
                ) : (
                  skills.map((s) => (
                    <PickerRow
                      key={`skill-${s.name}`}
                      emoji="🧩"
                      label={s.name}
                      description={s.description || ""}
                      attached={isAttached("skill", s.name)}
                      onClick={() =>
                        toggle({
                          kind: "skill",
                          id: s.name,
                          label: s.name,
                          emoji: "🧩",
                        })
                      }
                    />
                  ))
                )}
              </>
            )}

            {/* Connectors */}
            {connectorsOk && (
              <>
                <div className="px-3 pt-2 pb-1 text-[10px] uppercase tracking-wide text-secondary/70">
                  {t("EntityPicker.connectors")}
                </div>
                {connectors.length === 0 ? (
                  <div className="px-3 py-1.5 text-xs text-secondary/70">
                    {t("EntityPicker.emptyConnectors")}
                  </div>
                ) : (
                  connectors.map((c) => (
                    <PickerRow
                      key={`connector-${c.id}`}
                      emoji="🔌"
                      label={c.display_name || c.id}
                      description={c.description || ""}
                      hint={c.state !== "enabled" ? t("EntityPicker.connectOnSend") : undefined}
                      stateDot={connectorStateDot(c)}
                      attached={isAttached("connector", c.id)}
                      onClick={() =>
                        toggle({
                          kind: "connector",
                          id: c.id,
                          label: c.display_name || c.id,
                          emoji: "🔌",
                          state: c.state as ComposerChip["state"],
                        })
                      }
                    />
                  ))
                )}
              </>
            )}
          </div>
        </div>
      )}
    </div>
  );
}
