import { useState } from "react";
import { useTranslation } from "react-i18next";
import type { SyscityWebSocketTransport } from "@/SyscityWebSocketTransport";
import { Input } from "@/components/ui/Input";
import { Button } from "@/components/ui/Button";

interface AddSkillFormProps {
  transport: SyscityWebSocketTransport;
  onAdded?: () => void;
}

export function AddSkillForm({ transport, onAdded }: AddSkillFormProps) {
  const { t } = useTranslation("settings");
  const [addSkillError, setAddSkillError] = useState("");
  const [newSkillName, setNewSkillName] = useState("");
  const [newSkillZip, setNewSkillZip] = useState<File | null>(null);
  const [skillActionLoading, setSkillActionLoading] = useState<string>("");

  const handleAddSkill = async () => {
    setAddSkillError("");
    if (!newSkillName.trim()) {
      setAddSkillError(t("AddSkillForm.errNameRequired"));
      return;
    }
    if (!newSkillZip) {
      setAddSkillError(t("AddSkillForm.errZipRequired"));
      return;
    }
    setSkillActionLoading("add");
    try {
      const arrayBuffer = await newSkillZip.arrayBuffer();
      const bytes = new Uint8Array(arrayBuffer);
      let binary = "";
      for (let i = 0; i < bytes.byteLength; i++) {
        binary += String.fromCharCode(bytes[i]);
      }
      const zipBase64 = btoa(binary);
      const ok = await transport.installSkill(newSkillName.trim(), zipBase64);
      if (ok) {
        setNewSkillName("");
        setNewSkillZip(null);
        onAdded?.();
      } else {
        setAddSkillError(t("AddSkillForm.errInstallFailed"));
      }
    } catch {
      setAddSkillError(t("AddSkillForm.errZipReadFailed"));
    }
    setSkillActionLoading("");
  };

  return (
    <div className="mb-4 p-4 rounded-lg bg-card space-y-3">
      <Input
        label={t("AddSkillForm.nameLabel")}
        value={newSkillName}
        onChange={(e) => setNewSkillName(e.target.value)}
        placeholder="my-skill"
      />
      <div>
        <label className="block text-xs text-secondary mb-1">{t("AddSkillForm.zipLabel")}</label>
        <input
          type="file"
          accept=".zip"
          onChange={(e) => setNewSkillZip(e.target.files?.[0] || null)}
          className="w-full text-sm text-secondary file:mr-3 file:py-1.5 file:px-3 file:rounded-md file:border-0 file:text-xs file:font-medium file:bg-primary-50 file:text-primary-700 dark:file:bg-primary-900/20 dark:file:text-primary-400 hover:file:bg-primary-100"
        />
        <p className="text-[10px] text-secondary/70 mt-1">{t("AddSkillForm.zipHint")}</p>
      </div>
      {addSkillError && (
        <div className="text-xs text-red-600 dark:text-red-400">{addSkillError}</div>
      )}
      <div className="flex justify-end">
        <Button onClick={handleAddSkill} disabled={skillActionLoading === "add"}>
          {skillActionLoading === "add" ? t("AddSkillForm.installing") : t("AddSkillForm.installSkill")}
        </Button>
      </div>
    </div>
  );
}
