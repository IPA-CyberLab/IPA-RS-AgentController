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
  const [creating, setCreating] = useState(false);
  const [menuOpen, setMenuOpen] = useState(false);

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

  async function refresh() {
    setStatus("Refreshing");
    try {
      const response = await listEnvs(runtime);
      setEnvs(response.envs);
      if (!selected && response.envs.length > 0) {
        selectWorld(response.envs[0].env.id);
      }
      setStatus(`${response.envs.length} worlds`);
    } catch (error) {
      setStatus(String(error));
    }
  }

  function selectWorld(envId: string) {
    setSelected(envId);
    setStatus(`World: ${envId}`);
  }

  function updateSource(value: string) {
    setSource(value);
    if (!target.trim()) {
      setTarget(suggestWorldName(value));
    }
  }

  async function create() {
    if (!source.trim()) {
      setStatus("Open a root folder first");
      return;
    }
    if (!target.trim()) {
      setStatus("Enter a world name");
      return;
    }
    if (creating) {
      return;
    }
    setCreating(true);
    setStatus(`Creating ${target}`);
    const startedAt = Date.now();
    try {
      await createLane(runtime, { target, source });
      const elapsed = Math.round((Date.now() - startedAt) / 1000);
      setStatus(`Created ${target} in ${elapsed}s`);
      selectWorld(target);
      setMenuOpen(false);
      await refresh();
    } catch (error) {
      setStatus(String(error));
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
      setStatus(String(error));
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
      setStatus(String(error));
    }
  }

  return (
    <main className="workbench">
      <aside className="activityBar">
        <button
          className={`activityButton ${menuOpen ? "active" : ""}`}
          onClick={() => setMenuOpen((value) => !value)}
          title="New World"
        >
          <MoreHorizontal size={22} />
        </button>
      </aside>

      {menuOpen ? (
        <section className="newWorldMenu">
          <header>
            <strong>New World</strong>
            <button className="iconButton ghost" onClick={() => setMenuOpen(false)} title="Close">
              <X size={16} />
            </button>
          </header>
          <label>Root</label>
          <input
            value={source}
            onChange={(event) => updateSource(event.target.value)}
            placeholder="/Users/mizuame/Desktop/script/project"
          />
          <label>Name</label>
          <input value={target} onChange={(event) => setTarget(event.target.value)} />
          <button
            className="primary wide"
            onClick={create}
            disabled={!source.trim() || !target.trim() || creating}
          >
            <Plus size={16} />
            {creating ? "Creating" : "Create"}
          </button>
        </section>
      ) : null}

      <section className="worldPanel">
        <header className="panelHeader">
          <span>Worlds</span>
          <button className="iconButton ghost" onClick={refresh} title="Refresh">
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
            <button className="ghost" onClick={refresh} title="Refresh">
              <RefreshCw size={16} />
            </button>
            {selected ? (
              <button
                className="danger"
                onClick={() =>
                  void removeLane(runtime, selected)
                    .then(() => refresh())
                    .then(() => setSelected(""))
                    .catch((error) => setStatus(String(error)))
                }
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

function suggestWorldName(root: string) {
  const parts = root.split(/[\\/]+/).filter(Boolean);
  const leaf = parts[parts.length - 1]
    ?.replace(/[^A-Za-z0-9_.-]+/g, "-")
    .replace(/^-+|-+$/g, "");
  return leaf ? `${leaf}-1` : "world-1";
}

function formatDate(value: string) {
  const date = new Date(value);
  if (Number.isNaN(date.getTime())) {
    return value || "-";
  }
  return date.toLocaleString();
}

export default App;
