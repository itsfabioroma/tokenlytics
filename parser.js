import { readFile, stat } from "fs/promises";
import { homedir } from "os";
import { Glob } from "bun";

const CLAUDE_DIR = `${homedir()}/.claude`;
const PROJECTS_DIR = `${CLAUDE_DIR}/projects`;
const STATS_CACHE = `${CLAUDE_DIR}/stats-cache.json`;

// per-file cache: only re-parse when mtime changes
const fileCache = new Map();

// Parse JSONL, return per-message token data
async function parseJSONL(filePath) {
  const info = await stat(filePath).catch(() => null);
  if (!info) return [];
  const mtime = info.mtimeMs;
  const cached = fileCache.get(filePath);
  if (cached && cached.mtime === mtime) return cached.messages;

  const content = await readFile(filePath, "utf-8");
  const lines = content.split("\n").filter(Boolean);
  const messages = [];

  for (const line of lines) {
    try {
      const obj = JSON.parse(line);
      if (obj.type !== "assistant") continue;
      const usage = obj.message?.usage;
      if (!usage) continue;

      messages.push({
        timestamp: obj.timestamp,
        model: obj.message.model || "unknown",
        input: usage.input_tokens || 0,
        output: usage.output_tokens || 0,
        cacheRead: usage.cache_read_input_tokens || 0,
        cacheCreation: usage.cache_creation_input_tokens || 0,
      });
    } catch {}
  }

  fileCache.set(filePath, { mtime, messages });
  return messages;
}

export async function getUsageData() {
  const allMessages = [];
  const glob = new Glob("**/*.jsonl");
  for await (const path of glob.scan(PROJECTS_DIR)) {
    const messages = await parseJSONL(`${PROJECTS_DIR}/${path}`);
    allMessages.push(...messages);
  }

  const now = new Date();
  const ms = (h) => h * 60 * 60 * 1000;

  // current windows
  const cutoff24h = new Date(now - ms(24)).toISOString();
  const cutoff7d = new Date(now - ms(24 * 7)).toISOString();
  const cutoff30d = new Date(now - ms(24 * 30)).toISOString();

  // previous period windows (for trend comparison)
  const cutoffPrev24h = new Date(now - ms(48)).toISOString();
  const cutoffPrev7d = new Date(now - ms(24 * 14)).toISOString();
  const cutoffPrev30d = new Date(now - ms(24 * 60)).toISOString();

  // aggregate
  const modelUsage = {};
  let jsonlTotal = 0;
  const windows = {
    last24h: 0, last7d: 0, last30d: 0, allTime: 0,
    prev24h: 0, prev7d: 0, prev30d: 0,
  };

  for (const msg of allMessages) {
    const tokens = msg.input + msg.output;
    jsonlTotal += tokens;

    // current windows
    if (msg.timestamp >= cutoff30d) windows.last30d += tokens;
    if (msg.timestamp >= cutoff7d) windows.last7d += tokens;
    if (msg.timestamp >= cutoff24h) windows.last24h += tokens;

    // previous period (e.g. prev24h = 48h ago to 24h ago)
    if (msg.timestamp >= cutoffPrev24h && msg.timestamp < cutoff24h) windows.prev24h += tokens;
    if (msg.timestamp >= cutoffPrev7d && msg.timestamp < cutoff7d) windows.prev7d += tokens;
    if (msg.timestamp >= cutoffPrev30d && msg.timestamp < cutoff30d) windows.prev30d += tokens;

    // model aggregation
    if (!modelUsage[msg.model]) {
      modelUsage[msg.model] = { input: 0, output: 0, cacheRead: 0, cacheCreation: 0, messages: 0 };
    }
    modelUsage[msg.model].input += msg.input;
    modelUsage[msg.model].output += msg.output;
    modelUsage[msg.model].cacheRead += msg.cacheRead;
    modelUsage[msg.model].cacheCreation += msg.cacheCreation;
    modelUsage[msg.model].messages += 1;
  }

  // all-time from stats-cache (matches Claude /usage)
  let statsCacheTotal = 0;
  try {
    const raw = await readFile(STATS_CACHE, "utf-8");
    const cache = JSON.parse(raw);
    for (const u of Object.values(cache.modelUsage || {})) {
      statsCacheTotal += (u.inputTokens || 0) + (u.outputTokens || 0);
    }
  } catch {}
  windows.allTime = Math.max(jsonlTotal, statsCacheTotal);

  const estimatedCost = estimateCost(modelUsage);

  return { windows, modelUsage, estimatedCost, lastUpdated: new Date().toISOString() };
}

// Estimate API cost
function estimateCost(modelUsage) {
  const pricing = {
    "claude-opus-4-6": { input: 15, output: 75, cacheRead: 1.5, cacheCreation: 18.75 },
    "claude-opus-4-5-20251101": { input: 15, output: 75, cacheRead: 1.5, cacheCreation: 18.75 },
    "claude-sonnet-4-5-20250929": { input: 3, output: 15, cacheRead: 0.3, cacheCreation: 3.75 },
    "claude-sonnet-4-6": { input: 3, output: 15, cacheRead: 0.3, cacheCreation: 3.75 },
    "claude-haiku-4-5-20251001": { input: 0.8, output: 4, cacheRead: 0.08, cacheCreation: 1 },
  };

  let total = 0;
  const breakdown = {};

  for (const [model, usage] of Object.entries(modelUsage)) {
    const priceKey = Object.keys(pricing).find((k) => model.includes(k) || k.includes(model));
    const price = pricing[priceKey] || pricing["claude-sonnet-4-5-20250929"];

    const cost =
      (usage.input / 1_000_000) * price.input +
      (usage.output / 1_000_000) * price.output +
      (usage.cacheRead / 1_000_000) * price.cacheRead +
      (usage.cacheCreation / 1_000_000) * price.cacheCreation;

    breakdown[model] = Math.round(cost * 100) / 100;
    total += cost;
  }

  return { total: Math.round(total * 100) / 100, breakdown };
}
