import { it, vi } from "vitest";
import { writeFileSync } from "node:fs";
import { LedDriver, STATUS_LED_MAP } from "../src/hardware/led.js";
it("superseded first step", async () => {
  vi.useFakeTimers();
  const t0 = Date.now();
  const calls: unknown[] = [];
  const d = new LedDriver({ map: STATUS_LED_MAP, log: () => {}, probe: async () => ({ ok: true }),
    write: async (pins, high) => { calls.push({ t: Date.now() - t0, pins: [...pins], high }); return true; } });
  await d.init();
  void d.play("red", "blink-fast");
  void d.play("green", "solid");
  await vi.advanceTimersByTimeAsync(50);
  // and a real-pinctrl-like writer (resolves after 5 ms)
  const calls2: unknown[] = [];
  const d2 = new LedDriver({ map: STATUS_LED_MAP, log: () => {}, probe: async () => ({ ok: true }),
    write: (pins, high) => { calls2.push({ t: Date.now() - t0, pins: [...pins], high }); return new Promise((r) => setTimeout(() => r(true), 5)); } });
  await d2.init();
  void d2.play("green", "heartbeat");
  await vi.advanceTimersByTimeAsync(2);
  void d2.play("white", "flash2");
  await vi.advanceTimersByTimeAsync(50);
  writeFileSync("test/zz-review-supersede.json", JSON.stringify({ immediate: calls, delayed5ms: calls2 }, null, 1));
});
