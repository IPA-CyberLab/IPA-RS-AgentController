import {
  Code2,
  FolderOpen,
  GitCompare,
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
  changedPaths,
  createLane,
  listEnvs,
  openIde,
  openShell,
  pickSourceRoot,
  removeLane
} from "./api";
import type { EnvStatus, RuntimeOptions, StudioConfig } from "./types";

const defaultRuntime: RuntimeOptions = {};

function App() {
  const [runtime] = useState(defaultRuntime);
  const [config, setConfig] = useState<StudioConfig | null>(null);
  const [envs, setEnvs] = useState<EnvStatus[]>([]);
  const [selected, setSelected] = useState("");
  const [target, setTarget] = useState("");
  const [source, setSource] = useState("");
  const [status, setStatus] = useState("Starting");
  const [creating, setCreating] = useState(false);
  const [menuOpen, setMenuOpen] = useState(false);
  const [shellNotes, setShellNotes] = useState<Record<string, string>>({});

  const selectedWorld = useMemo(
    () => envs.find((item) => item.env.id === selected),
    [envs, selected]
  );
  const selectedEnv = selectedWorld?.env;
  const selectedSourceRoot = selectedWorld?.source_root || "";
  const selectedEnvPath = selectedWorld?.env_path || "";
  const selectedShellNote = selected ? shellNotes[selected] || "" : "";

  useEffect(() => {
    void appConfig(runtime)
      .then((value) => {
        setConfig(value);
        setStatus(`${value.platform} / ${value.agentfs}`);
      })
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
    setStatus(`Shell: ${envId}`);
  }

  function noteShell(envId: string, text: string) {
    setShellNotes((current) => ({
      ...current,
      [envId]: text
    }));
  }

  async function chooseRoot() {
    setStatus("Opening folder");
    try {
      const picked = await pickSourceRoot(runtime, source);
      if (picked) {
        setSource(picked);
        setTarget(suggestWorldName(picked));
        setStatus(picked);
      } else {
        setStatus("Folder selection cancelled");
      }
    } catch (error) {
      setStatus(String(error));
      if (selected) {
        noteShell(selected, String(error));
      }
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
      noteShell(target, `created ${target} in ${elapsed}s`);
      setStatus(`Created ${target} in ${elapsed}s`);
      selectWorld(target);
      setMenuOpen(false);
      await refresh();
    } catch (error) {
      setStatus(String(error));
      noteShell(target || "system", String(error));
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
      noteShell(selected, JSON.stringify(result, null, 2));
      await refresh();
    } catch (error) {
      setStatus(String(error));
      noteShell(selected, String(error));
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
      noteShell(selected, message);
    } catch (error) {
      setStatus(String(error));
      noteShell(selected, String(error));
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
          <button className={source ? "wide" : "primary wide"} onClick={chooseRoot}>
            <FolderOpen size={16} />
            {source ? "Change Folder" : "Open Folder"}
          </button>
          {source ? (
            <>
              <div className="rootSummary">
                <span>Root</span>
                <strong>{source}</strong>
              </div>
              <label>Name</label>
              <input value={target} onChange={(event) => setTarget(event.target.value)} />
              <button
                className="primary wide"
                onClick={create}
                disabled={!target.trim() || creating}
              >
                <Plus size={16} />
                {creating ? "Creating" : "Create"}
              </button>
            </>
          ) : null}
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
            <button onClick={() => void withEnv((id) => openIde(runtime, id, "code", ""))}>
              <Code2 size={16} />
              VSCode
            </button>
            <button onClick={() => void withEnv((id) => openIde(runtime, id, "cursor", ""))}>
              <Code2 size={16} />
              Cursor
            </button>
            <button onClick={() => void withEnv((id) => openIde(runtime, id, "reveal", ""))}>
              <FolderOpen size={16} />
              Folder
            </button>
            <button onClick={() => void withEnv((id) => changedPaths(runtime, id))}>
              <GitCompare size={16} />
              Changed
            </button>
            <button onClick={() => void openNativeShell()}>
              <Terminal size={16} />
              Shell
            </button>
            {selected ? (
              <button
                className="danger"
                onClick={() =>
                  void removeLane(runtime, selected)
                    .then(() => refresh())
                    .then(() => setSelected(""))
                    .catch((error) => noteShell(selected, String(error)))
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
            <div className="detailPanel">
              <header>
                <strong>Details</strong>
                <span>docker ps style</span>
              </header>
              <div className="detailTableWrap">
                <table className="detailTable">
                  <thead>
                    <tr>
                      <th>Name</th>
                      <th>Status</th>
                      <th>Backend</th>
                      <th>Base</th>
                      <th>Fork Root</th>
                      <th>AgentFS Env</th>
                      <th>Rootfs</th>
                      <th>Profile</th>
                      <th>Created</th>
                      <th>Sessions</th>
                    </tr>
                  </thead>
                  <tbody>
                    <tr>
                      <td className="strongCell">{selectedEnv.id}</td>
                      <td>
                        <span className={`worldState ${selectedEnv.state}`}>
                          {selectedEnv.state}
                        </span>
                      </td>
                      <td>{selectedEnv.backend}</td>
                      <td className="monoCell">{selectedEnv.base_id}</td>
                      <td className="pathCell">{selectedSourceRoot || "-"}</td>
                      <td className="pathCell">{selectedEnvPath || "-"}</td>
                      <td className="pathCell">{selectedEnv.rootfs_path}</td>
                      <td>{selectedEnv.profile}</td>
                      <td>{formatDate(selectedEnv.created_at)}</td>
                      <td>{selectedEnv.sessions.length ? selectedEnv.sessions.join(", ") : "-"}</td>
                    </tr>
                  </tbody>
                </table>
              </div>
            </div>
          ) : (
            <div className="emptyState">
              <MoreHorizontal size={24} />
              <span>Open the top-left menu to create a world.</span>
            </div>
          )}
        </section>

        <section className="terminalPane">
          <header>
            <div>
              <Terminal size={15} />
              <strong>Shell</strong>
              <span>{selectedEnv ? `agentctl shell ${selectedEnv.id}` : "Select a world"}</span>
            </div>
          </header>
          <div className="nativeShell">
            <div>
              <strong>{selectedEnv ? selectedEnv.id : "No world selected"}</strong>
              <span>
                {selectedEnv
                  ? "Opens the existing agentctl shell in your terminal."
                  : "Select a world from the left panel."}
              </span>
            </div>
            <button className="primary" onClick={() => void openNativeShell()} disabled={!selectedEnv}>
              <Terminal size={16} />
              Open Shell
            </button>
            <pre>{selectedShellNote}</pre>
          </div>
        </section>
      </section>
    </main>
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
