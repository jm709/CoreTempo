<script lang="ts">
  import { open } from "@tauri-apps/plugin-dialog";
  import { untrack } from "svelte";
  import { projectRegister, sessionCreate, toCmdError } from "../ipcSessions";
  import { selectSession, sessionsState } from "../state/sessions.svelte";
  import { buildCreateRequest } from "./modalHelpers";

  let { project, onClose }: { project: string; onClose: () => void } = $props();

  // A fresh modal instance per open (the {#if} in App.svelte remounts it), so
  // seeding the form from the initial prop value is deliberate, not stale state.
  let form = $state({
    project: untrack(() => project), worktree: true, cwd: "", title: "", prompt: "",
    model: "", permissionMode: "default" as "default" | "bypassPermissions",
    isolatedConfig: false,
  });
  let submitting = $state(false);
  let error = $state<string | null>(null);
  let registerError = $state<string | null>(null);

  async function registerProject(): Promise<void> {
    registerError = null;
    const path = await open({ directory: true });
    if (path === null) return;
    try {
      const registered = await projectRegister(path);
      form.project = registered.id;
    } catch (e) {
      registerError = toCmdError(e).message;
    }
  }

  async function submit(): Promise<void> {
    submitting = true;
    error = null;
    try {
      const created = await sessionCreate(buildCreateRequest(form));
      selectSession(created.id);
      onClose();
    } catch (e) {
      error = toCmdError(e).message;
    } finally {
      submitting = false;
    }
  }
</script>

<form
  class="create"
  onsubmit={(ev) => {
    ev.preventDefault();
    void submit();
  }}
>
  <div class="label">New session</div>
  {#if sessionsState.projects.length === 0}
    <p class="mono hint">register a project first</p>
    <button
      type="button"
      class="mono"
      onclick={() => {
        void registerProject();
      }}
    >
      + project
    </button>
    {#if registerError !== null}<p class="mono err">{registerError}</p>{/if}
  {:else}
    <label class="field">
      <span class="mono key">project</span>
      <select class="mono" bind:value={form.project}>
        {#each sessionsState.projects as p (p.id)}
          <option value={p.id}>{p.name}</option>
        {/each}
      </select>
    </label>
    <label class="field row">
      <input type="checkbox" bind:checked={form.worktree} />
      <span class="mono key">worktree</span>
    </label>
    <label class="field">
      <span class="mono key">cwd</span>
      <input class="mono" bind:value={form.cwd} placeholder="(project root)" />
    </label>
    <label class="field">
      <span class="mono key">title</span>
      <input class="mono" bind:value={form.title} />
    </label>
    <label class="field">
      <span class="mono key">prompt</span>
      <textarea class="mono" rows="4" bind:value={form.prompt}></textarea>
    </label>
    <details>
      <summary class="mono">advanced</summary>
      <label class="field">
        <span class="mono key">model</span>
        <input class="mono" bind:value={form.model} placeholder="(default)" />
      </label>
      <label class="field">
        <span class="mono key">permission mode</span>
        <select class="mono" bind:value={form.permissionMode}>
          <option value="default">default</option>
          <option value="bypassPermissions">bypassPermissions</option>
        </select>
      </label>
      <label class="field row">
        <input type="checkbox" bind:checked={form.isolatedConfig} />
        <span class="mono key">isolated config</span>
      </label>
    </details>
    {#if error !== null}<p class="mono err">{error}</p>{/if}
    <div class="actions">
      <button type="button" class="mono" onclick={onClose}>cancel</button>
      <button
        type="submit"
        class="mono"
        disabled={submitting || form.project === ""}
      >
        create
      </button>
    </div>
  {/if}
</form>

<style>
  .create { display: flex; flex-direction: column; gap: 8px; }
  .field { display: flex; flex-direction: column; gap: 2px; }
  .field.row { flex-direction: row; align-items: center; gap: 6px; }
  .key { color: var(--text-dim); }
  input, textarea, select {
    background: var(--terminal-bg); color: var(--text);
    border: 1px solid var(--panel-edge); padding: 2px 4px; width: 100%;
  }
  input[type="checkbox"] { width: auto; }
  textarea { resize: vertical; }
  .hint { color: var(--text-dim); }
  .err { color: var(--err); white-space: pre-wrap; }
  .actions { display: flex; justify-content: flex-end; gap: 8px; }
  .actions button { color: var(--accent); }
</style>
