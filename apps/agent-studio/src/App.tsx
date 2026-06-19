import {
  FolderOpen,
  MoreHorizontal,
  Plus,
  RefreshCw,
  Terminal,
  Trash2,
  X
} from "lucide-react";
import { useEffect, useMemo, useState } from "react";
import {
  appConfig,
  createLane,
  listEnvs,
  openIde,
  openShell,
  pickSourceRoot,
  removeLane
} from "./api";
import type { EnvStatus, RuntimeOptions } from "./types";

const defaultRuntime: RuntimeOptions = {};

function App() {
  const [runtime] = useState(defaultRuntime);
  const [envs, setEnvs] = useState<EnvStatus[]>([]);
  const [selected, setSelected] = useState("");
  const [target, setTarget] = useState("");
  const [source, setSource] = useState("");
  const [status, setStatus] = useState("Starting");
  const [formMessage, setFormMessage] = useState("");
  const [creating, setCreating] = useState(false);
  const [selectingSource, setSelectingSource] = useState(false);
  const [actionMenuOpen, setActionMenuOpen] = useState(false);
  const [menuOpen, setMenuOpen] = useState(false);
  const [pendingRemove, setPendingRemove] = useState("");
  const [removing, setRemoving] = useState(false);
  const [removeMessage, setRemoveMessage] = useState("");

  const targetName = sanitizeWorldName(target);
  const targetExists = useMemo(
    () => envs.some((item) => item.env.id === targetName),
    [envs, targetName]
  );
  const selectedWorld = useMemo(
    () => envs.find((item) => item.env.id === selected),
    [envs, selected]
  );
  const selectedEnv = selectedWorld?.env;
  const selectedSourceRoot = selectedWorld?.source_root || "";
  const selectedEnvPath = selectedWorld?.env_path || "";

  useEffect(() => {
    void appConfig(runtime)
      .then((value) => setStatus(`${value.platform} / ${value.agentfs}`))
      .catch((error) => setStatus(String(error)));
    void refresh();
  }, [runtime]);

  useEffect(() => {
    if (!creating) {
      return;
    }
    const timer = window.setTimeout(() => {
      setStatus("Still creating; large roots can take a while");
    }, 5000);
    return () => window.clearTimeout(timer);
  }, [creating]);

  async function refresh(preferredSelection?: string) {
    setStatus("Refreshing");
    try {
      const response = await listEnvs(runtime);
      setEnvs(response.envs);
      const nextSelection = chooseSelection(response.envs, preferredSelection ?? selected);
      setSelected(nextSelection);
      setStatus(`${response.envs.length} worlds`);
    } catch (error) {
      setStatus(errorMessage(error));
    }
  }

  function selectWorld(envId: string) {
    setSelected(envId);
    setStatus(`World: ${envId}`);
  }

  async function startNewWorld() {
    if (selectingSource) {
      return;
    }
    setSelectingSource(true);
    setActionMenuOpen(false);
    setMenuOpen(false);
    setFormMessage("");
    setStatus("Choosing root folder");
    try {
      await nextFrame();
      const response = await pickSourceRoot(source || undefined);
      if (!response.path) {
        setStatus("Folder selection cancelled");
        return;
      }
      setSource(response.path);
      setTarget(suggestWorldName(response.path, envs));
      setMenuOpen(true);
      setStatus(`Selected ${response.path}`);
    } catch (error) {
      const message = errorMessage(error);
      setFormMessage(message);
      setStatus(message);
    } finally {
      setSelectingSource(false);
    }
  }

  async function create() {
    const nextSource = source.trim();
    const nextTarget = sanitizeWorldName(target);
    if (nextTarget !== target) {
      setTarget(nextTarget);
    }
    if (!nextSource) {
      setFormMessage("Choose a root folder first.");
      setStatus("Open a root folder first");
      return;
    }
    if (!nextTarget) {
      setFormMessage("Enter a world name.");
      setStatus("Enter a world name");
      return;
    }
    if (envs.some((item) => item.env.id === nextTarget)) {
      const message = `World "${nextTarget}" already exists.`;
      setFormMessage(message);
      setStatus(message);
      return;
    }
    if (creating) {
      return;
    }
    setFormMessage("");
    setCreating(true);
    setStatus(`Creating ${nextTarget}`);
    const startedAt = Date.now();
    try {
      await nextFrame();
      await createLane(runtime, { target: nextTarget, source: nextSource });
      const elapsed = Math.round((Date.now() - startedAt) / 1000);
      setMenuOpen(false);
      await refresh(nextTarget);
      setStatus(`Created ${nextTarget} in ${elapsed}s`);
    } catch (error) {
      const message = errorMessage(error);
      setFormMessage(message);
      setStatus(message);
    } finally {
      setCreating(false);
    }
  }

  async function withEnv(action: (envId: string) => Promise<unknown>) {
    if (!selected) {
      setStatus("Select a world");
      return;
    }
    try {
      const result = await action(selected);
      setStatus(JSON.stringify(result));
      await refresh();
    } catch (error) {
      setStatus(errorMessage(error));
    }
  }

  async function openNativeShell() {
    if (!selected) {
      setStatus("Select a world");
      return;
    }
    try {
      await openShell(runtime, selected);
      const message = `opened agentctl shell ${selected}`;
      setStatus(message);
    } catch (error) {
      setStatus(errorMessage(error));
    }
  }

  async function confirmRemove() {
    if (!pendingRemove || removing) {
      return;
    }
    const removingTarget = pendingRemove;
    setRemoving(true);
    setRemoveMessage("");
    try {
      await nextFrame();
      await removeLane(runtime, removingTarget);
      setPendingRemove("");
      await refresh("");
      setStatus(`Removed ${removingTarget}`);
    } catch (error) {
      const message = errorMessage(error);
      setRemoveMessage(message);
      setStatus(message);
    } finally {
      setRemoving(false);
    }
  }

  return (
    <main className="workbench">
      <aside className="activityBar">
        <button
          className={`activityButton ${
            actionMenuOpen || menuOpen || selectingSource ? "active" : ""
          }`}
          onClick={() => {
            if (selectingSource) {
              return;
            }
            setMenuOpen(false);
            setActionMenuOpen((value) => !value);
          }}
          disabled={selectingSource}
          title="Menu"
        >
          <MoreHorizontal size={22} />
        </button>
      </aside>

      {actionMenuOpen ? (
        <section className="actionMenu">
          <button className="actionMenuItem" onClick={() => void startNewWorld()} type="button">
            <Plus size={16} />
            <span>フォークの作成</span>
          </button>
        </section>
      ) : null}

      {menuOpen ? (
        <form
          className="newWorldMenu"
          onSubmit={(event) => {
            event.preventDefault();
            void create();
          }}
        >
          <header>
            <strong>New World</strong>
            <button
              className="iconButton ghost"
              onClick={() => setMenuOpen(false)}
              title="Close"
              type="button"
            >
              <X size={16} />
            </button>
          </header>
          <div className="rootSummary">
            <span>Root</span>
            <strong>{source}</strong>
          </div>
          <label>Name</label>
          <input
            value={target}
            onChange={(event) => {
              setTarget(event.target.value);
              setFormMessage("");
            }}
            autoFocus
          />
          {formMessage || targetExists ? (
            <div className={targetExists ? "formMessage warning" : "formMessage"}>
              {targetExists ? `World "${targetName}" already exists.` : formMessage}
            </div>
          ) : null}
          <div className="newWorldActions">
            <button
              className="primary wide"
              disabled={!source.trim() || !targetName || targetExists || creating}
              type="submit"
            >
              <Plus size={16} />
              {creating ? "Creating" : "Create"}
            </button>
          </div>
        </form>
      ) : null}

      <section className="worldPanel">
        <header className="panelHeader">
          <span>Worlds</span>
          <button className="iconButton ghost" onClick={() => void refresh()} title="Refresh">
            <RefreshCw size={15} />
          </button>
        </header>
        <div className="worldList">
          {envs.map(({ env, source_root }) => (
            <button
              key={env.id}
              className={`worldItem ${selected === env.id ? "selected" : ""}`}
              onClick={() => selectWorld(env.id)}
            >
              <span className="worldTopLine">
                <span className="worldName">{env.id}</span>
                <span className={`worldState ${env.state}`}>{env.state}</span>
              </span>
              <span className="worldPath">{source_root || env.rootfs_path}</span>
            </button>
          ))}
        </div>
      </section>

      <section className="mainArea">
        <header className="titleBar">
          <div>
            <strong>{selectedEnv ? selectedEnv.id : "No world selected"}</strong>
            <span>{selectedEnv ? selectedSourceRoot || selectedEnv.rootfs_path : status}</span>
          </div>
          <div className="toolbarActions">
            <button className="ghost" onClick={() => void refresh()} title="Refresh">
              <RefreshCw size={16} />
            </button>
            {selected ? (
              <button
                className="danger"
                onClick={() => {
                  setRemoveMessage("");
                  setPendingRemove(selected);
                }}
              >
                <Trash2 size={16} />
                Remove
              </button>
            ) : null}
          </div>
        </header>

        <section className="editorPane">
          {selectedEnv ? (
            <div className="worldDashboard">
              <div className="launchStrip">
                <button
                  className="launchButton"
                  onClick={() => void withEnv((id) => openIde(runtime, id, "reveal", ""))}
                >
                  <FolderOpen size={18} />
                  <span>File</span>
                </button>
                <button className="launchButton" onClick={() => void openNativeShell()}>
                  <Terminal size={18} />
                  <span>Terminal</span>
                </button>
              </div>

              <div className="metricStrip">
                <Metric label="Status" value={selectedEnv.state} />
                <Metric label="Backend" value={selectedEnv.backend} />
                <Metric label="Base" value={selectedEnv.base_id} />
                <Metric
                  label="Sessions"
                  value={selectedEnv.sessions.length ? selectedEnv.sessions.join(", ") : "-"}
                />
                <Metric label="Profile" value={selectedEnv.profile} />
                <Metric label="Created" value={formatDate(selectedEnv.created_at)} />
                <Metric label="Fork Root" value={selectedSourceRoot || "-"} wide />
                <Metric label="AgentFS Env" value={selectedEnvPath || "-"} wide />
                <Metric label="Rootfs" value={selectedEnv.rootfs_path} wide />
              </div>
            </div>
          ) : (
            <div className="emptyState">
              <MoreHorizontal size={24} />
              <span>Open the top-left menu to create a world.</span>
            </div>
          )}
        </section>
      </section>

      {pendingRemove ? (
        <div className="modalBackdrop" role="presentation">
          <section className="confirmDialog" role="dialog" aria-modal="true">
            <header>
              <strong>Remove World</strong>
              <button
                className="iconButton ghost"
                onClick={() => {
                  if (!removing) {
                    setPendingRemove("");
                  }
                }}
                title="Close"
                type="button"
                disabled={removing}
              >
                <X size={16} />
              </button>
            </header>
            <p>
              This removes the isolated world and its env files. Host source files are not removed.
            </p>
            <div className="removeSummary">
              <span>Name</span>
              <strong>{pendingRemove}</strong>
              <span>Root</span>
              <strong>
                {envs.find((item) => item.env.id === pendingRemove)?.env.rootfs_path || "-"}
              </strong>
            </div>
            {removeMessage ? <div className="formMessage">{removeMessage}</div> : null}
            <div className="confirmActions">
              <button
                className="ghost"
                onClick={() => setPendingRemove("")}
                type="button"
                disabled={removing}
              >
                Cancel
              </button>
              <button
                className="danger"
                onClick={() => void confirmRemove()}
                type="button"
                disabled={removing}
              >
                <Trash2 size={16} />
                {removing ? "Removing" : "Remove World"}
              </button>
            </div>
          </section>
        </div>
      ) : null}

      {creating ? (
        <div className="busyOverlay" role="alert" aria-live="assertive">
          <div className="busyPanel">
            <div className="spinner" aria-hidden="true" />
            <strong>Creating Fork</strong>
            <span>{sanitizeWorldName(target) || "world"}</span>
          </div>
        </div>
      ) : null}
    </main>
  );
}

