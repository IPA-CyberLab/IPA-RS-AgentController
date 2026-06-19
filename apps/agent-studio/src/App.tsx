import {
  Code2,
  FolderOpen,
  GitCompare,
  Plus,
  RefreshCw,
  Trash2
} from "lucide-react";
import { useEffect, useMemo, useState } from "react";
import {
  appConfig,
  changedPaths,
  createLane,
  listEnvs,
  openIde,
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
  const [output, setOutput] = useState("");
  const [creating, setCreating] = useState(false);

  const selectedEnv = useMemo(
    () => envs.find((item) => item.env.id === selected)?.env,
    [envs, selected]
  );

  useEffect(() => {
    void appConfig(runtime)
      .then((value) => {
        setConfig(value);
        setStatus(`${value.platform} / ${value.agentfs}`);
      })
      .catch((error) => setStatus(String(error)));
    void refresh();
  }, [runtime]);

  async function refresh() {
    setStatus("Refreshing");
    try {
      const response = await listEnvs(runtime);
      setEnvs(response.envs);
      if (!selected && response.envs.length > 0) {
        setSelected(response.envs[0].env.id);
      }
      setStatus(`${response.envs.length} worlds`);
    } catch (error) {
      setStatus(String(error));
    }
  }

  function log(value: unknown) {
    const text =
      typeof value === "string" ? value : JSON.stringify(value, null, 2);
    setOutput((current) => `${text}\n${current}`);
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
      log(String(error));
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
      const result = await createLane(runtime, { target, source });
      const elapsed = Math.round((Date.now() - startedAt) / 1000);
      log(result);
      setStatus(`Created ${target} in ${elapsed}s`);
      setSelected(target);
      await refresh();
    } catch (error) {
      setStatus(String(error));
      log(String(error));
    } finally {
      setCreating(false);
    }
  }

  useEffect(() => {
    if (!creating) {
      return;
    }
    const timer = window.setTimeout(() => {
      setStatus("Still creating; large roots can take a while");
    }, 5000);
    return () => window.clearTimeout(timer);
  }, [creating]);

  async function withEnv(action: (envId: string) => Promise<unknown>) {
    if (!selected) {
      setStatus("Select a world");
      return;
    }
    try {
      log(await action(selected));
      await refresh();
    } catch (error) {
      setStatus(String(error));
      log(String(error));
    }
  }

  return (
    <main className="shell">
      <aside className="sidebar">
        <div className="brand">
          <div>
            <h1>Agent Studio</h1>
            <p>{config ? config.agentfs : status}</p>
          </div>
          <button className="iconButton" onClick={refresh} title="Refresh">
            <RefreshCw size={17} />
          </button>
        </div>

        <section className="pane">
          <h2>New World</h2>
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
      </aside>

      <section className="work">
        <header className="toolbar">
          <div>
            <h2>Worlds</h2>
            <span>{status}</span>
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
          </div>
        </header>

        <div className="tableWrap">
          <table>
            <thead>
              <tr>
                <th>World</th>
                <th>Backend</th>
                <th>State</th>
                <th>Root</th>
                <th></th>
              </tr>
            </thead>
            <tbody>
              {envs.map(({ env, disk_used }) => (
                <tr
                  key={env.id}
                  className={selected === env.id ? "selected" : ""}
                  onClick={() => setSelected(env.id)}
                >
                  <td>
                    <strong>{env.id}</strong>
                    <small>{env.base_id}</small>
                  </td>
                  <td>
                    <span className="pill">{env.backend}</span>
                    <small>{disk_used || "-"}</small>
                  </td>
                  <td>
                    <span className={`state ${env.state}`}>{env.state}</span>
                    <small>{env.sessions.length ? env.sessions.join(", ") : "-"}</small>
                  </td>
                  <td className="mono">{env.rootfs_path}</td>
                  <td>
                    <button
                      className="danger iconButton"
                      onClick={(event) => {
                        event.stopPropagation();
                        void removeLane(runtime, env.id)
                          .then(log)
                          .then(refresh)
                          .catch((error) => {
                            setStatus(String(error));
                            log(String(error));
                          });
                      }}
                      title="Remove"
                    >
                      <Trash2 size={16} />
                    </button>
                  </td>
                </tr>
              ))}
            </tbody>
          </table>
        </div>

        <footer className="details">
          <div>
            <strong>{selectedEnv ? selectedEnv.id : "No world selected"}</strong>
            <span>{selectedEnv ? selectedEnv.rootfs_path : ""}</span>
          </div>
          <pre>{output}</pre>
        </footer>
      </section>
    </main>
  );
}

function suggestWorldName(root: string) {
  const parts = root
    .split(/[\\/]+/)
    .filter(Boolean);
  const leaf = parts[parts.length - 1]
    ?.replace(/[^A-Za-z0-9_.-]+/g, "-")
    .replace(/^-+|-+$/g, "");
  return leaf ? `${leaf}-1` : "world-1";
}

export default App;
