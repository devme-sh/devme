#!/usr/bin/env bun

// CLI name saturation checker
// Usage:
//   bun scripts/name-check.ts devstack toolbox myapp
//   echo "devstack\ntoolbox" | bun scripts/name-check.ts
//   bun scripts/name-check.ts --file candidates.txt

// ---------------------------------------------------------------------------
// ANSI colors
// ---------------------------------------------------------------------------

const RESET = "\x1b[0m";
const BOLD = "\x1b[1m";
const DIM = "\x1b[2m";
const RED = "\x1b[31m";
const GREEN = "\x1b[32m";
const YELLOW = "\x1b[33m";
const CYAN = "\x1b[36m";

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

interface CheckResult {
  name: string;
  score: number;
  github: { stars: number; fullName: string };
  npm: boolean; // true = available
  crates: boolean;
  pypi: boolean;
  homebrew: boolean;
  domains: Record<string, boolean>; // true = available
  cliCollision: boolean; // true = collision exists
}

// ---------------------------------------------------------------------------
// Rate limiter for GitHub API
// ---------------------------------------------------------------------------

class RateLimiter {
  private timestamps: number[] = [];

  constructor(
    private maxRequests: number,
    private windowMs: number,
  ) {}

  async acquire(): Promise<void> {
    const now = Date.now();
    this.timestamps = this.timestamps.filter((t) => now - t < this.windowMs);

    if (this.timestamps.length < this.maxRequests) {
      this.timestamps.push(Date.now());
      return;
    }

    const oldest = this.timestamps[0];
    const waitMs = this.windowMs - (now - oldest) + 100;
    await Bun.sleep(waitMs);
    return this.acquire();
  }
}

const GITHUB_TOKEN = process.env.GITHUB_TOKEN ?? "";
const GITHUB_HEADERS: Record<string, string> = {
  Accept: "application/vnd.github.v3+json",
  "User-Agent": "name-check-cli",
  ...(GITHUB_TOKEN ? { Authorization: `Bearer ${GITHUB_TOKEN}` } : {}),
};

const githubLimiter = new RateLimiter(GITHUB_TOKEN ? 30 : 10, 60_000);

// ---------------------------------------------------------------------------
// Checkers
// ---------------------------------------------------------------------------

async function checkGitHub(
  name: string,
): Promise<{ stars: number; fullName: string }> {
  try {
    const res = await fetch(
      `https://api.github.com/search/repositories?q=${encodeURIComponent(name)}+in:name&sort=stars&order=desc&per_page=1`,
      { headers: GITHUB_HEADERS },
    );
    if (res.status === 403 || res.status === 429) {
      return { stars: -1, fullName: "(rate limited)" };
    }
    if (!res.ok) return { stars: 0, fullName: "" };
    const data = await res.json();
    if (!data.items?.length) return { stars: 0, fullName: "" };
    const top = data.items[0];
    // Only count it as a match if the repo name is an exact (case-insensitive) match
    if ((top.name as string).toLowerCase() !== name.toLowerCase()) {
      return { stars: 0, fullName: "" };
    }
    return {
      stars: top.stargazers_count as number,
      fullName: top.full_name as string,
    };
  } catch {
    return { stars: -1, fullName: "(error)" };
  }
}

async function checkNpm(name: string): Promise<boolean> {
  try {
    const res = await fetch(
      `https://registry.npmjs.org/${encodeURIComponent(name)}`,
      { method: "HEAD" },
    );
    return res.status === 404;
  } catch {
    return true;
  }
}

async function checkCrates(name: string): Promise<boolean> {
  try {
    const res = await fetch(
      `https://crates.io/api/v1/crates/${encodeURIComponent(name)}`,
      { headers: { "User-Agent": "name-check-cli (github.com)" } },
    );
    return res.status === 404;
  } catch {
    return true;
  }
}

async function checkPyPI(name: string): Promise<boolean> {
  try {
    const res = await fetch(
      `https://pypi.org/pypi/${encodeURIComponent(name)}/json`,
      { method: "HEAD" },
    );
    return res.status === 404;
  } catch {
    return true;
  }
}

async function checkHomebrew(name: string): Promise<boolean> {
  try {
    const [formula, cask] = await Promise.all([
      fetch(
        `https://formulae.brew.sh/api/formula/${encodeURIComponent(name)}.json`,
        { method: "HEAD" },
      ),
      fetch(
        `https://formulae.brew.sh/api/cask/${encodeURIComponent(name)}.json`,
        { method: "HEAD" },
      ),
    ]);
    return formula.status === 404 && cask.status === 404;
  } catch {
    return true;
  }
}

async function checkDomain(domain: string): Promise<boolean> {
  try {
    const proc = Bun.spawn(["dig", "+short", "+time=2", "+tries=1", domain], {
      stdout: "pipe",
      stderr: "pipe",
    });
    const output = await new Response(proc.stdout).text();
    await proc.exited;
    return output.trim().length === 0;
  } catch {
    return true;
  }
}