function Metric({
  label,
  value,
  wide = false
}: {
  label: string;
  value: string;
  wide?: boolean;
}) {
  return (
    <div className={wide ? "metric wideMetric" : "metric"}>
      <span>{label}</span>
      <strong>{value}</strong>
    </div>
  );
}

function chooseSelection(envs: EnvStatus[], preferred?: string) {
  if (preferred && envs.some((item) => item.env.id === preferred)) {
    return preferred;
  }
  return envs[0]?.env.id || "";
}

function suggestWorldName(root: string, envs: EnvStatus[]) {
  const parts = root.split(/[\\/]+/).filter(Boolean);
  const base = sanitizeWorldName(parts[parts.length - 1] || "world") || "world";
  const used = new Set(envs.map((item) => item.env.id));
  for (let index = 1; index < 10_000; index += 1) {
    const candidate = `${base}-${index}`;
    if (!used.has(candidate)) {
      return candidate;
    }
  }
  return `${base}-${Date.now()}`;
}

function sanitizeWorldName(value: string) {
  return value
    .replace(/[^A-Za-z0-9]+/g, "-")
    .replace(/-+/g, "-")
    .replace(/^-+|-+$/g, "");
}

function formatDate(value: string) {
  const date = new Date(value);
  if (Number.isNaN(date.getTime())) {
    return value || "-";
  }
  return date.toLocaleString();
}

function errorMessage(error: unknown) {
  return error instanceof Error ? error.message : String(error);
}

function nextFrame() {
  return new Promise<void>((resolve) => {
    window.requestAnimationFrame(() => resolve());
  });
}

export default App;
