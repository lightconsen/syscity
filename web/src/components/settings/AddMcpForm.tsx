import { useState } from "react";
import { useTranslation } from "react-i18next";
import type { SyscityWebSocketTransport } from "@/SyscityWebSocketTransport";
import { Input } from "@/components/ui/Input";
import { Select } from "@/components/ui/Select";
import { Button } from "@/components/ui/Button";

interface AddMcpFormProps {
  transport: SyscityWebSocketTransport;
  onAdded?: () => void;
}

export function AddMcpForm({ transport, onAdded }: AddMcpFormProps) {
  const { t } = useTranslation("settings");
  const [addMcpError, setAddMcpError] = useState("");
  const [newMcp, setNewMcp] = useState({
    id: "",
    transport: "stdio",
    command: "",
    args: "",
    url: "",
    auth_type: "",
    client_id: "",
    auth_url: "",
    token_url: "",
    scopes: "",
    auto_connect: true,
  });
  const [mcpActionLoading, setMcpActionLoading] = useState<string>("");

  const handleAddMcp = async () => {
    setAddMcpError("");
    if (!newMcp.id.trim()) {
      setAddMcpError(t("AddMcpForm.errIdRequired"));
      return;
    }
    if (newMcp.transport === "stdio" && !newMcp.command.trim()) {
      setAddMcpError(t("AddMcpForm.errCommandRequired"));
      return;
    }
    if (newMcp.transport !== "stdio" && !newMcp.url.trim()) {
      setAddMcpError(t("AddMcpForm.errUrlRequired"));
      return;
    }
    setMcpActionLoading("add");
    const res = await transport.addMcpServer({
      id: newMcp.id.trim(),
      transport: newMcp.transport,
      command: newMcp.command.trim() || undefined,
      args: newMcp.args.split(",").map((s) => s.trim()).filter(Boolean),
      url: newMcp.url.trim() || undefined,
      auth_type: newMcp.auth_type || undefined,
      client_id: newMcp.client_id.trim() || undefined,
      auth_url: newMcp.auth_url.trim() || undefined,
      token_url: newMcp.token_url.trim() || undefined,
      scopes: newMcp.scopes.trim() || undefined,
      auto_connect: newMcp.auto_connect,
    });
    if (res.ok) {
      setNewMcp({ id: "", transport: "stdio", command: "", args: "", url: "", auth_type: "", client_id: "", auth_url: "", token_url: "", scopes: "", auto_connect: true });
      onAdded?.();
    } else {
      setAddMcpError(res.error || t("AddMcpForm.errAddFailed"));
    }
    setMcpActionLoading("");
  };

  return (
    <div className="mb-4 p-4 rounded-lg bg-card space-y-3">
      <div className="grid grid-cols-1 sm:grid-cols-2 gap-3">
        <Input
          label={t("AddMcpForm.serverId")}
          value={newMcp.id}
          onChange={(e) => setNewMcp({ ...newMcp, id: e.target.value })}
          placeholder="filesystem"
        />
        <div>
          <label className="block text-xs text-secondary mb-1">{t("AddMcpForm.transport")}</label>
          <Select value={newMcp.transport} onChange={(e) => setNewMcp({ ...newMcp, transport: e.target.value })}>
            <option value="stdio">stdio</option>
            <option value="sse">sse</option>
            <option value="streamable_http">streamable_http</option>
          </Select>
        </div>
      </div>
      {newMcp.transport === "stdio" ? (
        <>
          <Input
            label={t("AddMcpForm.command")}
            value={newMcp.command}
            onChange={(e) => setNewMcp({ ...newMcp, command: e.target.value })}
            placeholder="npx -y @modelcontextprotocol/server-filesystem"
          />
          <Input
            label={t("AddMcpForm.args")}
            value={newMcp.args}
            onChange={(e) => setNewMcp({ ...newMcp, args: e.target.value })}
            placeholder="/home/user/docs"
          />
        </>
      ) : (
        <>
          <Input
            label={t("AddMcpForm.url")}
            value={newMcp.url}
            onChange={(e) => setNewMcp({ ...newMcp, url: e.target.value })}
            placeholder="http://localhost:3000/sse"
          />
          <div>
            <label className="block text-xs text-secondary mb-1">{t("AddMcpForm.authType")}</label>
            <Select value={newMcp.auth_type} onChange={(e) => setNewMcp({ ...newMcp, auth_type: e.target.value })}>
              <option value="">none</option>
              <option value="oauth2">oauth2</option>
            </Select>
          </div>
          {newMcp.auth_type === "oauth2" && (
            <div className="grid grid-cols-1 sm:grid-cols-2 gap-3">
              <Input
                label={t("AddMcpForm.clientId")}
                value={newMcp.client_id}
                onChange={(e) => setNewMcp({ ...newMcp, client_id: e.target.value })}
                placeholder="your-client-id"
              />
              <Input
                label={t("AddMcpForm.scopes")}
                value={newMcp.scopes}
                onChange={(e) => setNewMcp({ ...newMcp, scopes: e.target.value })}
                placeholder="read write"
              />
              <Input
                label={t("AddMcpForm.authUrl")}
                value={newMcp.auth_url}
                onChange={(e) => setNewMcp({ ...newMcp, auth_url: e.target.value })}
                placeholder="http://localhost:9999/auth (optional, discoverable)"
              />
              <Input
                label={t("AddMcpForm.tokenUrl")}
                value={newMcp.token_url}
                onChange={(e) => setNewMcp({ ...newMcp, token_url: e.target.value })}
                placeholder="http://localhost:9999/token (optional, discoverable)"
              />
            </div>
          )}
        </>
      )}
      <div className="flex items-center gap-2">
        <input
          id="mcp-auto"
          type="checkbox"
          checked={newMcp.auto_connect}
          onChange={(e) => setNewMcp({ ...newMcp, auto_connect: e.target.checked })}
          className="rounded border-subtle text-primary-500 focus:ring-primary-500"
        />
        <label htmlFor="mcp-auto" className="text-sm text-secondary">{t("AddMcpForm.autoConnect")}</label>
      </div>
      {addMcpError && <div className="text-xs text-red-600 dark:text-red-400">{addMcpError}</div>}
      <div className="flex justify-end">
        <Button onClick={handleAddMcp} disabled={mcpActionLoading === "add"}>
          {mcpActionLoading === "add" ? t("AddMcpForm.adding") : t("AddMcpForm.addServer")}
        </Button>
      </div>
    </div>
  );
}