async function checkDomains(name: string): Promise<Record<string, boolean>> {
  const tlds = ["dev", "sh", "io", "com"];
  const results = await Promise.all(
    tlds.map(async (tld) => {
      const available = await checkDomain(`${name}.${tld}`);
      return [tld, available] as const;
    }),
  );
  return Object.fromEntries(results);
}

async function checkCliCollision(name: string): Promise<boolean> {
  try {
    const proc = Bun.spawn(["which", name], {
      stdout: "pipe",
      stderr: "pipe",
    });
    const code = await proc.exited;
    return code === 0;
  } catch {
    return false;
  }
}

// ---------------------------------------------------------------------------
// Scoring
// ---------------------------------------------------------------------------

function computeScore(r: Omit<CheckResult, "score">): number {
  let s = 0;
  const stars = r.github.stars;
  if (stars > 1000) s += 15;
  else if (stars > 100) s += 7;
  else if (stars > 10) s += 3;
  else if (stars >= 1) s += 1;
  // stars < 0 means error/rate-limited, add nothing

  if (!r.npm) s += 3;
  if (!r.crates) s += 2;
  if (!r.pypi) s += 2;
  if (!r.homebrew) s += 5;
  for (const avail of Object.values(r.domains)) {
    if (!avail) s += 1;
  }
  if (r.cliCollision) s += 5;
  return s;
}

// ---------------------------------------------------------------------------
// Check a single name (all sources in parallel)
// ---------------------------------------------------------------------------

async function checkName(name: string): Promise<CheckResult> {
  const [github, npm, crates, pypi, homebrew, domains, cliCollision] =
    await Promise.all([
      checkGitHub(name),
      checkNpm(name),
      checkCrates(name),
      checkPyPI(name),
      checkHomebrew(name),
      checkDomains(name),
      checkCliCollision(name),
    ]);

  const partial = { name, github, npm, crates, pypi, homebrew, domains, cliCollision };
  return { ...partial, score: computeScore(partial) };
}

// ---------------------------------------------------------------------------
// Output formatting
// ---------------------------------------------------------------------------

function statusIcon(available: boolean): string {
  return available ? `${GREEN}✓${RESET}` : `${RED}✗${RESET}`;
}

function starsLabel(stars: number): string {
  if (stars < 0) return `${DIM}err${RESET}`;
  if (stars === 0) return `${GREEN}0${RESET}`;
  const str = stars >= 1000 ? `${(stars / 1000).toFixed(1)}k` : String(stars);
  if (stars > 1000) return `${RED}${BOLD}${str}${RESET}`;
  if (stars > 100) return `${RED}${str}${RESET}`;
  if (stars > 10) return `${YELLOW}${str}${RESET}`;
  return `${GREEN}${str}${RESET}`;
}

function scoreLabel(score: number): string {
  if (score <= 3) return `${GREEN}${BOLD}${score}${RESET}`;
  if (score <= 8) return `${YELLOW}${score}${RESET}`;
  if (score <= 15) return `${RED}${score}${RESET}`;
  return `${RED}${BOLD}${score}${RESET}`;
}

function domainSummary(domains: Record<string, boolean>): string {
  return Object.entries(domains)
    .map(([tld, avail]) => `${avail ? GREEN : RED}.${tld}${RESET}`)
    .join(" ");
}

