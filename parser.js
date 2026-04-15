import { readFile } from "fs/promises";
import { homedir } from "os";
import { Glob } from "bun";

const CLAUDE_DIR = `${homedir()}/.claude`;
const PROJECTS_DIR = `${CLAUDE_DIR}/projects`;
const STATS_CACHE = `${CLAUDE_DIR}/stats-cache.json`;

// Read stats-cache.json — same source Claude Code's /usage uses
async function readStatsCache() {
  const raw = await readFile(STATS_CACHE, "utf-8");
  return JSON.parse(raw);
}

// Parse JSONL for time-windowed data (24h/7d/30d need timestamps)
async function parseJSONL(filePath) {
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
        tokens: (usage.input_tokens || 0) + (usage.output_tokens || 0),
      });
    } catch {}
  }
  return messages;
}

export async function getUsageData() {
  // all-time totals from stats-cache (matches Claude's /usage exactly)
  const cache = await readStatsCache();
  const modelUsage = {};
  let allTime = 0;

  for (const [model, u] of Object.entries(cache.modelUsage || {})) {
    const total = u.inputTokens + u.outputTokens;
    allTime += total;
    modelUsage[model] = {
      input: u.inputTokens,
      output: u.outputTokens,
      cacheRead: u.cacheReadInputTokens,
      cacheCreation: u.cacheCreationInputTokens,
      messages: 0,
    };
  }

  // time windows from JSONL (need timestamps)
  const now = new Date();
  const cutoff24h = new Date(now - 24 * 60 * 60 * 1000).toISOString();
  const cutoff7d = new Date(now - 7 * 24 * 60 * 60 * 1000).toISOString();
  const cutoff30d = new Date(now - 30 * 24 * 60 * 60 * 1000).toISOString();
  const windows = { last24h: 0, last7d: 0, last30d: 0, allTime };

  const glob = new Glob("**/*.jsonl");
  for await (const path of glob.scan(PROJECTS_DIR)) {
    const messages = await parseJSONL(`${PROJECTS_DIR}/${path}`);
    for (const msg of messages) {
      if (msg.timestamp >= cutoff30d) windows.last30d += msg.tokens;
      if (msg.timestamp >= cutoff7d) windows.last7d += msg.tokens;
      if (msg.timestamp >= cutoff24h) windows.last24h += msg.tokens;
    }
  }

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
