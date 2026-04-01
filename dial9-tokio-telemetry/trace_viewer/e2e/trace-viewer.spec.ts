import { test, expect, type ConsoleMessage } from "@playwright/test";

// Collect console errors across all tests
const consoleErrors: string[] = [];

test.beforeEach(async ({ page }) => {
  page.on("console", (msg: ConsoleMessage) => {
    if (msg.type() === "error") {
      consoleErrors.push(`[${msg.type()}] ${msg.text()}`);
    }
  });
});

test.describe("Trace Viewer Sanity", () => {
  test("demo trace loads and viewer appears", async ({ page }) => {
    await page.goto("/trace_viewer/");
    // Drop zone should be visible initially
    await expect(page.locator("#drop-zone")).toBeVisible();
    await expect(page.locator("#viewer")).not.toBeVisible();

    // Click "load demo trace"
    await page.click("#load-demo");

    // Viewer should appear with filename and stats
    await expect(page.locator("#viewer")).toBeVisible({ timeout: 15_000 });
    await expect(page.locator("#tb-filename")).not.toBeEmpty();
    await expect(page.locator("#tb-stats")).toContainText("events");
    await expect(page.locator("#tb-stats")).toContainText("workers");
  });

  test("timeline and worker lanes render", async ({ page }) => {
    await page.goto("/trace_viewer/");
    await page.click("#load-demo");
    await expect(page.locator("#viewer")).toBeVisible({ timeout: 15_000 });

    // Timeline header canvas should be visible
    await expect(page.locator("#timeline-canvas")).toBeVisible();

    // Worker lanes should have child canvas elements
    const laneCanvases = page.locator("#lanes-container canvas");
    await expect(laneCanvases.first()).toBeVisible({ timeout: 5_000 });
    const count = await laneCanvases.count();
    expect(count).toBeGreaterThanOrEqual(2); // at least 2 workers

    // Verify at least one lane canvas has been drawn to (non-blank)
    const hasContent = await laneCanvases.first().evaluate((canvas: HTMLCanvasElement) => {
      const ctx = canvas.getContext("2d");
      if (!ctx || canvas.width === 0 || canvas.height === 0) return false;
      const data = ctx.getImageData(0, 0, canvas.width, canvas.height).data;
      for (let i = 3; i < data.length; i += 4) {
        if (data[i] > 0) return true; // found a non-transparent pixel
      }
      return false;
    });
    expect(hasContent).toBe(true);
  });

  test("clicking a poll shows task detail", async ({ page }) => {
    await page.goto("/trace_viewer/");
    await page.click("#load-demo");
    await expect(page.locator("#viewer")).toBeVisible({ timeout: 15_000 });

    // Use POI navigation to jump to a long poll (guaranteed to have a visible poll)
    await page.locator("#poi-filter").selectOption("long-poll");
    await page.click("#btn-next-poi");

    // Wait for the view to settle, then click the center of the first lane
    const firstLane = page.locator("#lanes-container canvas").first();
    await expect(firstLane).toBeVisible({ timeout: 5_000 });
    const box = await firstLane.boundingBox();
    if (box) {
      await firstLane.click({ position: { x: box.width / 2, y: box.height / 2 } });
    }

    // Task detail panel should become visible
    await expect(page.locator("#task-detail")).toBeVisible({ timeout: 5_000 });
  });

  test("POI navigation works", async ({ page }) => {
    await page.goto("/trace_viewer/");
    await page.click("#load-demo");
    await expect(page.locator("#viewer")).toBeVisible({ timeout: 15_000 });

    // Click next POI
    await page.click("#btn-next-poi");

    // POI counter should show something like "1 of N"
    const counter = page.locator("#poi-counter");
    await expect(counter).not.toBeEmpty({ timeout: 3_000 });
    const text = await counter.textContent();
    expect(text).toMatch(/\d+\/\d+/);
  });

  test("queue depth chart renders", async ({ page }) => {
    await page.goto("/trace_viewer/");
    await page.click("#load-demo");
    await expect(page.locator("#viewer")).toBeVisible({ timeout: 15_000 });

    const queueCanvas = page.locator("#queue-canvas");
    await expect(queueCanvas).toBeVisible();

    // Check the canvas has been drawn to
    const hasContent = await queueCanvas.evaluate((canvas: HTMLCanvasElement) => {
      const ctx = canvas.getContext("2d");
      if (!ctx || canvas.width === 0 || canvas.height === 0) return false;
      const data = ctx.getImageData(0, 0, canvas.width, canvas.height).data;
      for (let i = 3; i < data.length; i += 4) {
        if (data[i] > 0) return true;
      }
      return false;
    });
    expect(hasContent).toBe(true);
  });

  test("no console errors during interaction", async () => {
    expect(consoleErrors).toEqual([]);
  });
});
