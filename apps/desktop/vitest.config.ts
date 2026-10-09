import { defineConfig } from "vitest/config";
import react from "@vitejs/plugin-react";

// UI tests run in a simulated DOM; Tauri's APIs are mocked per test.
export default defineConfig({
  plugins: [react()],
  test: { environment: "happy-dom", include: ["src/**/*.test.{ts,tsx}"] },
});
