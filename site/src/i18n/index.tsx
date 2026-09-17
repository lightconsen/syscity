import {
  createContext,
  useContext,
  useEffect,
  useState,
  type ReactNode,
} from "react";

export type Lang = "en" | "zh";

const STORAGE_KEY = "syscity-lang";

const en = {
  nav: {
    features: "Features",
    platforms: "Download",
    quickstart: "Quick Start",
    starLong: "Star on GitHub",
    starShort: "Star",
    cloud: "Syscity Cloud",
    switchTo: "中文",
  },
  hero: {
    badge: "Syscity · AI Agent System",
    titleTop: "One agent runtime,",
    titleBottom: "every device.",
    subtitle:
      "Syscity turns a language model into an agent that lives inside your machine — clicking buttons, browsing the web, running code, and managing files. Runs natively on macOS, Windows, Linux, iOS, and Android. Your data stays on it — what leaves goes only where you point it: your model provider, and any chat channel or MCP server you connect.",
    getStarted: "Get Started",
    viewOnGithub: "View on GitHub",
    copyInstall: "Copy install command",
    installNote: "macOS/Linux use the shell command; Windows uses PowerShell.",
  },
  demo: {
    chromeTitle: "syscity — agent preview",
    alt: "Syscity demo — an agent generates a markdown report and previews it in a split panel",
    captionBefore: "An agent generates a markdown report via",
    captionTool: "write_report",
    captionAfter: ", then previews it in a split-panel view.",
    iosCaption: "Chat, voice, camera & device tools on iOS",
    androidCaption: "Chat, voice, camera & device tools on Android",
  },
  features: {
    eyebrow: "Features",
    titleBefore: "Why",
    titleBrand: "Syscity",
    titleAfter: "?",
    lead: "Most “AI agents” are just chatbots with function calling. Syscity agents control your computer — not just your API keys.",
    items: [
      {
        title: "Your desktop is the canvas",
        body: "Click buttons, type text, read UI trees, take screenshots. Agents act on your computer — not just chat.",
      },
      {
        title: "Your browser, automated",
        body: "Navigate, fill forms, capture network requests, debug console errors with sourcemaps. The agent debugs like a developer.",
      },
      {
        title: "Your tools, connected",
        body: "MCP servers, shell commands, file operations, AppleScript. Bring your own ecosystem into the loop.",
      },
      {
        title: "Your data, private",
        body: "Memory, knowledge bases and artifacts stay on your machine, with no service of ours in the middle. Only the endpoints you configure — your model provider, the chat channels you connect — see what you send them.",
      },
      {
        title: "Every platform, one agent",
        body: "macOS, Windows, Linux, iOS, Android. The same runtime and memory, on every device you own.",
      },
      {
        title: "Multiple models, one agent",
        body: "Swap between OpenAI, Anthropic, DeepSeek, GLM, Ollama, or custom endpoints. Use the right model for each task.",
      },
    ],
  },
  actionCognition: {
    titleA: "Action.",
    titleB: "Cognition.",
    lead: "An agent system bridges language models with real computing environments — an action layer, a memory layer, and a control plane.",
    actionEyebrow: "Action",
    actionTitle: "Things it does on your machine",
    actionItems: [
      "Desktop Control",
      "AppleScript",
      "Shell Commands",
      "Code Execution",
      "Browser Automation",
      "File Operations",
    ],
    cognitionEyebrow: "Cognition",
    cognitionTitle: "How it thinks and remembers",
    cognitionItems: [
      "Multi-Provider LLM",
      "Sub-Agents (ACP)",
      "Vector Memory",
      "MCP Support",
      "WASM Plugins",
    ],
  },
  platforms: {
    titleA: "Every device,",
    titleB: "one agent",
    lead: "The same local runtime and memory on every machine you own. Chat, voice, camera, and device tools on your phone — full desktop control on your computer.",
    iosAlt: "Syscity on iOS",
    androidAlt: "Syscity on Android",
    downloadCta: "Download the client",
  },
  download: {
    title: "Download",
    subtitle: "Every device, one agent",
    lead: "The same local runtime and memory on every machine you own. Chat, voice, camera, and device tools on your phone — full desktop control on your computer.",
    cloudNote: "After install, sign in to your Syscity Cloud account to use cloud models, search, and the marketplace.",
  },
  cloud: {
    eyebrow: "Syscity Cloud",
    titleA: "Local engine,",
    titleB: "cloud power.",
    lead: "One local runtime with cloud superpowers — cloud LLMs and web search with zero config, cross-device sync, and a marketplace of connectors and experts.",
    card1Title: "Zero-config cloud models",
    card1Desc: "Cloud LLMs and web search work right after sign-in — no API keys to manage, credits billed on the cloud.",
    card2Title: "Multi-device sync",
    card2Desc: "Sessions, artifacts and connectors stay in sync across every machine you own.",
    card3Title: "Connector & expert market",
    card3Desc: "Browse and install connectors, skills and expert agents from the cloud marketplace.",
    plansEyebrow: "Pricing",
    plansTitle: "Plans",
    plansSubtitle: "Credits are granted monthly; plans only set the allowance, not the features.",
    recommended: "Recommended",
    plan_free_name: "Free",
    plan_free_price: "Free",
    plan_free_credits: "200 credits/mo",
    plan_free_features: ["Cloud LLMs & search", "Knowledge base", "Marketplace", "Community support"],
    plan_pro_name: "Pro",
    plan_pro_price: "$10/mo",
    plan_pro_credits: "3,000 credits/mo",
    plan_pro_features: ["Everything in Free", "Higher limits", "Priority support"],
    plan_enterprise_name: "Enterprise",
    plan_enterprise_price: "$24/mo",
    plan_enterprise_credits: "16,000 credits/mo",
    plan_enterprise_features: ["Everything in Pro", "Custom limits", "Dedicated support"],
    cta: "Try Syscity Cloud",
  },
  quickstart: {
    eyebrow: "Quick Start",
    titleA: "Up and running in",
    titleB: "seconds",
    lead: "No new IDE, no cloud subscription, no complex deployment. Install, configure, start — then ask your agent to take a screenshot, build a report, or automate a task.",
    readDocs: "Read the docs",
    githubReadme: "GitHub README",
    termInstalled: "installed syscity v0.2.0",
    termConfigured: "configured providers.openai.api_key",
    termRunning: "gateway running at http://127.0.0.1:18080",
  },
  footer: {
    tagline: "AI agents that control your computer. Local-first, one runtime, every device.",
    product: "Product",
    community: "Community",
    features: "Features",
    platforms: "Platforms",
    quickstart: "Quick Start",
    documentation: "Documentation",
    cloud: "Syscity Cloud",
    license: "Apache-2.0 Licensed · Open Source",
  },
};

