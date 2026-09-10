import { useEffect, useRef, useState } from "react";
import { useTranslation } from "react-i18next";
import { Plus, Check, Search } from "lucide-react";
import type { SyscityWebSocketTransport } from "@/SyscityWebSocketTransport";
import { useChatStore, type ComposerChip, type ChipKind } from "@/stores/chatStore";

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

type TabId = Extract<ChipKind, "agent" | "skill" | "connector">;

/** Composer "+" picker: attach skill / connector chips (and, on the default
 *  agent, expert chips) to the next message. Chips are UI references only —
 *  consumed by the transport at send time (skills/agents become `mentions`;
 *  connectors are enabled pre-send). Sessions already bound to a specific
 *  agent hide the Experts tab: that agent answers directly, and delegating
 *  to another expert from inside its session is not offered.
 *  Tabbed layout: Experts is single-select with a search box (attaching one
 *  expert replaces the previous); Skills/Connectors are multi-select.
 *  Clicking an attached row toggles it off; the panel closes on Escape /
 *  outside pointerdown. */
export function EntityPicker({ transport }: EntityPickerProps) {
  const { t } = useTranslation("chat");
  const boundAgentId = useChatStore((s) => s.currentAgent?.id);
  const [open, setOpen] = useState(false);
  const [tab, setTab] = useState<TabId>("agent");
  const [query, setQuery] = useState("");
  const [agents, setAgents] = useState<AgentRow[]>([]);
  const [skills, setSkills] = useState<SkillRow[]>([]);
  const [connectors, setConnectors] = useState<ConnectorRow[]>([]);
  const [agentsOk, setAgentsOk] = useState(true);
  const [skillsOk, setSkillsOk] = useState(true);
  const [connectorsOk, setConnectorsOk] = useState(true);
  const chips = useChatStore((s) => s.pendingChips);
  const rootRef = useRef<HTMLDivElement>(null);
  const searchRef = useRef<HTMLInputElement>(null);

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

  // Close on outside pointerdown or Escape. Reopen resets to the Experts tab
  // with a cleared search.
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

  useEffect(() => {
    if (!open) {
      setQuery("");
    }
  }, [open]);

  // Sessions bound to a specific agent don't offer expert delegation.
  const expertsEnabled = !boundAgentId;

  // Keep the active tab valid when the Experts tab disappears (session with
  // a bound agent) — fall back to Skills.
  useEffect(() => {
    if (tab === "agent" && !expertsEnabled) setTab("skill");
  }, [tab, expertsEnabled]);

  const isAttached = (kind: ChipKind, id: string) =>
    chips.some((c) => c.kind === kind && c.id === id);

  const toggleMulti = (chip: ComposerChip) => {
    useChatStore.getState().addComposerChip(chip);
  };
  // Experts are single-select: attaching one replaces any previously attached.
  const selectExpert = (chip: ComposerChip) => {
    const store = useChatStore.getState();
    const already = chips.some((c) => c.kind === "agent" && c.id === chip.id);
    store.setPendingChips(
      already
        ? chips.filter((c) => c.kind !== "agent")
        : [...chips.filter((c) => c.kind !== "agent"), chip]
    );
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
          className="absolute bottom-full left-0 mb-1.5 w-80 max-w-[min(26rem,calc(100vw-4rem))] bg-card rounded-xl shadow-xl border border-subtle overflow-hidden z-50"
        >
          {/* Tab bar */}
          <div className="flex border-b border-subtle">
            {(
              (
                [
                  ["agent", t("EntityPicker.experts")],
                  ["skill", t("EntityPicker.skills")],
                  ["connector", t("EntityPicker.connectors")],
                ] as Array<[TabId, string]>
              ).filter(([id]) => id !== "agent" || expertsEnabled)
            ).map(([id, label]) => (
              <button
                key={id}
                type="button"
                role="tab"
                aria-selected={tab === id}
                onClick={() => setTab(id)}
                className={`flex-1 px-3 py-2 text-xs font-medium transition ${
                  tab === id
                    ? "text-primary border-b-2 border-primary-500"
                    : "text-secondary hover:text-primary"
                }`}
              >
                {id === "agent" && chips.some((c) => c.kind === "agent") && (
                  <Check className="inline w-3 h-3 mr-1 text-primary" />
                )}
                {id === "skill" && chips.some((c) => c.kind === "skill") && (
                  <Check className="inline w-3 h-3 mr-1 text-primary" />
                )}
                {id === "connector" && chips.some((c) => c.kind === "connector") && (
                  <Check className="inline w-3 h-3 mr-1 text-primary" />
                )}
                {label}
              </button>
            ))}
          </div>

          {/* Search box (Experts tab only) */}
          {tab === "agent" && (
            <div className="px-3 pt-2">
              <div className="flex items-center gap-1.5 rounded-lg bg-black/[0.04] dark:bg-white/[0.06] px-2 py-1.5">
                <Search className="w-3.5 h-3.5 text-secondary/70 shrink-0" />
                <input
                  ref={searchRef}
                  type="text"
                  value={query}
                  onChange={(e) => setQuery(e.target.value)}
                  onKeyDown={(e) => {
                    // Don't let the composer's keydown handlers steal Escape.
                    if (e.key === "Escape") {
                      e.stopPropagation();
                      setQuery("");
                    }
                    e.stopPropagation();
                  }}
                  placeholder={t("EntityPicker.searchExperts")}
                  className="flex-1 min-w-0 bg-transparent text-xs text-primary placeholder:text-secondary/60 focus:outline-none"
                />
              </div>
            </div>
          )}

          <div className="max-h-72 overflow-y-auto py-1">
            {/* Experts: single-select + search filter (default-agent sessions only) */}
            {tab === "agent" && expertsEnabled && (
              <>
                {agentsOk && agents.length === 0 && (
                  <div className="px-3 py-1.5 text-xs text-secondary/70">
                    {t("EntityPicker.emptyExperts")}
                  </div>
                )}
                {!agentsOk && (
                  <div className="px-3 py-1.5 text-xs text-secondary/70">
                    {t("EntityPicker.emptyExperts")}
                  </div>
                )}
                {agentsOk &&
                  agents
                    .filter(
                      (a) =>
                        query.trim() === "" ||
                        a.display_name.toLowerCase().includes(query.trim().toLowerCase()) ||
                        a.id.toLowerCase().includes(query.trim().toLowerCase())
                    )
                    .map((a) => (
                      <PickerRow
                        key={`agent-${a.id}`}
                        emoji={a.emoji || "🤖"}
                        label={a.display_name}
                        description=""
                        attached={isAttached("agent", a.id)}
                        onClick={() =>
                          selectExpert({
                            kind: "agent",
                            id: a.id,
                            label: a.display_name,
                            emoji: a.emoji || "🤖",
                          })
                        }
                      />
                    ))}
                {agentsOk &&
                  agents.length > 0 &&
                  agents.every(
                    (a) =>
                      query.trim() !== "" &&
                      !a.display_name.toLowerCase().includes(query.trim().toLowerCase()) &&
                      !a.id.toLowerCase().includes(query.trim().toLowerCase())
                  ) && (
                    <div className="px-3 py-1.5 text-xs text-secondary/70">
                      {t("EntityPicker.noMatches")}
                    </div>
                  )}
              </>
            )}

            {/* Skills: multi-select */}
            {tab === "skill" && (
              <>
                {(!skillsOk || skills.length === 0) && (
                  <div className="px-3 py-1.5 text-xs text-secondary/70">
                    {t("EntityPicker.emptySkills")}
                  </div>
                )}
                {skillsOk &&
                  skills.map((s) => (
                    <PickerRow
                      key={`skill-${s.name}`}
                      emoji="🧩"
                      label={s.name}
                      description={s.description || ""}
                      attached={isAttached("skill", s.name)}
                      onClick={() =>
                        toggleMulti({
                          kind: "skill",
                          id: s.name,
                          label: s.name,
                          emoji: "🧩",
                        })
                      }
                    />
                  ))}
              </>
            )}

            {/* Connectors: multi-select */}
            {tab === "connector" && (
              <>
                {(!connectorsOk || connectors.length === 0) && (
                  <div className="px-3 py-1.5 text-xs text-secondary/70">
                    {t("EntityPicker.emptyConnectors")}
                  </div>
                )}
                {connectorsOk &&
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
                        toggleMulti({
                          kind: "connector",
                          id: c.id,
                          label: c.display_name || c.id,
                          emoji: "🔌",
                          state: c.state as ComposerChip["state"],
                        })
                      }
                    />
                  ))}
              </>
            )}
          </div>
        </div>
      )}
    </div>
  );
}
