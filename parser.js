import { readdir, readFile, stat } from "fs/promises";
import { join, basename } from "path";
import { homedir } from "os";

const CLAUDE_DIR = join(homedir(), ".claude");
const PROJECTS_DIR = join(CLAUDE_DIR, "projects");

// Parse a single JSONL file, extracting token usage per assistant message
async function parseJSONL(filePath) {
  const content = await readFile(filePath, "utf-8");
  const lines = content.split("\n").filter(Boolean);

  const messages = [];
  for (const line of lines) {
    try {
      const obj = JSON.parse(line);
      if (obj.type !== "assistant") continue;

      const msg = obj.message || {};
      const usage = msg.usage;
      if (!usage) continue;

      messages.push({
        timestamp: obj.timestamp,
        model: msg.model || "unknown",
        sessionId: obj.sessionId,
        inputTokens: usage.input_tokens || 0,
        outputTokens: usage.output_tokens || 0,
        cacheReadTokens: usage.cache_read_input_tokens || 0,
        cacheCreationTokens: usage.cache_creation_input_tokens || 0,
      });
    } catch {
      // skip malformed lines
    }
  }
  return messages;
}

// Discover all JSONL files across projects
async function findJSONLFiles() {
  const files = [];

  try {
    const projects = await readdir(PROJECTS_DIR);

    for (const project of projects) {
      const projectDir = join(PROJECTS_DIR, project);
      const projectStat = await stat(projectDir).catch(() => null);
      if (!projectStat?.isDirectory()) continue;

      // scan top-level JSONL files
      const entries = await readdir(projectDir).catch(() => []);
      for (const entry of entries) {
        if (entry.endsWith(".jsonl")) {
          files.push({
            path: join(projectDir, entry),
            project: project.replace(/-Users-fabioroma-Code-?/, "").replace(/-/g, "/"),
            sessionId: basename(entry, ".jsonl"),
          });
        }

        // scan subagent JSONLs
        if (entry === "subagents" || entry.match(/^[0-9a-f-]+$/)) {
          const subDir = join(projectDir, entry);
          const subStat = await stat(subDir).catch(() => null);
          if (!subStat?.isDirectory()) continue;

          const subEntries = await readdir(subDir).catch(() => []);
          for (const sub of subEntries) {
            if (sub.endsWith(".jsonl")) {
              files.push({
                path: join(subDir, sub),
                project: project.replace(/-Users-fabioroma-Code-?/, "").replace(/-/g, "/"),
                sessionId: basename(sub, ".jsonl"),
              });
            }

            // nested subagents dir inside session dir
            if (sub === "subagents") {
              const nestedDir = join(subDir, sub);
              const nestedEntries = await readdir(nestedDir).catch(() => []);
              for (const nested of nestedEntries) {
                if (nested.endsWith(".jsonl")) {
                  files.push({
                    path: join(nestedDir, nested),
                    project: project.replace(/-Users-fabioroma-Code-?/, "").replace(/-/g, "/"),
                    sessionId: basename(nested, ".jsonl"),
                  });
                }
              }
            }
          }
        }
      }
    }
  } catch (err) {
    console.error("Error scanning projects:", err.message);
  }

  return files;
}

// Aggregate all usage data
export async function getUsageData() {
  const jsonlFiles = await findJSONLFiles();
  const allMessages = [];

  for (const file of jsonlFiles) {
    const messages = await parseJSONL(file.path);
    for (const msg of messages) {
      msg.project = file.project;
    }
    allMessages.push(...messages);
  }

  // sort by timestamp
  allMessages.sort((a, b) => new Date(a.timestamp) - new Date(b.timestamp));

  // aggregate by day
  const dailyUsage = {};
  const modelUsage = {};
  const projectUsage = {};
  let totalInput = 0;
  let totalOutput = 0;
  let totalCacheRead = 0;
  let totalCacheCreation = 0;

  for (const msg of allMessages) {
    const day = msg.timestamp?.slice(0, 10) || "unknown";

    // daily aggregation
    if (!dailyUsage[day]) {
      dailyUsage[day] = { input: 0, output: 0, cacheRead: 0, cacheCreation: 0, messages: 0 };
    }
    dailyUsage[day].input += msg.inputTokens;
    dailyUsage[day].output += msg.outputTokens;
    dailyUsage[day].cacheRead += msg.cacheReadTokens;
    dailyUsage[day].cacheCreation += msg.cacheCreationTokens;
    dailyUsage[day].messages += 1;

    // model aggregation
    if (!modelUsage[msg.model]) {
      modelUsage[msg.model] = { input: 0, output: 0, cacheRead: 0, cacheCreation: 0, messages: 0 };
    }
    modelUsage[msg.model].input += msg.inputTokens;
    modelUsage[msg.model].output += msg.outputTokens;
    modelUsage[msg.model].cacheRead += msg.cacheReadTokens;
    modelUsage[msg.model].cacheCreation += msg.cacheCreationTokens;
    modelUsage[msg.model].messages += 1;

    // project aggregation
    if (!projectUsage[msg.project]) {
      projectUsage[msg.project] = { input: 0, output: 0, cacheRead: 0, cacheCreation: 0, messages: 0 };
    }
    projectUsage[msg.project].input += msg.inputTokens;
    projectUsage[msg.project].output += msg.outputTokens;
    projectUsage[msg.project].cacheRead += msg.cacheReadTokens;
    projectUsage[msg.project].cacheCreation += msg.cacheCreationTokens;
    projectUsage[msg.project].messages += 1;

    // totals
    totalInput += msg.inputTokens;
    totalOutput += msg.outputTokens;
    totalCacheRead += msg.cacheReadTokens;
    totalCacheCreation += msg.cacheCreationTokens;
  }

  // estimate cost (API pricing for reference, subscription = flat rate)
  const estimatedCost = estimateCost(modelUsage);

  return {
    totals: {
      inputTokens: totalInput,
      outputTokens: totalOutput,
      cacheReadTokens: totalCacheRead,
      cacheCreationTokens: totalCacheCreation,
      totalMessages: allMessages.length,
      totalSessions: new Set(allMessages.map((m) => m.sessionId)).size,
    },
    dailyUsage,
    modelUsage,
    projectUsage,
    estimatedCost,
    lastUpdated: new Date().toISOString(),
  };
}

// Estimate what this would cost at API rates
function estimateCost(modelUsage) {
  // pricing per 1M tokens (as of 2026)
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
    // find matching pricing (partial match)
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
