#!/usr/bin/env node
"use strict";
// Validates skills: extracts code blocks from SKILL.md files and runs them against
// the demo trace. Also validates scripts/ files load correctly and schema docs match
// the actual runtime objects.

const fs = require("fs");
const path = require("path");

const skillsDir = path.resolve(__dirname, "..", "skills");
const demoPath = path.join(__dirname, "demo-trace.bin");

// Parse markdown: extract ```javascript blocks with their heading
function extractRecipes(md, filename) {
  const recipes = [];
  let currentHeading = "(preamble)";
  let inBlock = false;
  let block = "";

  for (const line of md.split("\n")) {
    if (line.startsWith("## ")) {
      currentHeading = line.slice(3).trim();
    } else if (line.startsWith("```javascript")) {
      inBlock = true;
      block = "";
    } else if (line.startsWith("```") && inBlock) {
      inBlock = false;
      recipes.push({ heading: `${filename}: ${currentHeading}`, code: block });
    } else if (inBlock) {
      block += line + "\n";
    }
  }
  return recipes;
}

// Skip blocks that aren't runnable
function shouldSkip(recipe) {
  const code = recipe.code.trim();
  if (code.includes("{ ... }") || code === "..." || code === "") return true;
  if (recipe.heading.includes("Setup boilerplate")) return true;
  if (recipe.heading.includes("Working with large directories")) return true;
  // Skip pure structure/type definitions
  if (/^\{\s*\n\s*(events|workerSpans|eventType|timestamp):/.test(code)) return true;
  // Skip S3 examples (need a running server)
  if (code.includes("localhost:3000")) return true;
  return false;
}

// Replace placeholder values with real ones so examples are runnable
function fixPlaceholders(code, tracePath) {
  return code
    .replace(/['"]\/path\/to\/traces?\/['"]/g, JSON.stringify(tracePath))
    .replace(/['"]\/path\/to\/trace\.bin['"]/g, JSON.stringify(tracePath))
    .replace(/['"]trace\.bin['"]/g, JSON.stringify(tracePath));
}

// Walk skills/ subdirectories and collect all SKILL.md files
function collectSkillMds() {
  const results = [];
  for (const entry of fs.readdirSync(skillsDir, { withFileTypes: true })) {
    if (!entry.isDirectory()) continue;
    const skillMd = path.join(skillsDir, entry.name, "SKILL.md");
    if (fs.existsSync(skillMd)) {
      results.push({ name: entry.name, path: skillMd });
    }
  }
  return results.sort((a, b) => a.name.localeCompare(b.name));
}

async function main() {
  const skillMds = collectSkillMds();

  // Collect recipes from all SKILL.md files
  let allRecipes = [];
  for (const { name, path: mdPath } of skillMds) {
    const md = fs.readFileSync(mdPath, "utf8");
    allRecipes.push(...extractRecipes(md, name));
  }

  console.log(`Found ${allRecipes.length} code blocks across ${skillMds.length} skill SKILL.md files\n`);

  const { parseTrace, EVENT_TYPES, formatFrame, symbolizeChain, deduplicateSamples } = require("./trace_parser.js");
  const { buildWorkerSpans, attachCpuSamples, buildActiveTaskTimeline,
          computeSchedulingDelays, filterPointsOfInterest, buildFgData,
          buildSpanData } = require("./trace_analysis.js");

  // Create a temp directory for directory-mode testing
  const os = require("os");
  const testDir = fs.mkdtempSync(path.join(os.tmpdir(), "d9-recipe-test-"));
  fs.copyFileSync(demoPath, path.join(testDir, "t1.bin"));
  fs.copyFileSync(demoPath, path.join(testDir, "t2.bin"));

  const inputs = [
    { label: "file", path: demoPath },
    { label: "dir", path: testDir },
  ];

  let passed = 0;
  let failed = 0;
  let skipped = 0;

  // Resolve analyze.js from the toolkit skill scripts
  const analyzeJsPath = path.join(skillsDir, "dial9-toolkit", "scripts", "analyze.js");

  for (const input of inputs) {
    console.log(`── ${input.label}: ${input.path} ──\n`);

    // Run the prelude to get the variables every recipe expects
    let trace, workerIds, minTs, maxTs, spans, schedDelays, taskTimeline;
    for await (const t of parseTrace(input.path)) {
      trace = t;
      workerIds = [...new Set(
        trace.events.filter(e => e.eventType !== EVENT_TYPES.QueueSample && e.eventType !== EVENT_TYPES.WakeEvent)
          .map(e => e.workerId)
      )].sort((a, b) => a - b);
      maxTs = trace.events.reduce((m, e) => Math.max(m, e.timestamp), -Infinity);
      minTs = trace.events.reduce((m, e) => Math.min(m, e.timestamp), Infinity);
      spans = buildWorkerSpans(trace.events, workerIds, maxTs);
      attachCpuSamples(trace.cpuSamples, spans.workerSpans);
      taskTimeline = buildActiveTaskTimeline(trace.taskSpawnTimes, trace.taskTerminateTimes);
      schedDelays = computeSchedulingDelays(spans.workerSpans, workerIds, spans.wakesByTask);
      break; // use first trace for prelude variables
    }

    // Context: all variables available to code blocks
    const { analyzeTraces } = require(analyzeJsPath);
    const ctx = {
      trace, workerIds, minTs, maxTs, spans, schedDelays, taskTimeline,
      EVENT_TYPES, formatFrame, symbolizeChain, deduplicateSamples,
      buildWorkerSpans, attachCpuSamples, buildActiveTaskTimeline,
      computeSchedulingDelays, filterPointsOfInterest, buildFgData, buildSpanData,
      require, console, parseTrace, fs, path,
      event: trace.events[0],
      sample: trace.cpuSamples[0] || {},
      tracePath: input.path,
      analyzeTraces,
    };
    const ctxNames = Object.keys(ctx);
    const ctxValues = Object.values(ctx);

  for (const recipe of allRecipes) {
    if (shouldSkip(recipe)) {
      if (input === inputs[0]) skipped++;
      continue;
    }

    const origLog = console.log;
    const logs = [];
    console.log = (...args) => logs.push(args.join(" "));

    try {
      // Strip require() lines (already provided via context) and
      // convert const redeclarations of context vars to assignments
      const cleanCode = recipe.code
        .split("\n")
        .filter(line => !line.match(/^\s*const\s*\{.*\}\s*=\s*require\(/))
        .map(line => {
          for (const v of ctxNames) {
            if (new RegExp(`^(\\s*)const\\s+${v}\\s*=`).test(line))
              return line.replace(/const\s+/, '');
          }
          return line;
        })
        .join("\n");

      const body = `return (async () => { ${fixPlaceholders(cleanCode, input.path)} })();`;
      const fn = new Function(...ctxNames, body);
      await fn(...ctxValues);
      console.log = origLog;
      passed++;
    } catch (err) {
      console.log = origLog;
      origLog(`✗ [${input.label}] ${recipe.heading}: ${err.message}`);
      failed++;
    }
  }
  } // end inputs loop

  fs.rmSync(testDir, { recursive: true, force: true });

  // ── Script validation ──
  // Verify that scripts/ files in each skill load without errors
  console.log(`\n── Script validation ──\n`);
  for (const { name } of skillMds) {
    const scriptsDir = path.join(skillsDir, name, "scripts");
    if (!fs.existsSync(scriptsDir)) continue;
    for (const f of fs.readdirSync(scriptsDir).filter(f => f.endsWith(".js"))) {
      const scriptPath = path.join(scriptsDir, f);
      try {
        require(scriptPath);
        console.log(`✓ ${name}/scripts/${f} loads`);
        passed++;
      } catch (err) {
        console.log(`✗ ${name}/scripts/${f}: ${err.message}`);
        failed++;
      }
    }
  }

  // ── Schema validation helpers ──
  function validateSchema(schemaBlock, actualObj) {
    const topLevelErrors = [];
    const deepErrors = [];

    const actualKeys = new Set(Object.keys(actualObj));
    const topKeyMatches = schemaBlock.match(/^ {2}(\w+):/gm);
    if (!topKeyMatches) { topLevelErrors.push('no top-level keys found in schema'); return { topLevelErrors, deepErrors, docKeyCount: 0 }; }
    const docKeys = new Set(topKeyMatches.map(m => m.trim().replace(/:$/, '')));
    for (const k of actualKeys) { if (!docKeys.has(k)) topLevelErrors.push(`'${k}' in result but not documented`); }
    for (const k of docKeys) { if (!actualKeys.has(k)) topLevelErrors.push(`'${k}' documented but missing from result`); }

    let schemaJs = schemaBlock
      .replace(/\/\/.*$/gm, '')
      .replace(/\[\w+\]:/g, '_dynamic_:')
      .replace(/:\s*Map<[^,]+,\s*([^>]+)>/g, (_, valType) => {
        const v = valType.trim();
        const objMatch = v.match(/\{([^}]+)\}/);
        if (objMatch) {
          const keys = objMatch[1].split(',').map(k => k.trim()).filter(Boolean);
          return ': {"_map_":{' + keys.map(k => `"${k}":"_any_"`).join(',') + '}}';
        }
        const t = v.replace(/\|null$/, '');
        return ': {"_map_":"' + t + '"}';
      })
      .replace(/:\s*(Histogram)(\|null)?/g, (_, __, n) => ': "Histogram' + (n ? '|null' : '') + '"')
      .replace(/:\s*(number\[\])/g, ': "number[]"')
      .replace(/:\s*(number)(\|null)?/g, (_, __, n) => ': "number' + (n ? '|null' : '') + '"')
      .replace(/:\s*(string)(\|null)?/g, (_, __, n) => ': "string' + (n ? '|null' : '') + '"')
      .replace(/:\s*(boolean)/g, ': "boolean"')
      .replace(/:\s*(\w+)\[\]/g, ': "unknown[]"')
      .replace(/\[\{([^}]+)\}\]/g, (_, inner) => {
        const keys = inner.split(',').map(k => k.trim()).filter(Boolean);
        return '[{' + keys.map(k => `"${k}":"_any_"`).join(',') + '}]';
      });
    let docSkeleton;
    try { docSkeleton = (new Function('return {' + schemaJs + '}'))(); }
    catch (e) { deepErrors.push(`schema parse failed: ${e.message}`); }

    function toSkeleton(val) {
      if (val === null || val === undefined) return '_null_';
      if (val instanceof Map) {
        const first = val.values().next().value;
        return { '_map_': first !== undefined ? toSkeleton(first) : '_empty_' };
      }
      if (typeof val === 'object' && typeof val.percentile === 'function') return 'Histogram';
      if (Array.isArray(val)) {
        if (val.length === 0) return '[]';
        if (typeof val[0] === 'number') return 'number[]';
        if (typeof val[0] === 'object' && val[0] !== null) return [toSkeleton(val[0])];
        return 'unknown[]';
      }
      if (typeof val === 'number') return 'number';
      if (typeof val === 'string') return 'string';
      if (typeof val === 'boolean') return 'boolean';
      const out = {};
      for (const [k, v] of Object.entries(val)) out[k] = toSkeleton(v);
      return out;
    }

    if (docSkeleton) {
      const actualSkeleton = toSkeleton(actualObj);
      function diff(doc, actual, p) {
        if (typeof doc === 'object' && doc !== null && !Array.isArray(doc) && '_dynamic_' in doc) {
          if (typeof actual !== 'object' || actual === null) { deepErrors.push(`${p}: expected object with dynamic keys`); return; }
          const firstVal = Object.values(actual)[0];
          if (firstVal !== undefined) diff(doc._dynamic_, firstVal, p + '[*]');
          return;
        }
        if (typeof doc === 'string' && typeof actual === 'string') {
          if (doc === '_any_' || actual === doc) return;
          if (actual === '_empty_') return;
          if (doc.endsWith('|null') && (actual === doc.replace('|null', '') || actual === '_null_')) return;
          if (doc === 'number[]' && actual === '[]') return;
          deepErrors.push(`${p}: type mismatch (documented: ${doc}, actual: ${actual})`);
          return;
        }
        if (doc === '_any_') return;
        if (doc === 'unknown[]') return;
        if (Array.isArray(doc) && Array.isArray(actual)) {
          if (doc.length > 0 && actual.length > 0) diff(doc[0], actual[0], p + '[0]');
          return;
        }
        if (typeof doc === 'object' && doc !== null && typeof actual === 'object' && actual !== null) {
          const dk = new Set(Object.keys(doc)), ak = new Set(Object.keys(actual));
          for (const k of dk) { if (!ak.has(k)) deepErrors.push(`${p}.${k}: documented but missing from result`); }
          for (const k of ak) { if (!dk.has(k)) deepErrors.push(`${p}.${k}: in result but not documented`); }
          for (const k of dk) { if (ak.has(k)) diff(doc[k], actual[k], p + '.' + k); }
          return;
        }
        if (typeof doc !== typeof actual) deepErrors.push(`${p}: shape mismatch (documented: ${typeof doc}, actual: ${typeof actual})`);
      }
      diff(docSkeleton, actualSkeleton, '');
    }
    return { topLevelErrors, deepErrors, docKeyCount: docKeys.size };
  }

  function runSchemaCheck(label, skillName, headingRegex, actualObj) {
    const mdPath = path.join(skillsDir, skillName, "SKILL.md");
    const md = fs.readFileSync(mdPath, "utf8");
    const match = md.match(headingRegex);
    if (!match) { console.log(`✗ ${label}: could not find schema block in ${skillName}/SKILL.md`); failed++; return; }
    const { topLevelErrors, deepErrors, docKeyCount } = validateSchema(match[1], actualObj);
    if (topLevelErrors.length > 0) {
      for (const e of topLevelErrors) console.log(`✗ ${label} sync: ${e}`);
      failed++;
    } else {
      console.log(`✓ ${label} sync: ${skillName}/SKILL.md matches (${docKeyCount} keys)`);
      passed++;
    }
    if (deepErrors.length > 0) {
      for (const e of deepErrors) console.log(`✗ ${label} deep: ${e}`);
      failed++;
    } else {
      console.log(`✓ ${label} deep: nested shapes and types match`);
      passed++;
    }
  }

  // ── analyzeTraces schema (dial9-trace-analysis) ──
  const { analyzeTraces: at } = require(analyzeJsPath);
  const analyzeResult = await at(demoPath);
  runSchemaCheck('analyzeTraces', 'dial9-trace-analysis', /## analyzeTraces return schema[\s\S]*?```\n\{([\s\S]*?)\n\}[\s\S]*?```/, analyzeResult);

  // ── ParsedTrace schema (dial9-trace-loading) ──
  let parsedTrace;
  for await (const t of require("./trace_parser.js").parseTrace(demoPath)) { parsedTrace = t; break; }
  runSchemaCheck('ParsedTrace', 'dial9-trace-loading', /## ParsedTrace structure[\s\S]*?```\n\{([\s\S]*?)\n\}[\s\S]*?```/, parsedTrace);

  const unique = allRecipes.filter(r => !shouldSkip(r)).length;
  console.log(`\n${failed === 0 ? "✓" : "✗"} ${unique} snippets × ${inputs.length} modes: ${passed} passed, ${failed} failed, ${skipped} skipped`);
  if (failed > 0) process.exit(1);
}

main().catch(err => { console.error(err); process.exit(1); });
