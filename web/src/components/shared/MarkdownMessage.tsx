import ReactMarkdown, { type Components } from "react-markdown";
import remarkGfm from "remark-gfm";
import { Children, isValidElement, memo, useState } from "react";
import { CodeBlock } from "./CodeBlock";

/** Detect bare image/video URLs and convert them to markdown embeds. */
function autoEmbedMedia(text: string): string {
  // Match standalone URLs on their own line (or preceded by whitespace)
  // that end with image or video extensions.
  const imageRe =
    /(^|\s)(https?:\/\/[^\s<>"{}|\\^`[\]]+\.(?:png|jpe?g|gif|webp|svg|bmp|ico))(?=$|\s|[.,;!?])/gi;
  const videoRe =
    /(^|\s)(https?:\/\/[^\s<>"{}|\\^`[\]]+\.(?:mp4|webm|mov|mkv|ogv))(?=$|\s|[.,;!?])/gi;

  return text
    .replace(imageRe, (_m, prefix, url) => `${prefix}![image](${url})`)
    .replace(videoRe, (_m, prefix, url) => `${prefix}<video src="${url}" controls />`);
}

// Module-level so the component identities are stable across renders. A
// fresh object literal here (new function identity per render) makes React
// unmount/remount the entire markdown subtree on every unrelated parent
// re-render, which flickers item heights and makes the virtualizer yank the
// scroll position.
const markdownComponents: Components = {
  h1: ({ children }) => (
    <h1 className="text-lg font-bold text-gray-900 dark:text-gray-100 mt-4 mb-2">{children}</h1>
  ),
  h2: ({ children }) => (
    <h2 className="text-base font-bold text-gray-900 dark:text-gray-100 mt-3 mb-2">{children}</h2>
  ),
  h3: ({ children }) => (
    <h3 className="text-sm font-bold text-gray-900 dark:text-gray-100 mt-3 mb-1">{children}</h3>
  ),
  strong: ({ children }) => (
    <strong className="font-semibold text-gray-900 dark:text-gray-100">{children}</strong>
  ),
  code: ({ children, className }) => {
    if (className?.includes("language-")) {
      const language = className.replace("language-", "");
      const code = String(children).replace(/\n$/, "");
      return <CodeBlock code={code} language={language} />;
    }
    return (
      <code className="px-1.5 py-0.5 rounded-md bg-sidebar text-primary text-xs font-mono">
        {children}
      </code>
    );
  },
  pre: ({ children }) => {
    const child = Children.toArray(children)[0];
    if (isValidElement(child) && child.type === CodeBlock) {
      return <>{children}</>;
    }
    return (
      <pre className="rounded-xl bg-sidebar p-4 overflow-x-auto my-3 text-xs font-mono leading-relaxed">
        {children}
      </pre>
    );
  },
  blockquote: ({ children }) => (
    <blockquote className="border-l-2 border-primary-400 dark:border-primary-600 pl-4 my-3 text-secondary italic">
      {children}
    </blockquote>
  ),
  ul: ({ children }) => (
    <ul className="list-disc list-inside my-2 space-y-1 text-sm">{children}</ul>
  ),
  ol: ({ children }) => (
    <ol className="list-decimal list-inside my-2 space-y-1 text-sm">{children}</ol>
  ),
  li: ({ children }) => (
    <li className="text-sm text-gray-700 dark:text-gray-300 leading-relaxed">{children}</li>
  ),
  p: ({ children }) => (
    <p className="text-sm text-gray-700 dark:text-gray-300 leading-relaxed mb-2 last:mb-0">{children}</p>
  ),
  a: ({ children, href }) => (
    <a href={href} className="text-primary-600 dark:text-primary-400 hover:underline" target="_blank" rel="noopener noreferrer">
      {children}
    </a>
  ),
  hr: () => <hr className="my-4 border-subtle" />,
  table: ({ children }) => (
    <table className="w-full text-sm border-collapse my-3">{children}</table>
  ),
  thead: ({ children }) => (
    <thead className="bg-sidebar">{children}</thead>
  ),
  th: ({ children }) => (
    <th className="border-b border-subtle px-3 py-2 text-left text-xs font-semibold text-primary">{children}</th>
  ),
  td: ({ children }) => (
    <td className="border-b border-subtle px-3 py-2 text-sm text-secondary">{children}</td>
  ),
  img: ImageNode,
  video: VideoNode,
};

function MarkdownMessageImpl({ text }: { text: string }) {
  const processed = autoEmbedMedia(text);

  return (
    <div className="prose prose-sm dark:prose-invert max-w-none">
      <ReactMarkdown
        remarkPlugins={[remarkGfm]}
        urlTransform={(url) => url}
        components={markdownComponents}
      >
        {processed}
      </ReactMarkdown>
    </div>
  );
}

export const MarkdownMessage = memo(MarkdownMessageImpl);

function isUnfetchableUrl(url?: string): boolean {
  if (!url) return true;
  // Browser security blocks file:// URLs entirely; skip the doomed request.
  return url.startsWith("file://");
}

function ImageNode({ src, alt }: { src?: string; alt?: string }) {
  const [open, setOpen] = useState(false);
  const [failed, setFailed] = useState(() => isUnfetchableUrl(src));

  if (failed) {
    return (
      <div className="rounded-xl bg-sidebar px-4 py-3 text-xs text-secondary inline-block">
        (image not found: {src})
      </div>
    );
  }

  return (
    <>
      <img
        src={src}
        alt={alt || "image"}
        className="rounded-xl max-w-full max-h-[400px] object-contain cursor-zoom-in hover:opacity-90 transition"
        onClick={() => setOpen(true)}
        loading="lazy"
        onError={() => setFailed(true)}
      />
      {open && (
        <div
          className="fixed inset-0 z-50 flex items-center justify-center bg-black/80 cursor-zoom-out"
          onClick={() => setOpen(false)}
        >
          <img src={src} alt={alt || "image"} className="max-w-[90vw] max-h-[90vh] object-contain" />
        </div>
      )}
    </>
  );
}

function VideoNode({ src }: { src?: string }) {
  return (
    <video
      src={src}
      controls
      preload="metadata"
      className="rounded-xl max-w-full max-h-[400px] my-2"
    />
  );
}
