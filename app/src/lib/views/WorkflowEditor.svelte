<script lang="ts">
  import { toCmdError, workflowMerge, workflowOpen, workflowParse, workflowSave } from "../ipc";
  import { pushRecent } from "../recents";
  import { uiState } from "../state/ui.svelte";
  import type { ParseReport, WorkflowModel } from "../types";
  import { plannedRoster, WORKFLOW_TEMPLATE } from "./editorHelpers";
  import GraphCanvas from "./GraphCanvas.svelte";
  import Inspector from "./Inspector.svelte";

  const TOGGLE_BLOCKED = "fix the TOML before switching to the graph";

  let view = $state<"graph" | "toml">("graph");
  let text = $state(""); // last known on-disk-shaped toml; also the merge base
  let model = $state<WorkflowModel | null>(null);
  let selected = $state<string | null>(null);
  let loaded = $state(false);
  let report = $state<ParseReport | null>(null);
  let ioError = $state<string | null>(null);
  let toggleError = $state<string | null>(null);

  const planned = $derived(plannedRoster(text));
  const graph = $derived(view === "graph" ? model : null);

  function openWorkflow(path: string): void {
    workflowOpen(path)
      .then((t) => {
        bootFrom(t, path);
      })
      .catch((e: unknown) => {
        const err = toCmdError(e);
        // Seeding over a file that exists but cannot be read would destroy it.
        if (err.code === "not_found") bootFrom(WORKFLOW_TEMPLATE, path);
        else ioError = err.message;
      });
  }

  $effect(() => {
    const path = uiState.editorPath;
    if (path === null || loaded) return;
    openWorkflow(path);
  });

  function retryOpen(): void {
    const path = uiState.editorPath;
    if (path === null) return;
    ioError = null;
    openWorkflow(path);
  }

  function bootFrom(t: string, path: string): void {
    text = t;
    loaded = true;
    pushRecent(localStorage, path);
    workflowParse(t)
      .then((r) => {
        report = r;
        if (r.model === null) view = "toml"; // unparsable file opens raw, issues shown
        else model = r.model;
      })
      .catch((e: unknown) => {
        ioError = toCmdError(e).message;
      });
  }

  // Live validation for the raw view only: in the graph view `text` is the stale merge base.
  $effect(() => {
    const current = text;
    if (!loaded || view !== "toml") return;
    const timer = setTimeout(() => {
      workflowParse(current)
        .then((r) => {
          report = r;
        })
        .catch((e: unknown) => {
          ioError = toCmdError(e).message;
        });
    }, 300);
    return () => clearTimeout(timer);
  });

  function markDirty(): void {
    uiState.editorDirty = true;
  }

  // The flag gates ▶ Run; an editor that unmounts with edits pending must not
  // leave Run disabled with nothing left to save.
  $effect(() => () => {
    uiState.editorDirty = false;
  });

  async function showGraph(): Promise<void> {
    if (view === "graph") return; // already active; re-parsing would discard graph edits
    const r = await workflowParse(text);
    report = r;
    if (r.model === null) {
      toggleError = TOGGLE_BLOCKED;
      return;
    }
    model = r.model;
    if (selected !== null && !(selected in r.model.agents)) selected = null;
    toggleError = null;
    view = "graph";
  }

  async function showToml(): Promise<void> {
    if (view === "toml") return; // already active
    if (model !== null && uiState.editorDirty) {
      try {
        text = await workflowMerge(text, model);
      } catch (e: unknown) {
        ioError = toCmdError(e).message; // stay on the graph rather than show stale text
        return;
      }
    }
    ioError = null;
    toggleError = null;
    view = "toml";
  }

  async function save(): Promise<void> {
    const path = uiState.editorPath;
    if (path === null) return;
    ioError = null;
    try {
      if (view === "graph" && model !== null) {
        const merged = await workflowMerge(text, model);
        const check = await workflowParse(merged);
        report = check;
        if (!check.ok) return; // validation blocks the write
        await workflowSave(path, merged);
        text = merged;
      } else {
        await workflowSave(path, text);
      }
      uiState.editorDirty = false;
      pushRecent(localStorage, path);
    } catch (e: unknown) {
      ioError = toCmdError(e).message;
    }
  }
