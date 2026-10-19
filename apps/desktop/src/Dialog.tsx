import { useEffect, useRef, type ReactNode } from "react";

/**
 * A modal dialog: Escape or the backdrop closes it, focus starts inside.
 * `bare`: no title or padding, the content lays itself out (Settings).
 * Focus goes to `[data-autofocus]` if there is one.
 */
export function Dialog({
  title,
  onClose,
  children,
  bare,
}: {
  title: string;
  onClose: () => void;
  children: ReactNode;
  bare?: string;
}) {
  const box = useRef<HTMLDivElement>(null);
  // The latest, without focusing again when the parent renders anew.
  const close = useRef(onClose);
  close.current = onClose;
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => e.key === "Escape" && close.current();
    window.addEventListener("keydown", onKey);
    const first = box.current?.querySelector<HTMLElement>("[data-autofocus]") ?? box.current?.querySelector<HTMLElement>("input, button, select, textarea");
    first?.focus();
    return () => window.removeEventListener("keydown", onKey);
  }, []);
  return (
    <div className="backdrop" onMouseDown={(e) => e.target === e.currentTarget && onClose()}>
      <div className={bare ? `dialog bare ${bare}` : "dialog"} role="dialog" aria-modal="true" aria-label={title} ref={box}>
        {!bare && <h1 className="dialog-title">{title}</h1>}
        {children}
      </div>
    </div>
  );
}
