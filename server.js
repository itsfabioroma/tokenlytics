import { readFile } from "fs/promises";
import { join } from "path";
import { getUsageData } from "./parser.js";

const __dirname = import.meta.dir;
const PORT = process.env.PORT || 3456;

// cache usage data, refresh every 2s (file mtime cache makes re-parse cheap)
let cachedData = null;
let lastFetch = 0;
const CACHE_TTL = 2_000;

async function getData() {
  if (cachedData && Date.now() - lastFetch < CACHE_TTL) return cachedData;
  cachedData = await getUsageData();
  lastFetch = Date.now();
  return cachedData;
}

// CORS headers for agent access
const corsHeaders = {
  "Access-Control-Allow-Origin": "*",
  "Access-Control-Allow-Methods": "GET, OPTIONS",
  "Access-Control-Allow-Headers": "Content-Type",
};

const server = Bun.serve({
  port: PORT,
  async fetch(req) {
    const url = new URL(req.url);

    // CORS preflight
    if (req.method === "OPTIONS") {
      return new Response(null, { status: 204, headers: corsHeaders });
    }

    try {
      // full usage data
      if (url.pathname === "/api/usage") {
        const data = await getData();
        return Response.json(data, { headers: corsHeaders });
      }

      // quick summary — single number agents can parse fast
      if (url.pathname === "/api/tokens") {
        const data = await getData();
        const w = data.windows;
        return Response.json({
          last24h: w.last24h,
          last7d: w.last7d,
          last30d: w.last30d,
          allTime: w.allTime,
          trend24h: w.prev24h ? Math.round((w.last24h - w.prev24h) / w.prev24h * 100) : null,
          trend7d: w.prev7d ? Math.round((w.last7d - w.prev7d) / w.prev7d * 100) : null,
          trend30d: w.prev30d ? Math.round((w.last30d - w.prev30d) / w.prev30d * 100) : null,
        }, { headers: corsHeaders });
      }

      // model breakdown
      if (url.pathname === "/api/models") {
        const data = await getData();
        return Response.json(data.modelUsage, { headers: corsHeaders });
      }

      // cost estimate
      if (url.pathname === "/api/cost") {
        const data = await getData();
        return Response.json(data.estimatedCost, { headers: corsHeaders });
      }

      // serve dashboard
      const html = await readFile(join(__dirname, "dashboard.html"), "utf-8");
      return new Response(html, { headers: { "Content-Type": "text/html" } });
    } catch (err) {
      return Response.json({ error: err.message }, { status: 500, headers: corsHeaders });
    }
  },
});

console.log(`Tokenlytics running at http://localhost:${server.port}`);
console.log(`\nAPI endpoints:`);
console.log(`  GET /api/usage   — full usage data`);
console.log(`  GET /api/tokens  — token counts + trends`);
console.log(`  GET /api/models  — per-model breakdown`);
console.log(`  GET /api/cost    — estimated API cost`);