</script>

<div class="editor">
  <div class="toolbar mono">
    <span class="path">{uiState.editorPath}{uiState.editorDirty ? " •" : ""}</span>
    <div class="views">
      <button
        class:active={view === "graph"}
        onclick={() => {
          void showGraph();
        }}>graph</button
      >
      <button
        class:active={view === "toml"}
        onclick={() => {
          void showToml();
        }}>toml</button
      >
    </div>
    <button
      class="save"
      onclick={() => {
        void save();
      }}
      disabled={!loaded || !uiState.editorDirty}>Save</button
    >
  </div>
  <div class="body" class:graph={graph !== null}>
    {#if !loaded}
      <div class="load-error mono">
        {#if ioError !== null}
          <p class="err">{ioError}</p>
          <button onclick={retryOpen}>retry</button>
        {:else}
          <p class="dim">loading…</p>
        {/if}
      </div>
    {:else if graph !== null}
      <GraphCanvas model={graph} bind:selected onchanged={markDirty} />
      <Inspector model={graph} bind:selected onchanged={markDirty} />
    {:else}
      <textarea
        class="mono src"
        spellcheck="false"
        bind:value={text}
        oninput={() => {
          markDirty();
        }}
      ></textarea>
      <aside class="side">
        <div class="label">Planned roster</div>
        <ul class="mono roster">
          {#each planned as id (id)}
            <li><span class="glyph starting">◐</span> {id}</li>
          {:else}
            <li class="dim">no agents defined</li>
          {/each}
        </ul>
        <div class="label">Validation</div>
        {#if report === null}
          <p class="mono dim">validating…</p>
        {:else if report.ok}
          <p class="mono ok">✓ valid</p>
        {:else}
          <ul class="mono issues">
            {#each report.errors as issue (issue.path + issue.message)}
              <li><span class="err">{issue.path}</span> {issue.message}</li>
            {/each}
          </ul>
        {/if}
      </aside>
    {/if}
  </div>
  {#if (loaded && ioError !== null) || toggleError !== null || (graph !== null && report?.ok === false)}
    <footer class="problems mono">
      {#if loaded && ioError !== null}<p class="err">{ioError}</p>{/if}
      {#if toggleError !== null}<p class="err">{toggleError}</p>{/if}
      {#if graph !== null && report !== null && !report.ok}
        <ul class="issues">
          {#each report.errors as issue (issue.path + issue.message)}
            <li><span class="err">{issue.path}</span> {issue.message}</li>
          {/each}
        </ul>
      {/if}
    </footer>
  {/if}
</div>

<style>
  .editor { display: flex; flex-direction: column; height: 100%; background: var(--panel); }
  .toolbar {
    display: flex; justify-content: space-between; align-items: center; gap: 10px;
    padding: 4px 10px; border-bottom: 1px solid var(--panel-edge);
  }
  .path { flex: 1; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
  .views { display: flex; gap: 8px; }
  .views button { color: var(--text-dim); }
  .views button.active { color: var(--accent); }
  .save { color: var(--accent); }
  .body { display: grid; grid-template-columns: 1fr 220px; flex: 1; min-height: 0; }
  .body.graph { grid-template-columns: 1fr 260px; }
  .src {
    background: var(--terminal-bg); color: var(--text); border: none; outline: none;
    resize: none; padding: 10px; font-size: var(--fs-data); line-height: 1.5;
    user-select: text;
  }
  .side {
    border-left: 1px solid var(--panel-edge); padding: 10px;
    display: flex; flex-direction: column; gap: 6px; overflow-y: auto;
  }
  .load-error {
    grid-column: 1 / -1; display: flex; flex-direction: column; align-items: flex-start;
    gap: 10px; padding: 16px;
  }
  .problems {
    border-top: 1px solid var(--panel-edge); padding: 6px 10px;
    display: flex; flex-direction: column; gap: 4px; max-height: 120px; overflow-y: auto;
  }
  .roster { list-style: none; }
  .issues { list-style: none; display: flex; flex-direction: column; gap: 4px; }
  .ok { color: var(--ok); }
  .err { color: var(--err); }
  .dim { color: var(--text-dim); }
</style>
