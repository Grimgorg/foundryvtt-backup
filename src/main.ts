import { listen } from "@tauri-apps/api/event";
import { getVersion } from "@tauri-apps/api/app";
import { ask, open } from "@tauri-apps/plugin-dialog";
import { api, BackupInfo, Overview, ProgressEvent, Settings, Status, WorldEntry } from "./api";

const $ = <T extends HTMLElement = HTMLElement>(id: string) => document.getElementById(id) as T;

let settings: Settings = { backupDir: null, worldsDir: null, keepLast: 10 };
let overview: Overview | null = null;
let busy = false;
const expanded = new Set<string>();

// ---------- formatting ----------

const dateFmt = new Intl.DateTimeFormat(undefined, { dateStyle: "medium", timeStyle: "short" });
const relFmt = new Intl.RelativeTimeFormat(undefined, { numeric: "auto" });

function fmtDate(iso: string | null | undefined): string {
  if (!iso) return "—";
  const d = new Date(iso);
  return isNaN(d.getTime()) ? iso : dateFmt.format(d);
}

function fmtRelative(iso: string | null | undefined): string {
  if (!iso) return "";
  const diff = (new Date(iso).getTime() - Date.now()) / 1000;
  if (isNaN(diff)) return "";
  const units: [Intl.RelativeTimeFormatUnit, number][] = [
    ["year", 31536000],
    ["month", 2592000],
    ["week", 604800],
    ["day", 86400],
    ["hour", 3600],
    ["minute", 60],
  ];
  for (const [unit, secs] of units) {
    if (Math.abs(diff) >= secs) return relFmt.format(Math.round(diff / secs), unit);
  }
  return relFmt.format(0, "minute");
}

function fmtSize(bytes: number): string {
  const units = ["B", "KB", "MB", "GB", "TB"];
  let i = 0;
  let n = bytes;
  while (n >= 1024 && i < units.length - 1) {
    n /= 1024;
    i++;
  }
  return `${n.toFixed(n < 10 && i > 0 ? 1 : 0)} ${units[i]}`;
}

