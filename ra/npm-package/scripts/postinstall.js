const { spawnSync } = require("node:child_process");
const fs = require("node:fs");
const https = require("node:https");
const os = require("node:os");
const path = require("node:path");

function getTargetTriple(platform, arch) {
  if (platform === "darwin" && arch === "x64") return "x86_64-apple-darwin";
  if (platform === "darwin" && arch === "arm64") return "aarch64-apple-darwin";
  if (platform === "linux" && arch === "x64") return "x86_64-unknown-linux-musl";
  if (platform === "linux" && arch === "arm64") return "aarch64-unknown-linux-musl";
  return null;
}

function download(url, destPath) {
  return new Promise((resolve, reject) => {
    const file = fs.createWriteStream(destPath);
    const request = https.get(
      url,
      {
        headers: {
          "User-Agent": "ra-cli-npm-installer",
          Accept: "application/octet-stream",
        },
      },
      (res) => {
        // Follow redirects.
        if (
          res.statusCode &&
          res.statusCode >= 300 &&
          res.statusCode < 400 &&
          res.headers.location
        ) {
          file.close();
          fs.unlinkSync(destPath);
          resolve(download(res.headers.location, destPath));
          return;
        }

        if (res.statusCode !== 200) {
          reject(
            new Error(
              `Download failed (${res.statusCode}) for ${url}${
                res.headers["content-type"]
                  ? `; content-type=${res.headers["content-type"]}`
                  : ""
              }`
            )
          );
          return;
        }

        res.pipe(file);
        file.on("finish", () => file.close(resolve));
      }
    );

    request.on("error", (err) => {
      try {
        file.close();
      } catch {
        // ignore
      }
      reject(err);
    });
  });
}

function findFileNamed(dir, filename) {
  /** @type {string[]} */
  const stack = [dir];
  while (stack.length > 0) {
    const current = stack.pop();
    if (!current) continue;
    for (const entry of fs.readdirSync(current, { withFileTypes: true })) {
      const full = path.join(current, entry.name);
      if (entry.isDirectory()) {
        stack.push(full);
      } else if (entry.isFile() && entry.name === filename) {
        return full;
      }
    }
  }
  return null;
}

async function main() {
  const pkg = require("../package.json");
  const version = pkg.version;

  const platform = os.platform();
  const arch = os.arch();
  const target = getTargetTriple(platform, arch);
  if (!target) {
    throw new Error(
      `Unsupported platform/arch for ra-cli npm package: ${platform}/${arch}`
    );
  }

  // Matches `.github/workflows/release.yml` asset naming: `archive: ra-${target}`.
  const tag = `v${version}`;
  const assetBase = `ra-${target}`;
  const candidates = [`${assetBase}.tar.gz`, `${assetBase}.zip`];

  const binDir = path.join(__dirname, "..", "bin");
  const finalBinPath = path.join(binDir, "ra");

  if (fs.existsSync(finalBinPath)) return;
  fs.mkdirSync(binDir, { recursive: true });

  const tmpDir = fs.mkdtempSync(path.join(os.tmpdir(), "ra-cli-"));
  try {
    let archivePath = null;
    let lastErr = null;

    for (const filename of candidates) {
      const url = `https://github.com/justinwangx/ra-cli/releases/download/${tag}/${filename}`;
      const dest = path.join(tmpDir, filename);
      try {
        // eslint-disable-next-line no-await-in-loop
        await download(url, dest);
        archivePath = dest;
        break;
      } catch (err) {
        lastErr = err;
      }
    }

    if (!archivePath) {
      throw lastErr ?? new Error("Failed to download ra release asset.");
    }

    const extractDir = path.join(tmpDir, "extract");
    fs.mkdirSync(extractDir, { recursive: true });

    if (archivePath.endsWith(".tar.gz")) {
      const r = spawnSync("tar", ["-xzf", archivePath, "-C", extractDir], {
        stdio: "inherit",
      });
      if (r.status !== 0) throw new Error("Failed to extract tarball.");
    } else if (archivePath.endsWith(".zip")) {
      const r = spawnSync("unzip", ["-q", archivePath, "-d", extractDir], {
        stdio: "inherit",
      });
      if (r.status !== 0) throw new Error("Failed to extract zip archive.");
    } else {
      throw new Error(`Unknown archive type: ${archivePath}`);
    }

    const extractedBin = findFileNamed(extractDir, "ra");
    if (!extractedBin) {
      throw new Error("Could not find extracted 'ra' binary in archive.");
    }

    fs.copyFileSync(extractedBin, finalBinPath);
    fs.chmodSync(finalBinPath, 0o755);
  } finally {
    try {
      fs.rmSync(tmpDir, { recursive: true, force: true });
    } catch {
      // ignore
    }
  }
}

main().catch((err) => {
  console.error(err && err.stack ? err.stack : String(err));
  process.exit(1);
});


