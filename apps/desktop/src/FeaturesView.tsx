/** The Features view (D-041): product-oriented work, independent of the Workspace view. */
export function FeaturesView() {
  return (
    <main className="pane empty">
      <div className="drag-strip" data-tauri-drag-region />
      <h1 className="empty-title">Features</h1>
      <p className="muted">Describe what you want built; Otter plans it and drives the work in your workspaces.</p>
    </main>
  );
}
