/// <reference types="vitest/config" />
import { defineConfig } from "vite";

// In development glacierd runs separately on 8260; in production it serves web/dist.
export default defineConfig({
  server: { proxy: { "/api": "http://127.0.0.1:8260" } },
  test: { environment: "node" },
});
