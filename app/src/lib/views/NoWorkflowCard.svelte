<script lang="ts">
  import { open as openDialog } from "@tauri-apps/plugin-dialog";
  import { toCmdError, workflowOpen, workflowSave } from "../ipc";
  import { loadRecents } from "../recents";
  import { uiState } from "../state/ui.svelte";
  import { WORKFLOW_TEMPLATE } from "./editorHelpers";

  let path = $state("");
  let error = $state<string | null>(null);
  const recents = loadRecents(localStorage);

  function open(p: string): void {
    const trimmed = p.trim();
    if (trimmed !== "") uiState.editorPath = trimmed;
  }

  async function browse(): Promise<void> {
    const picked = await openDialog({
      multiple: false,
      filters: [{ name: "TOML workflow", extensions: ["toml"] }],
    });
    if (typeof picked === "string") open(picked);
  }

  async function newWorkflow(): Promise<void> {
    error = null;
    const dir = await openDialog({ directory: true });
    if (typeof dir !== "string") return;
    const file = `${dir}/tempo.toml`;
    try {
      await workflowOpen(file); // exists → open it, never overwrite (spec §5)
    } catch (e: unknown) {
      const err = toCmdError(e);
      if (err.code !== "not_found") { error = err.message; return; }
      try {
        await workflowSave(file, WORKFLOW_TEMPLATE);
      } catch (saveErr: unknown) {
        error = toCmdError(saveErr).message;
        return;
      }
    }
    open(file);
  }
</script>

<div class="card-wrap">
  <div class="card panel">
    <div class="label">CoreTempo</div>
    <p class="prose">Open a workflow file to begin, or create a new one.</p>
    <div class="row">
      <input
        class="mono"
        placeholder="/path/to/tempo.toml"
        bind:value={path}
        onkeydown={(ev) => {
          if (ev.key === "Enter") open(path);
        }}
      />
      <button class="mono go" onclick={() => open(path)}>Open / New</button>
    </div>
    <div class="row">
      <button
        class="mono go"
        onclick={() => {
          void browse();
        }}>Browse…</button
      >
      <button
        class="mono go"
        onclick={() => {
          void newWorkflow();
        }}>New workflow…</button
      >
    </div>
    {#if error !== null}<p class="err">{error}</p>{/if}
    {#if recents.length > 0}
      <div class="label">Recent</div>
      <ul class="recents">
        {#each recents as r (r)}
          <li><button class="mono recent" onclick={() => open(r)}>{r}</button></li>
        {/each}
      </ul>
    {/if}
  </div>
</div>

<style>
  .card-wrap { display: grid; place-items: center; height: 100%; }
  .card { width: 420px; padding: 20px; display: flex; flex-direction: column; gap: 10px; }
  .prose { color: var(--text-dim); }
  .row { display: flex; gap: 6px; }
  input {
    flex: 1; background: var(--terminal-bg); color: var(--text);
    border: 1px solid var(--panel-edge); padding: 4px 8px; user-select: text;
  }
  .go { color: var(--accent); border: 1px solid var(--panel-edge); padding: 4px 10px; }
  .err { color: var(--err); }
  .recents { list-style: none; display: flex; flex-direction: column; gap: 4px; }
  .recent { color: var(--info); text-align: left; }
</style>
