import { openUrl } from "@tauri-apps/plugin-opener";
import type { ReactNode } from "react";
import ReactMarkdown, { type Components } from "react-markdown";
import remarkGfm from "remark-gfm";

/** Links open in the browser, never in the app; only web links are links. */
const components: Components = {
  a: ({ href, children }) =>
    href && /^https?:\/\//i.test(href) ? (
      <a
        href={href}
        title={href}
        onClick={(e) => {
          e.preventDefault();
          void openUrl(href);
        }}
      >
        {children}
      </a>
    ) : (
      <span>{children}</span>
    ),
  // Nothing is fetched on the model's say-so: an image is its description.
  img: ({ alt }) => (alt ? <span className="muted">[{alt}]</span> : null),
};

/**
 * What Otter or the coding agent wrote, rendered as the Markdown it is
 * (D-086). Raw HTML in it stays text: it comes from a model that reads
 * untrusted repositories.
 */
export function Markdown({ text, className, children }: { text: string; className?: string; children?: ReactNode }) {
  return (
    <div className={`markdown${className ? ` ${className}` : ""}`}>
      <ReactMarkdown remarkPlugins={[remarkGfm]} components={components} skipHtml>
        {text}
      </ReactMarkdown>
      {children}
    </div>
  );
}
