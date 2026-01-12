const fs = require("fs");
const path = require("path");

function formatAttempt(label, basePath, err) {
  const suffix = err ? `: ${err.message ?? String(err)}` : "";
  return `- ${label} (${basePath})${suffix}`;
}

function tryRequireTypeScript(label, dir, attempts) {
  const basePath = path.resolve(dir, "node_modules", "typescript");
  try {
    return require(basePath);
  } catch (err) {
    attempts.push(formatAttempt(label, basePath, err));
    return null;
  }
}

function loadTypeScript() {
  const attempts = [];

  const envDir = process.env.TYPECHECK_TS_HARNESS_TYPESCRIPT_DIR;
  if (envDir) {
    const loaded = tryRequireTypeScript("env TYPECHECK_TS_HARNESS_TYPESCRIPT_DIR", envDir, attempts);
    if (loaded) {
      return loaded;
    }
  }

  const harnessRoot = path.resolve(__dirname, "..");
  const harnessPkg = path.join(harnessRoot, "package.json");
  if (fs.existsSync(harnessPkg)) {
    const loaded = tryRequireTypeScript("typecheck-ts-harness/package.json", harnessRoot, attempts);
    if (loaded) {
      return loaded;
    }
  } else {
    attempts.push(formatAttempt("typecheck-ts-harness/package.json missing", harnessPkg, null));
  }

  const help = [
    "Cannot load the TypeScript compiler (`typescript` npm package).",
    "",
    "Install it locally (recommended):",
    "  cd typecheck-ts-harness && npm ci",
    "",
    "Or point the harness at an existing install:",
    "  export TYPECHECK_TS_HARNESS_TYPESCRIPT_DIR=/path/to/dir/with/node_modules",
    "",
    "Note: for deterministic difftsc/conformance output, the harness does not fall back",
    "to a globally-installed `typescript` package. Install it locally or use the env var.",
    "",
    "Load attempts:",
    ...attempts,
  ].join("\n");

  throw new Error(help);
}

module.exports = { loadTypeScript };
