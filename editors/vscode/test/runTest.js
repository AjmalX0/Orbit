// Launches a real VS Code (downloaded by @vscode/test-electron) with this
// extension and runs test/suite against the fixture workspace.
//   ORBIT_BIN=/path/to/orbit xvfb-run -a npm test
const path = require("node:path");
const { runTests } = require("@vscode/test-electron");

async function main() {
  if (!process.env.ORBIT_BIN) throw new Error("set ORBIT_BIN to the orbit binary to test against");
  await runTests({
    extensionDevelopmentPath: path.resolve(__dirname, ".."),
    extensionTestsPath: path.resolve(__dirname, "suite", "index.js"),
    launchArgs: [path.resolve(__dirname, "fixture"), "--disable-extensions", "--disable-workspace-trust"],
    extensionTestsEnv: { ORBIT_BIN: process.env.ORBIT_BIN },
  });
}

main().catch((err) => {
  console.error(err);
  process.exit(1);
});
