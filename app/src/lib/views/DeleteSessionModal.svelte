<script lang="ts">
  import { untrack } from "svelte";
  import { sessionDelete, toCmdError } from "../ipcSessions";
  import type { DeleteSessionResponse, SessionView } from "../types";

  let { session, onClose }: { session: SessionView; onClose: () => void } = $props();

  let phase = $state<"confirm" | "dirty" | "kept">("confirm");
  // A fresh modal instance per open (the {#if} in App.svelte remounts it), so
  // seeding the checkbox default from the initial prop is deliberate.
  let removeWorktree = $state(untrack(() => session.worktree !== null));
  let dirtyMessage = $state("");
  let error = $state<string | null>(null);
  let submitting = $state(false);
  let keptBranch = $state("");

  function onDeleted(res: DeleteSessionResponse): void {
    if (res.branch_kept) {
      keptBranch = session.worktree?.branch ?? "";
      phase = "kept";
    } else {
      onClose();
    }
  }

  async function run(remove: boolean, force: boolean): Promise<void> {
    submitting = true;
    error = null;
    try {
      onDeleted(await sessionDelete(session.id, remove, force));
    } catch (e) {
      const err = toCmdError(e);
      if (err.code === "dirty_worktree") {
        dirtyMessage = err.message;
        phase = "dirty";
      } else {
        error = err.message;
      }
    } finally {
      submitting = false;
    }
  }
</script>

<div class="delete mono">
  {#if phase === "confirm"}
    <div class="label">Delete session</div>
    <p>{session.title}</p>
    {#if session.worktree !== null}
      <label class="field row">
        <input type="checkbox" bind:checked={removeWorktree} />
        <span class="key">remove worktree</span>
      </label>
    {/if}
    {#if error !== null}<p class="err">{error}</p>{/if}
    <div class="actions">
      <button type="button" onclick={onClose}>cancel</button>
      <button
        type="button"
        class="danger"
        disabled={submitting}
        onclick={() => {
          void run(removeWorktree, false);
        }}
      >
        delete
      </button>
    </div>
  {:else if phase === "dirty"}
    <div class="label">Worktree has uncommitted work</div>
    <p class="msg">{dirtyMessage}</p>
    {#if error !== null}<p class="err">{error}</p>{/if}
    <div class="actions">
      <button
        type="button"
        disabled={submitting}
        onclick={() => {
          void run(false, true);
        }}
      >
        keep
      </button>
      <button
        type="button"
        class="danger"
        disabled={submitting}
        onclick={() => {
          void run(removeWorktree, true);
        }}
      >
        force delete
      </button>
    </div>
  {:else}
    <p>branch {keptBranch} kept — it has its own commits</p>
    <div class="actions">
      <button type="button" onclick={onClose}>ok</button>
    </div>
  {/if}
</div>

<style>
  .delete { display: flex; flex-direction: column; gap: 8px; }
  .field.row { display: flex; align-items: center; gap: 6px; }
  .key { color: var(--text-dim); }
  .msg { white-space: pre-wrap; }
  .err { color: var(--err); white-space: pre-wrap; }
  .actions { display: flex; justify-content: flex-end; gap: 8px; }
  .actions button { color: var(--accent); }
  .actions .danger { color: var(--err); }
</style>
