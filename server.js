import { readFile } from "fs/promises";
import { join } from "path";
import { getUsageData } from "./parser.js";

const __dirname = import.meta.dir;
const PORT = process.env.PORT || 3456;

// cache usage data, refresh every 5s
let cachedData = null;
let lastFetch = 0;
const CACHE_TTL = 5_000;

async function getData() {
  if (cachedData && Date.now() - lastFetch < CACHE_TTL) return cachedData;
  cachedData = await getUsageData();
  lastFetch = Date.now();
  return cachedData;
}

const server = Bun.serve({
  port: PORT,
  async fetch(req) {
    const url = new URL(req.url);

    try {
      if (url.pathname === "/api/usage") {
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