export type Dict = typeof en;

const zh: Dict = {
  nav: {
    features: "功能",
    platforms: "下载",
    quickstart: "快速开始",
    starLong: "在 GitHub 上点赞",
    starShort: "Star",
    cloud: "Syscity 云端",
    switchTo: "EN",
  },
  hero: {
    badge: "Syscity · AI 智能体系统",
    titleTop: "一个智能体运行时，",
    titleBottom: "覆盖每一台设备。",
    subtitle:
      "Syscity 把大语言模型变成住在你电脑里的智能体——点击按钮、浏览网页、运行代码、管理文件。原生支持 macOS、Windows、Linux、iOS 和 Android。数据留在本机，只有你指向的地方会收到它——你配置的模型服务，以及你接入的聊天渠道或 MCP 服务器。",
    getStarted: "开始使用",
    viewOnGithub: "在 GitHub 查看",
    copyInstall: "复制安装命令",
    installNote: "macOS/Linux 用 shell 命令；Windows 用 PowerShell。",
  },
  demo: {
    chromeTitle: "syscity — 智能体预览",
    alt: "Syscity 演示——智能体生成一份 Markdown 报告，并在分栏面板中预览",
    captionBefore: "智能体通过",
    captionTool: "write_report",
    captionAfter: "生成一份 Markdown 报告，随后在分栏视图中预览。",
    iosCaption: "iOS 上的聊天、语音、相机与设备工具",
    androidCaption: "Android 上的聊天、语音、相机与设备工具",
  },
  features: {
    eyebrow: "功能",
    titleBefore: "为什么选择",
    titleBrand: "Syscity",
    titleAfter: "？",
    lead: "市面上大多数“AI 智能体”只是带函数调用的聊天机器人。Syscity 的智能体真正掌控你的电脑——而不只是你的 API 密钥。",
    items: [
      {
        title: "桌面就是画布",
        body: "点击按钮、输入文字、读取 UI 树、截取屏幕。智能体直接操作你的电脑，而不只是陪你聊天。",
      },
      {
        title: "浏览器自动化",
        body: "导航、填表单、抓网络请求、结合 sourcemap 排查控制台报错。智能体像开发者一样调试。",
      },
      {
        title: "连接你的工具",
        body: "MCP 服务、Shell 命令、文件操作、AppleScript。把你自己的生态接入整个循环。",
      },
      {
        title: "数据完全私有",
        body: "记忆、知识库和产物文件都留在你自己的机器上，中间没有我们的服务。只有你配置的端点——模型服务、你接入的聊天渠道——会看到你发给它们的内容。",
      },
      {
        title: "全平台，同一个智能体",
        body: "macOS、Windows、Linux、iOS、Android。同一套运行时和记忆，跟着你走每一台设备。",
      },
      {
        title: "多模型，同一个智能体",
        body: "在 OpenAI、Anthropic、DeepSeek、GLM、Ollama 或自定义端点之间自由切换，为每个任务选最合适的模型。",
      },
    ],
  },
  actionCognition: {
    titleA: "行动。",
    titleB: "认知。",
    lead: "智能体系统把大语言模型与真实计算环境连接起来——一层行动、一层记忆，外加一个控制平面。",
    actionEyebrow: "行动",
    actionTitle: "它在你的机器上做的事",
    actionItems: [
      "桌面控制",
      "AppleScript",
      "Shell 命令",
      "代码执行",
      "浏览器自动化",
      "文件操作",
    ],
    cognitionEyebrow: "认知",
    cognitionTitle: "它如何思考与记忆",
    cognitionItems: [
      "多供应商 LLM",
      "子智能体（ACP）",
      "向量记忆",
      "MCP 支持",
      "WASM 插件",
    ],
  },
  platforms: {
    titleA: "每一台设备，",
    titleB: "同一个智能体",
    lead: "你拥有的每台机器上，都是同一套本地运行时和同一份记忆。手机上有聊天、语音、相机和设备工具，电脑上则是完整的桌面控制。",
    iosAlt: "iOS 上的 Syscity",
    androidAlt: "Android 上的 Syscity",
    downloadCta: "下载客户端",
  },
  download: {
    title: "下载",
    subtitle: "每一台设备，同一个智能体",
    lead: "你拥有的每台机器上，都是同一套本地运行时和同一份记忆。手机上有聊天、语音、相机和设备工具，电脑上则是完整的桌面控制。",
    cloudNote: "安装后登录你的 Syscity 云端账号，即可使用云模型、搜索与市场。",
  },
  cloud: {
    eyebrow: "Syscity 云端",
    titleA: "本地引擎，",
    titleB: "云端加持。",
    lead: "一个本地运行时 + 云端能力——登录即用云模型与联网搜索（零配置）、多设备同步、以及连接器与专家市场。",
    card1Title: "零配置云模型",
    card1Desc: "登录后云模型与联网搜索即刻可用，无需管理任何 API key，积分在云端计费。",
    card2Title: "多设备同步",
    card2Desc: "会话、工件与连接器在每台设备间保持同步。",
    card3Title: "连接器与专家市场",
    card3Desc: "从云端市场浏览并安装连接器、技能与专家智能体。",
    plansEyebrow: "定价",
    plansTitle: "套餐",
    plansSubtitle: "积分按月发放；套餐只设额度，不限功能。",
    recommended: "推荐",
    plan_free_name: "免费",
    plan_free_price: "免费",
    plan_free_credits: "每月 200 积分",
    plan_free_features: ["云模型与搜索", "知识库", "市场", "社区支持"],
    plan_pro_name: "专业版",
    plan_pro_price: "¥68/月",
    plan_pro_credits: "每月 3,000 积分",
    plan_pro_features: ["免费版全部", "更高限额", "优先支持"],
    plan_enterprise_name: "企业版",
    plan_enterprise_price: "¥168/月",
    plan_enterprise_credits: "每月 16,000 积分",
    plan_enterprise_features: ["专业版全部", "自定义限额", "专属支持"],
    cta: "试用 Syscity 云端",
  },
  quickstart: {
    eyebrow: "快速开始",
    titleA: "几秒钟内",
    titleB: "跑起来",
    lead: "不需要新的 IDE，不需要云订阅，也没有复杂的部署。安装、配置、启动——然后让智能体帮你截个屏、生成一份报告，或者自动完成一项任务。",
    readDocs: "阅读文档",
    githubReadme: "GitHub README",
    termInstalled: "已安装 syscity v0.2.0",
    termConfigured: "已配置 providers.openai.api_key",
    termRunning: "网关运行于 http://127.0.0.1:18080",
  },
  footer: {
    tagline: "掌控你电脑的 AI 智能体。本地优先，一套运行时，覆盖每一台设备。",
    product: "产品",
    community: "社区",
    features: "功能",
    platforms: "平台",
    quickstart: "快速开始",
    documentation: "文档",
    cloud: "Syscity 云端",
    license: "Apache-2.0 许可 · 开源",
  },
};

