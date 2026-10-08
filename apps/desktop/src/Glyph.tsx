import type { Glyph as Kind } from "./types";

/** Status mark: each state differs in shape, not only in color. */
export function Glyph({ kind, size = 12 }: { kind: Kind; size?: number }) {
  return (
    <svg width={size} height={size} viewBox="0 0 12 12" aria-hidden="true" className="glyph">
      {kind === "needs" && <circle cx="6" cy="6" r="5" fill="var(--needs)" />}
      {kind === "working" && (
        <>
          <circle cx="6" cy="6" r="4.25" fill="none" stroke="var(--working)" strokeWidth="1.5" />
          <circle cx="6" cy="6" r="1.75" fill="var(--working)" />
        </>
      )}
      {kind === "done" && (
        <path d="M2.5 6.2l2.3 2.3 4.7-5" fill="none" stroke="var(--done)" strokeWidth="1.6" strokeLinecap="round" strokeLinejoin="round" />
      )}
      {kind === "failed" && <path d="M3 3l6 6M9 3l-6 6" stroke="var(--failed)" strokeWidth="1.6" strokeLinecap="round" />}
      {kind === "idle" && <circle cx="6" cy="6" r="4.25" fill="none" stroke="var(--idle)" strokeWidth="1.5" />}
    </svg>
  );
}