function esc(s: string | null | undefined): string {
  return (s ?? "").replace(/[&<>"']/g, (c) =>
    ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" })[c]!,
  );
}

const STATUS: Record<Status, { label: string; cls: string; hint: string }> = {
  synced: { label: "Up to date", cls: "ok", hint: "The local world matches the latest backup." },
  localNewer: { label: "Local changes", cls: "warn", hint: "The world changed on this PC since the last backup. Back it up." },
  backupNewer: { label: "Newer backup", cls: "info", hint: "A newer backup exists (e.g. from another PC). Restore it to update this PC." },
  conflict: { label: "Conflict", cls: "danger", hint: "This PC has changes AND there is a newer backup from elsewhere. Decide which one to keep." },
  onlyInBackup: { label: "Not on this PC", cls: "info", hint: "This world only exists in the backup folder. Restore it to get it on this PC." },
  noBackup: { label: "No backup yet", cls: "warn", hint: "This world has never been backed up." },
};

// ---------- toasts & progress ----------

function toast(msg: string, kind: "ok" | "error" | "info" = "info") {
  const el = document.createElement("div");
  el.className = `toast ${kind}`;
  el.textContent = msg;
  $("toasts").appendChild(el);
  setTimeout(() => el.classList.add("hide"), kind === "error" ? 9000 : 4000);
  setTimeout(() => el.remove(), kind === "error" ? 9500 : 4500);
  el.addEventListener("click", () => el.remove());
}

function errText(e: unknown): string {
  return typeof e === "string" ? e : e instanceof Error ? e.message : JSON.stringify(e);
}

function showProgress(label: string, fraction: number | null) {
  $("progress").hidden = false;
  $("progress-label").textContent = label;
  const fill = $("progress-fill");
  fill.classList.toggle("indeterminate", fraction === null);
  fill.style.width = fraction === null ? "30%" : `${Math.round(fraction * 100)}%`;
}

function hideProgress() {
  $("progress").hidden = true;
}

function setBusy(b: boolean) {
  busy = b;
  document.body.classList.toggle("busy", b);
  document.querySelectorAll<HTMLButtonElement>("button[data-act]:not([data-act=toggle]), #btn-backup-all, #btn-refresh").forEach((btn) => {
    btn.disabled = b || btn.dataset.disabled === "1";
  });
}

// ---------- settings ----------

function renderSettings() {
  $<HTMLInputElement>("backup-dir").value = settings.backupDir ?? "";
  $<HTMLInputElement>("worlds-dir").value = settings.worldsDir ?? "";
  $<HTMLInputElement>("keep-last").value = String(settings.keepLast);
  $<HTMLButtonElement>("btn-open-backup").disabled = !settings.backupDir;
  $<HTMLButtonElement>("btn-open-worlds").disabled = !settings.worldsDir;
}

async function updateSettings(patch: Partial<Settings>) {
  try {
    settings = await api.saveSettings({ ...settings, ...patch });
    renderSettings();
    await refresh();
  } catch (e) {
    toast(`Could not save settings: ${errText(e)}`, "error");
  }
}

async function chooseFolder(current: string | null, title: string): Promise<string | null> {
  const picked = await open({ directory: true, multiple: false, title, defaultPath: current ?? undefined });
  return typeof picked === "string" ? picked : null;
}

// ---------- world list ----------

function backupLine(b: BackupInfo | undefined): string {
  if (!b) return `<span class="muted">No backups yet</span>`;
  return `${esc(fmtDate(b.created))} <span class="muted">(${esc(fmtRelative(b.created))})</span>
    <span class="muted">· from <b>${esc(b.host ?? "unknown PC")}</b> · ${fmtSize(b.size)}</span>`;
}

function worldCard(w: WorldEntry): string {
  const st = STATUS[w.status];
  const latest = w.backups[0];
  const inUse = w.local?.inUse ?? false;
  const canBackup = !!w.local && !inUse && !!overview?.backupDirOk;
  const canRestore = !!latest && !inUse && !!overview?.worldsDirOk;
  const restoreFirst = w.status === "backupNewer" || w.status === "onlyInBackup";
  const isOpen = expanded.has(w.id);
  const sub = [w.id, w.system && `${w.system}${w.systemVersion ? " " + w.systemVersion : ""}`, w.coreVersion && `Foundry ${w.coreVersion}`]
    .filter(Boolean)
    .map((s) => esc(s as string))
    .join(" · ");

  const local = w.local
    ? `${esc(fmtDate(w.local.modified))} <span class="muted">(${esc(fmtRelative(w.local.modified))}) · ${fmtSize(w.local.size)}</span>`
    : `<span class="muted">Not on this PC</span>`;

  const rows = w.backups
    .map(
      (b, i) => `<tr>
        <td>${esc(fmtDate(b.created))}${i === 0 ? ' <span class="tag">latest</span>' : ""}</td>
        <td class="muted">${esc(fmtRelative(b.created))}</td>
        <td>${esc(b.host ?? "—")}</td>
        <td class="num">${fmtSize(b.size)}</td>
        <td class="num"><button class="btn small" data-act="restore" data-world="${esc(w.id)}" data-file="${esc(b.fileName)}"
          ${canRestore ? "" : 'data-disabled="1" disabled'}>Restore</button></td>
      </tr>`,
    )
    .join("");

  return `<article class="card world ${isOpen ? "open" : ""}" data-world="${esc(w.id)}">
    <div class="world-head">
      <div class="world-title">
        <h2>${esc(w.title)}</h2>
        <p class="muted small">${sub}</p>
      </div>
      <div class="badges">
        ${inUse ? `<span class="badge danger" title="Foundry has this world open. Return to setup in Foundry to back up or restore.">Open in Foundry</span>` : ""}
        <span class="badge ${st.cls}" title="${esc(st.hint)}${w.statusGuessed ? " (estimated from file dates)" : ""}">${st.label}${w.statusGuessed ? "?" : ""}</span>
      </div>
    </div>
    <dl class="facts">
      <dt>This PC</dt><dd>${local}</dd>
      <dt>Latest backup</dt><dd>${backupLine(latest)}</dd>
    </dl>
    <div class="actions">
      <button class="btn ${restoreFirst ? "" : "primary"}" data-act="backup" data-world="${esc(w.id)}"
        ${canBackup ? "" : 'data-disabled="1" disabled'}>Back up now</button>
      <button class="btn ${restoreFirst ? "primary" : ""}" data-act="restore" data-world="${esc(w.id)}" data-file="${esc(latest?.fileName)}"
        ${canRestore ? "" : 'data-disabled="1" disabled'}>Restore latest</button>
      <div class="spacer"></div>
      ${w.backups.length ? `<button class="btn ghost" data-act="toggle" data-world="${esc(w.id)}" aria-expanded="${isOpen}">
        ${isOpen ? "Hide" : "Show"} all backups (${w.backups.length})</button>` : ""}
    </div>
    ${isOpen && w.backups.length ? `<div class="backups"><table>
      <thead><tr><th>Date</th><th></th><th>From PC</th><th class="num">Size</th><th></th></tr></thead>
      <tbody>${rows}</tbody></table></div>` : ""}
  </article>`;
}

function renderWorlds() {
  const root = $("worlds");
  if (!overview) {
    root.innerHTML = `<div class="empty muted">Loading…</div>`;
    return;
  }
  $("host-line").textContent = `This PC: ${overview.host}`;
  const notes: string[] = [];
  if (!overview.backupDirOk) notes.push("Choose a <b>backup folder</b> (for example a folder in Dropbox, OneDrive, iCloud Drive, Google Drive or Nextcloud).");
  if (!overview.worldsDirOk) notes.push("Choose your <b>Foundry worlds folder</b> or click <b>Auto-detect</b>.");
  const noteHtml = notes.length ? `<div class="card note">${notes.map((n) => `<p>${n}</p>`).join("")}</div>` : "";
  const list = overview.worlds.length
    ? overview.worlds.map(worldCard).join("")
    : overview.worldsDirOk || overview.backupDirOk
      ? `<div class="empty muted">No worlds found.</div>`
      : "";
  root.innerHTML = noteHtml + list;
  setBusy(busy);
}

async function refresh() {
  try {
    overview = await api.listWorlds();
  } catch (e) {
    toast(`Could not read worlds: ${errText(e)}`, "error");
  }
  renderWorlds();
}

function findWorld(id: string) {
  return overview?.worlds.find((w) => w.id === id);
}

// ---------- actions ----------

async function doBackup(id: string): Promise<boolean> {
  const w = findWorld(id);
  showProgress(`Backing up “${w?.title ?? id}”…`, null);
  try {
    const b = await api.backupWorld(id);
    toast(`Backed up “${w?.title ?? id}” (${fmtSize(b.size)}).`, "ok");
    return true;
  } catch (e) {
    toast(`Backup of “${w?.title ?? id}” failed: ${errText(e)}`, "error");
    return false;
  }
}

async function backupOne(id: string) {
  if (busy) return;
  setBusy(true);
  await doBackup(id);
  hideProgress();
  setBusy(false);
  await refresh();
}

async function backupAll() {
  if (busy || !overview) return;
  const targets = overview.worlds.filter((w) => w.local);
  const skipped = targets.filter((w) => w.local!.inUse);
  const todo = targets.filter((w) => !w.local!.inUse);
  if (!todo.length) {
    toast(skipped.length ? "All worlds are open in Foundry — nothing to back up." : "No local worlds to back up.", "info");
    return;
  }
  setBusy(true);
  let ok = 0;
  for (const w of todo) if (await doBackup(w.id)) ok++;
  hideProgress();
  setBusy(false);
  const skipNote = skipped.length ? ` Skipped (open in Foundry): ${skipped.map((w) => w.title).join(", ")}.` : "";
  toast(`Backed up ${ok} of ${todo.length} world(s).${skipNote}`, ok === todo.length ? "ok" : "error");
  await refresh();
}

async function restore(id: string, fileName: string) {
  if (busy) return;
  const w = findWorld(id);
  const b = w?.backups.find((x) => x.fileName === fileName);
  if (!w || !b) return;
  const lines = [
    w.local
      ? `Replace the world “${w.title}” on this PC with the backup from ${fmtDate(b.created)} (${b.host ?? "unknown PC"})?`
      : `Restore “${w.title}” to this PC from the backup of ${fmtDate(b.created)} (${b.host ?? "unknown PC"})?`,
  ];
  if (w.status === "localNewer" || w.status === "conflict" || w.status === "noBackup") {
    lines.push("", "⚠️ This PC has changes that are not in any backup. They will be replaced.");
  }
  if (w.local) lines.push("", "A safety copy of the current world is saved locally first.");
  const yes = await ask(lines.join("\n"), { title: "Restore world", kind: "warning", okLabel: "Restore", cancelLabel: "Cancel" });
  if (!yes) return;

  setBusy(true);
  showProgress(`Restoring “${w.title}”…`, null);
  try {
    await api.restoreWorld(id, fileName);
    toast(`Restored “${w.title}” from ${fmtDate(b.created)}.`, "ok");
  } catch (e) {
    toast(`Restore failed: ${errText(e)}`, "error");
  }
  hideProgress();
  setBusy(false);
  await refresh();
}

// ---------- wiring ----------

function wire() {
  $("btn-refresh").addEventListener("click", () => !busy && refresh());
  $("btn-backup-all").addEventListener("click", backupAll);

  $("btn-choose-backup").addEventListener("click", async () => {
    const p = await chooseFolder(settings.backupDir, "Choose the backup (cloud) folder");
    if (p) await updateSettings({ backupDir: p });
  });
  $("btn-choose-worlds").addEventListener("click", async () => {
    const p = await chooseFolder(settings.worldsDir, "Choose Foundry's Data/worlds folder");
    if (p) await updateSettings({ worldsDir: p });
  });
  $("btn-detect-worlds").addEventListener("click", async () => {
    const p = await api.detectWorldsDir();
    if (p) {
      await updateSettings({ worldsDir: p });
      toast(`Found Foundry worlds at ${p}`, "ok");
    } else {
      toast("Could not find Foundry's worlds folder. Please choose it manually.", "error");
    }
  });
  $("btn-open-backup").addEventListener("click", () => settings.backupDir && api.openFolder(settings.backupDir).catch((e) => toast(errText(e), "error")));
  $("btn-open-worlds").addEventListener("click", () => settings.worldsDir && api.openFolder(settings.worldsDir).catch((e) => toast(errText(e), "error")));
  $("keep-last").addEventListener("change", (ev) => {
    const n = Math.max(0, Math.min(999, Math.floor(Number((ev.target as HTMLInputElement).value) || 0)));
    updateSettings({ keepLast: n });
  });
  $("link-safety").addEventListener("click", async (ev) => {
    ev.preventDefault();
    const dir = await api.safetyDir();
    api.openFolder(dir).catch(() => toast(`No safety copies yet. They will be stored in ${dir}`, "info"));
  });

  $("worlds").addEventListener("click", (ev) => {
    const btn = (ev.target as HTMLElement).closest<HTMLButtonElement>("button[data-act]");
    if (!btn || btn.disabled) return;
    const id = btn.dataset.world!;
    switch (btn.dataset.act) {
      case "backup":
        backupOne(id);
        break;
      case "restore":
        restore(id, btn.dataset.file!);
        break;
      case "toggle":
        expanded.has(id) ? expanded.delete(id) : expanded.add(id);
        renderWorlds();
        break;
    }
  });

  // Cloud sync may have delivered new backups while the window was in the background.
  let lastFocusRefresh = 0;
  window.addEventListener("focus", () => {
    if (!busy && Date.now() - lastFocusRefresh > 5000) {
      lastFocusRefresh = Date.now();
      refresh();
    }
  });

  listen<ProgressEvent>("progress", ({ payload: p }) => {
    const w = findWorld(p.worldId);
    const verb = p.op === "backup" ? "Backing up" : "Restoring";
    showProgress(`${verb} “${w?.title ?? p.worldId}”… ${p.done} / ${p.total} files`, p.total ? p.done / p.total : null);
  });
}

async function init() {
  wire();
  renderWorlds();
  getVersion().then((v) => ($("version").textContent = `Version ${v}`));
  try {
    settings = await api.getSettings();
  } catch (e) {
    toast(`Could not load settings: ${errText(e)}`, "error");
  }
  renderSettings();
  await refresh();
}

init();