const dicts: Record<Lang, Dict> = { en, zh };

function detectInitialLang(): Lang {
  try {
    const saved = localStorage.getItem(STORAGE_KEY);
    if (saved === "en" || saved === "zh") return saved;
  } catch {
    /* storage unavailable — fall through */
  }
  const nav = typeof navigator !== "undefined" ? navigator.language : "en";
  return nav.toLowerCase().startsWith("zh") ? "zh" : "en";
}

interface LangContextValue {
  lang: Lang;
  setLang: (lang: Lang) => void;
  t: Dict;
}

const LangContext = createContext<LangContextValue>({
  lang: "en",
  setLang: () => {},
  t: en,
});

export function LangProvider({ children }: { children: ReactNode }) {
  const [lang, setLangState] = useState<Lang>(detectInitialLang);

  useEffect(() => {
    document.documentElement.lang = lang === "zh" ? "zh-CN" : "en";
  }, [lang]);

  const setLang = (next: Lang) => {
    setLangState(next);
    try {
      localStorage.setItem(STORAGE_KEY, next);
    } catch {
      /* storage unavailable — ignore */
    }
  };

  return (
    <LangContext.Provider value={{ lang, setLang, t: dicts[lang] }}>
      {children}
    </LangContext.Provider>
  );
}

export function useLanguage(): LangContextValue {
  return useContext(LangContext);
}

/** The Syscity Cloud URL with the current language as a `?lang=` query param,
 * so the console picks up the same language on navigation. */
export function cloudUrl(lang: Lang): string {
  return `https://cloud.syscity.net/?lang=${lang}`;
}
