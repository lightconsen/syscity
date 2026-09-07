import { useState } from "react";
import {
  ChevronDown,
  ChevronUp,
  CheckCircle2,
  XCircle,
  Loader2,
  Crosshair,
} from "lucide-react";
import { useGoalStore, type GoalState } from "@/stores/goalStore";
import { useTranslation } from "react-i18next";

function isActive(status: GoalState["status"]) {
  return status === "running";
}

function goalStatusIcon(status: GoalState["status"]) {
  switch (status) {
    case "running":
      return <Loader2 className="w-4 h-4 animate-spin text-blue-500" />;
    case "done":
      return <CheckCircle2 className="w-4 h-4 text-green-500" />;
    case "aborted":
      return <XCircle className="w-4 h-4 text-red-500" />;
  }
}

function goalStatusKey(status: GoalState["status"]): string {
  switch (status) {
    case "running":
      return "GoalPanel.statusRunning";
    case "done":
      return "GoalPanel.statusDone";
    case "aborted":
      return "GoalPanel.statusAborted";
  }
}

function GoalCard({
  goal,
}: {
  goal: GoalState;
}) {
  const [expanded, setExpanded] = useState(false);
  const { t } = useTranslation("chat");

  return (
    <div className="rounded-lg bg-card overflow-hidden">
      {/* Header */}
      <button
        onClick={() => setExpanded((e) => !e)}
        className="w-full flex items-center gap-2 px-3 py-2 text-left hover:bg-black/[0.03] dark:hover:bg-white/[0.04] transition"
      >
        {goalStatusIcon(goal.status)}
        <span className="flex-1 text-sm font-medium text-primary truncate">
          {goal.description}
        </span>
        <span className="text-xs text-secondary shrink-0">
          {goal.passed}/{goal.total}
        </span>
        <span
          className={`text-xs px-1.5 py-0.5 rounded shrink-0 ${
            goal.status === "running"
              ? "bg-primary-50 dark:bg-primary-900/20 text-primary-600 dark:text-primary-400"
              : goal.status === "done"
              ? "bg-green-50 dark:bg-green-900/20 text-green-600 dark:text-green-400"
              : "bg-red-50 dark:bg-red-900/20 text-red-600 dark:text-red-400"
          }`}
        >
          {t(goalStatusKey(goal.status))}
        </span>
        {expanded ? (
          <ChevronUp className="w-4 h-4 text-secondary/60" />
        ) : (
          <ChevronDown className="w-4 h-4 text-secondary/60" />
        )}
      </button>

      {/* Expanded details */}
      {expanded && (
        <div className="px-3 pb-3 pt-1 border-t border-subtle">
          <div className="text-xs text-secondary mb-2">
            {t("GoalPanel.roundOf", { round: goal.round, maxRounds: goal.maxRounds })} &middot;{" "}
            {t("GoalPanel.idLabel")}{" "}
            <code className="text-[10px] bg-sidebar px-1 rounded">
              {goal.id}
            </code>
          </div>

          {goal.conditions.length > 0 && (
            <div className="space-y-1 mb-2">
              <div className="text-[11px] font-medium text-secondary/70 uppercase tracking-wider">
                {t("GoalPanel.conditions")}
              </div>
              {goal.conditions.map((c, i) => {
                const condPassed = goal.status !== "running" && goal.status === "done"
                  ? true
                  : i < goal.passed
                    ? true
                    : false;
                return (
                  <div
                    key={i}
                    className={`flex items-center gap-1.5 text-xs ${
                      condPassed
                        ? "text-green-600 dark:text-green-400"
                        : "text-secondary/70"
                    }`}
                  >
                    {condPassed ? (
                      <CheckCircle2 className="w-3 h-3 shrink-0" />
                    ) : (
                      <div className="w-3 h-3 shrink-0 rounded-full border border-subtle" />
                    )}
                    <span className="truncate">{c}</span>
                  </div>
                );
              })}
            </div>
          )}

          {goal.summary && (
            <div className="text-xs text-secondary mt-1">
              {goal.summary}
            </div>
          )}
          {goal.reason && goal.status === "aborted" && (
            <div className="text-xs text-red-500 dark:text-red-400 mt-1">
              {t("GoalPanel.reason", { reason: goal.reason })}
            </div>
          )}
        </div>
      )}
    </div>
  );
}

export function GoalPanel() {
  const goals = useGoalStore((s) => s.goals);
  const [collapsed, setCollapsed] = useState(false);
  const { t } = useTranslation("chat");

  const goalList = Object.values(goals);
  if (goalList.length === 0) return null;

  const activeCount = goalList.filter((g) => isActive(g.status)).length;

  return (
    <div className="shrink-0 border-t border-subtle bg-page">
      {/* Collapse toggle bar */}
      <button
        onClick={() => setCollapsed((c) => !c)}
        className="w-full flex items-center gap-2 px-4 py-1.5 text-xs text-secondary hover:bg-black/[0.03] dark:hover:bg-white/[0.04] transition"
      >
        <Crosshair className="w-3.5 h-3.5" />
        <span className="font-medium">
          {t("GoalPanel.goals")}
          {activeCount > 0 && (
            <span className="ml-1 text-primary-500">
              {t("GoalPanel.activeCount", { count: activeCount })}
            </span>
          )}
        </span>
        <span className="flex-1" />
        {collapsed ? (
          <ChevronUp className="w-3.5 h-3.5" />
        ) : (
          <ChevronDown className="w-3.5 h-3.5" />
        )}
      </button>

      {/* Goal list */}
      {!collapsed && (
        <div className="px-4 pb-3 space-y-2 max-h-[300px] overflow-y-auto">
          {goalList.map((goal) => (
            <GoalCard key={goal.id} goal={goal} />
          ))}
        </div>
      )}
    </div>
  );
}
