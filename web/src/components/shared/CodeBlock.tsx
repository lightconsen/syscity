import { useMemo, useState, useCallback } from "react";
import { useTranslation } from "react-i18next";
import { useShikiHighlighter } from "@/hooks/useShikiHighlighter";
import { useThemeStore } from "@/stores/themeStore";
import { Check, Copy } from "lucide-react";

interface CodeBlockProps {
  code: string;
  language?: string;
}

export function CodeBlock({ code, language = "text" }: CodeBlockProps) {
  const { t } = useTranslation("common");
  const highlighter = useShikiHighlighter();
  const resolvedTheme = useThemeStore((s) => s.resolvedTheme);
  const [copied, setCopied] = useState(false);

  const html = useMemo(() => {
    if (!highlighter) return null;
    const themeName = resolvedTheme === "dark" ? "github-dark" : "github-light";
    try {
      return highlighter.codeToHtml(code, {
        lang: language === "text" || !language ? "text" : language,
        theme: themeName,
      });
    } catch {
      return highlighter.codeToHtml(code, {
        lang: "text",
        theme: themeName,
      });
    }
  }, [highlighter, code, language, resolvedTheme]);

  const handleCopy = useCallback(() => {
    navigator.clipboard.writeText(code).then(() => {
      setCopied(true);
      setTimeout(() => setCopied(false), 2000);
    });
  }, [code]);

  if (!html) {
    return (
      <div className="relative rounded-xl bg-sidebar overflow-hidden my-3">
        <div className="flex items-center justify-between px-4 py-2 border-b border-subtle bg-card">
          <span className="text-[10px] font-medium text-secondary uppercase tracking-wider">
            {language}
          </span>
        </div>
        <pre className="p-4 overflow-x-auto text-xs font-mono leading-relaxed text-primary">
          <code>{code}</code>
        </pre>
      </div>
    );
  }

  return (
    <div className="relative rounded-xl bg-sidebar overflow-hidden my-3 group">
      <div className="flex items-center justify-between px-4 py-2 border-b border-subtle bg-card">
        <span className="text-[10px] font-medium text-secondary uppercase tracking-wider">
          {language}
        </span>
        <button
          onClick={handleCopy}
          className="flex items-center gap-1 px-2 py-1 rounded-md text-[10px] text-secondary hover:bg-black/5 dark:hover:bg-white/5 transition opacity-0 group-hover:opacity-100 focus:opacity-100"
          aria-label={t("CodeBlock.copyCode")}
          title={t("CodeBlock.copy")}
        >
          {copied ? (
            <>
              <Check className="w-3 h-3" />
              <span>{t("CodeBlock.copied")}</span>
            </>
          ) : (
            <>
              <Copy className="w-3 h-3" />
              <span>{t("CodeBlock.copy")}</span>
            </>
          )}
        </button>
      </div>
      <div
        className="p-4 overflow-x-auto text-xs leading-relaxed [&>pre]:!bg-transparent [&>pre]:!p-0 [&>pre]:!m-0"
        dangerouslySetInnerHTML={{ __html: html }}
      />
    </div>
  );
}
