import { defineConfig } from "@playwright/test";

export default defineConfig({
  testDir: "./e2e",
  timeout: 30_000,
  retries: 0,
  use: {
    baseURL: "http://localhost:3000/trace_viewer/",
    headless: true,
  },
  projects: [
    {
      name: "chromium",
      use: { browserName: "chromium" },
    },
  ],
  webServer: {
    command: "python3 ../serve.py",
    url: "http://localhost:3000",
    reuseExistingServer: !process.env.CI,
    timeout: 10_000,
  },
});
