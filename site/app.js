function singleQuote(value) {
  return "'" + value.replaceAll("'", "'\\''") + "'";
}

function powershellQuote(value) {
  return "'" + value.replaceAll("'", "''") + "'";
}

function updateCommands() {
  const origin = window.location.origin;
  if (origin === "https://unbind.dev") return;

  for (const command of document.querySelectorAll("[data-install]")) {
    const verify = command.hasAttribute("data-verify");
    if (command.dataset.install === "sh") {
      command.textContent = `curl -fsSL ${singleQuote(origin + "/install.sh")} | sh -s -- ${verify ? "--verify " : ""}${singleQuote(origin)}`;
    } else {
      command.textContent = `& ([scriptblock]::Create((irm ${powershellQuote(origin + "/install.ps1")}))) ${powershellQuote(origin)}${verify ? " -Verify" : ""}`;
    }
  }
}

function enableCopyButtons() {
  if (!navigator.clipboard) return;

  for (const button of document.querySelectorAll("[data-copy]")) {
    const command = button.parentElement.querySelector("code");
    const label = button.textContent;
    const status = document.createElement("span");
    status.className = "visually-hidden";
    status.setAttribute("role", "status");
    button.after(status);
    button.hidden = false;
    let resetTimer;
    button.addEventListener("click", async () => {
      window.clearTimeout(resetTimer);
      try {
        await navigator.clipboard.writeText(command.textContent);
        button.textContent = "Copied";
        status.textContent = "Command copied.";
      } catch {
        if (command.hidden) {
          button.textContent = "Copy from address bar";
          status.textContent = "Copy the invite from the address bar.";
        } else {
          button.textContent = "Select to copy";
          const range = document.createRange();
          range.selectNodeContents(command);
          const selection = window.getSelection();
          selection.removeAllRanges();
          selection.addRange(range);
          status.textContent = "Command selected. Use your keyboard shortcut to copy it.";
        }
      }
      resetTimer = window.setTimeout(() => {
        button.textContent = label;
        status.textContent = "";
      }, 2500);
    });
  }
}

function showJoinCommand() {
  const message = document.getElementById("join-message");
  if (!message) return;

  const terminal = document.getElementById("join-terminal");
  const command = document.getElementById("join-command");
  const setup = document.getElementById("join-setup");
  const cli = document.getElementById("join-cli");
  const open = document.getElementById("join-open");
  const appLink = document.getElementById("join-app-link");
  const copyLink = document.getElementById("join-link");
  const fragment = window.location.hash;
  const valid = /^#v1\.[A-Za-z0-9_-]{43}\.[A-Za-z0-9_-]{43}$/.test(fragment);

  terminal.hidden = !valid;
  setup.hidden = !valid;
  cli.hidden = !valid;
  open.hidden = !valid;
  if (valid) {
    const invite = new URL("/join", window.location.origin);
    invite.hash = fragment;
    const token = btoa(invite.href).replaceAll("+", "-").replaceAll("/", "_").replace(/=+$/, "");
    appLink.href = `unbind://connect#${token}`;
    copyLink.textContent = invite.href;
    command.textContent = `unbind connect ${singleQuote(invite.href)} --listen 8080`;
    message.textContent = "Open the invite on the computer you want to connect from.";
  } else {
    appLink.removeAttribute("href");
    copyLink.textContent = "";
    command.textContent = "";
    message.textContent = fragment
      ? "The invite is incomplete or invalid. Ask the host for the full share link."
      : "Open the full share link to connect.";
  }
}

async function showDownloads() {
  const list = document.getElementById("download-list");
  if (!list) return;

  const status = document.getElementById("download-status");
  const platforms = new Map([
    ["unbind-linux-amd64", "Linux · x86-64"],
    ["unbind-linux-arm64", "Linux · ARM64"],
    ["unbind-darwin-amd64", "macOS · Intel"],
    ["unbind-darwin-arm64", "macOS · Apple silicon"],
    ["unbind-windows-amd64.exe", "Windows · x86-64"],
    ["unbind-windows-arm64.exe", "Windows · ARM64"],
  ]);

  try {
    const response = await fetch("/downloads/SHA256SUMS", { cache: "no-cache" });
    if (!response.ok) throw new Error("Checksums are unavailable");
    const manifest = await response.text();
    const published = new Set();

    for (const line of manifest.split("\n")) {
      const match = /^[a-fA-F0-9]{64}\s+\*?(\S+)\s*$/.exec(line);
      if (match) published.add(match[1]);
    }

    for (const [filename, label] of platforms) {
      if (!published.has(filename)) continue;
      const item = document.createElement("li");
      const link = document.createElement("a");
      const name = document.createElement("span");
      link.href = `/downloads/${filename}`;
      link.download = filename;
      link.textContent = label;
      name.textContent = filename;
      link.append(name);
      item.append(link);
      list.append(item);
    }

    if (!list.children.length) throw new Error("No supported binaries are published");
    list.hidden = false;
    document.getElementById("download-checksums").hidden = false;
    status.hidden = true;
  } catch {
    status.textContent = "No executable downloads are available right now.";
  }
}

updateCommands();
enableCopyButtons();
showJoinCommand();
window.addEventListener("hashchange", showJoinCommand);
showDownloads();
