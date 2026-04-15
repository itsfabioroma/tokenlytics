import { readFile } from "fs/promises";
import { join } from "path";
import { getUsageData } from "./parser.js";
import { startOtelReceiver, getOtelData } from "./otel-receiver.js";

const __dirname = import.meta.dir;
const PORT = process.env.PORT || 3456;

// cache JSONL usage data, refresh every 30s
let cachedData = null;
let lastFetch = 0;
const CACHE_TTL = 30_000;

async function getData() {
  if (cachedData && Date.now() - lastFetch < CACHE_TTL) return cachedData;
  cachedData = await getUsageData();
  lastFetch = Date.now();
  return cachedData;
}

// start OTel receiver on port 4318
startOtelReceiver();

// main dashboard server
const server = Bun.serve({
  port: PORT,
  async fetch(req) {
    const url = new URL(req.url);

    try {
      // historical data from JSONL files
      if (url.pathname === "/api/usage") {
        const data = await getData();
        return Response.json(data);
      }

      // real-time OTel data
      if (url.pathname === "/api/otel") {
        return Response.json(getOtelData());
      }

      // force refresh
      if (url.pathname === "/api/refresh") {
        cachedData = null;
        const data = await getData();
        return Response.json(data);
      }

      // serve dashboard
      const html = await readFile(join(__dirname, "dashboard.html"), "utf-8");
      return new Response(html, { headers: { "Content-Type": "text/html" } });
    } catch (err) {
      return Response.json({ error: err.message }, { status: 500 });
    }
  },
});

console.log(`Tokenlytics running at http://localhost:${server.port}`);
console.log(`\nTo enable real-time tracking, start Claude Code with:`);
console.log(`  CLAUDE_CODE_ENABLE_TELEMETRY=1 OTEL_METRICS_EXPORTER=otlp OTEL_LOGS_EXPORTER=otlp OTEL_EXPORTER_OTLP_PROTOCOL=http/json OTEL_EXPORTER_OTLP_ENDPOINT=http://localhost:4318 claude`);