function stripAnsi(s: string): string {
  return s.replace(/\x1b\[[0-9;]*m/g, "");
}

function padVisible(s: string, width: number): string {
  const visible = stripAnsi(s).length;
  return s + " ".repeat(Math.max(0, width - visible));
}

function printResults(results: CheckResult[]) {
  const sorted = [...results].sort((a, b) => a.score - b.score);
  const maxName = Math.max(12, ...sorted.map((r) => r.name.length));

  console.log(
    `\n${BOLD}${CYAN}  Name Saturation Check${RESET} ${DIM}(${results.length} candidate${results.length !== 1 ? "s" : ""})${RESET}\n`,
  );
  console.log(
    `${DIM}  Lower score = more available. 0 = pristine, 30+ = very saturated.${RESET}\n`,
  );

  // Header
  const hdr = [
    "Name".padEnd(maxName),
    "Sc".padStart(3),
    " GH★".padStart(6),
    " Len",
    " npm",
    " crt",
    " pyp",
    " brw",
    " cli",
    " Domains",
  ].join("  ");
  console.log(`  ${BOLD}${hdr}${RESET}`);
  console.log(`  ${"─".repeat(stripAnsi(hdr).length)}`);

  for (const r of sorted) {
    const cols = [
      `${BOLD}${r.name.padEnd(maxName)}${RESET}`,
      padVisible(scoreLabel(r.score), 3),
      padVisible(starsLabel(r.github.stars), 6),
      String(r.name.length).padStart(4),
      `  ${statusIcon(r.npm)} `,
      `  ${statusIcon(r.crates)} `,
      `  ${statusIcon(r.pypi)} `,
      `  ${statusIcon(r.homebrew)} `,
      `  ${statusIcon(!r.cliCollision)} `,
      ` ${domainSummary(r.domains)}`,
    ].join(" ");
    console.log(`  ${cols}`);
  }

  console.log(`\n${DIM}  ✓ = available   ✗ = taken   cli ✓ = no collision${RESET}`);
  console.log(
    `${DIM}  GH★: ${RESET}${GREEN}0-10${RESET}${DIM} clear  ${RESET}${YELLOW}11-100${RESET}${DIM} caution  ${RESET}${RED}101-1000${RESET}${DIM} taken  ${RESET}${RED}${BOLD}1000+${RESET}${DIM} hard no${RESET}`,
  );

  // Top picks summary
  const top = sorted.filter((r) => r.score <= 5);
  if (top.length > 0) {
    console.log(`\n${GREEN}${BOLD}  Top picks:${RESET}`);
    for (const r of top) {
      const gh =
        r.github.stars > 0
          ? ` ${DIM}(GitHub: ${r.github.fullName} ${r.github.stars}★)${RESET}`
          : "";
      console.log(`    ${BOLD}${r.name}${RESET} ${DIM}score ${r.score}${RESET}${gh}`);
    }
  }

  if (!GITHUB_TOKEN) {
    console.log(
      `\n${DIM}  Tip: Set GITHUB_TOKEN for higher GitHub API rate limits (30/min vs 10/min).${RESET}`,
    );
  }
  console.log();
}

// ---------------------------------------------------------------------------
// Input parsing
// ---------------------------------------------------------------------------

function parseLines(text: string): string[] {
  return text
    .split("\n")
    .map((l) => l.trim())
    .filter((l) => l.length > 0 && !l.startsWith("#"));
}

async function getNames(): Promise<string[]> {
  const args = process.argv.slice(2);

  if (args.includes("--help") || args.includes("-h")) {
    console.log(`
${BOLD}name-check${RESET} — CLI name saturation checker

${BOLD}Usage:${RESET}
  bun scripts/name-check.ts <name1> <name2> ...
  bun scripts/name-check.ts --file candidates.txt
  echo "name1\\nname2" | bun scripts/name-check.ts

${BOLD}Options:${RESET}
  --file <path>   Read candidate names from a file (one per line, # comments)
  --help, -h      Show this help

${BOLD}Environment:${RESET}
  GITHUB_TOKEN    GitHub personal access token for higher rate limits

${BOLD}Sources checked:${RESET}
  GitHub repos, npm, crates.io, PyPI, Homebrew, domains (.dev/.sh/.io/.com),
  CLI collision (which)

${BOLD}Scoring (lower = better):${RESET}
  GitHub stars:   0=0  1-10=1  11-100=3  101-1000=7  1000+=15
  npm taken:      3    crates.io taken: 2    PyPI taken: 2
  Homebrew taken: 5    Each domain taken: 1  CLI collision: 5
`);
    process.exit(0);
  }

  const names: string[] = [];

  // --file flag
  const fileIdx = args.indexOf("--file");
  if (fileIdx !== -1) {
    const filePath = args[fileIdx + 1];
    if (!filePath) {
      console.error(`${RED}Error: --file requires a path argument.${RESET}`);
      process.exit(1);
    }
    const content = await Bun.file(filePath).text();
    names.push(...parseLines(content));
    // Also collect any positional args
    for (let i = 0; i < args.length; i++) {
      if (i === fileIdx || i === fileIdx + 1) continue;
      if (!args[i].startsWith("--")) names.push(args[i]);
    }
    return [...new Set(names)];
  }

  // Positional args
  const positional = args.filter((a) => !a.startsWith("--"));
  if (positional.length > 0) {
    return [...new Set(positional)];
  }

  // Try stdin (works when piped, e.g. echo "foo\nbar" | bun script.ts)
  try {
    const text = await Bun.stdin.text();
    if (text.trim()) {
      return [...new Set(parseLines(text))];
    }
  } catch {
    // stdin not available or empty
  }

  return [];
}

// ---------------------------------------------------------------------------
// Main
// ---------------------------------------------------------------------------

async function main() {
  const names = await getNames();

  if (names.length === 0) {
    console.error(
      `${RED}Error: No candidate names provided.${RESET}\nRun with --help for usage information.`,
    );
    process.exit(1);
  }

  const total = names.length;
  console.log(`${DIM}Checking ${total} name${total !== 1 ? "s" : ""}...${RESET}`);

  const results: CheckResult[] = [];
  const batchSize = GITHUB_TOKEN ? 10 : 5;
  for (let i = 0; i < names.length; i += batchSize) {
    const batch = names.slice(i, i + batchSize);
    const batchResults = await Promise.all(batch.map((n) => checkName(n.toLowerCase())));
    results.push(...batchResults);
    process.stdout.write(`\r${DIM}  [${results.length}/${total}]${RESET}${"".padEnd(30)}`);
    if (i + batchSize < names.length) {
      await Bun.sleep(GITHUB_TOKEN ? 3000 : 8000);
    }
  }

  // Clear progress line
  process.stdout.write("\r" + " ".repeat(60) + "\r");

  printResults(results);
}

main().catch((err) => {
  console.error(`${RED}Fatal: ${err.message}${RESET}`);
  process.exit(1);
});
