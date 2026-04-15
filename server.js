import { createServer } from "http";
import { readFile } from "fs/promises";
import { join, dirname } from "path";
import { fileURLToPath } from "url";
import { getUsageData } from "./parser.js";

const __dirname = dirname(fileURLToPath(import.meta.url));
const PORT = process.env.PORT || 3456;

// cache usage data, refresh every 30s
let cachedData = null;
let lastFetch = 0;
const CACHE_TTL = 30_000;

async function getData() {
  if (cachedData && Date.now() - lastFetch < CACHE_TTL) return cachedData;
  cachedData = await getUsageData();
  lastFetch = Date.now();
  return cachedData;
}

const server = createServer(async (req, res) => {
  try {
    // API endpoint
    if (req.url === "/api/usage") {
      const data = await getData();
      res.writeHead(200, { "Content-Type": "application/json" });
      res.end(JSON.stringify(data));
      return;
    }

    // force refresh
    if (req.url === "/api/refresh") {
      cachedData = null;
      const data = await getData();
      res.writeHead(200, { "Content-Type": "application/json" });
      res.end(JSON.stringify(data));
      return;
    }

    // serve dashboard
    const html = await readFile(join(__dirname, "dashboard.html"), "utf-8");
    res.writeHead(200, { "Content-Type": "text/html" });
    res.end(html);
  } catch (err) {
    res.writeHead(500, { "Content-Type": "application/json" });
    res.end(JSON.stringify({ error: err.message }));
  }
});

server.listen(PORT, () => {
  console.log(`Token tracker running at http://localhost:${PORT}`);
});
