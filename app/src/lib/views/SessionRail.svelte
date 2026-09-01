<script lang="ts">
  import { open } from "@tauri-apps/plugin-dialog";
  import { projectRegister, toCmdError } from "../ipcSessions";
  import { byProject, selectSession, sessionsState } from "../state/sessions.svelte";
  import type { SessionView } from "../types";
  import SessionCard from "./SessionCard.svelte";

  let { onCreate, onDelete }: {
    onCreate: (projectId: string) => void;
    onDelete: (s: SessionView) => void;
  } = $props();

  let registerError = $state<string | null>(null);

  async function registerProject(): Promise<void> {
    registerError = null;
    const path = await open({ directory: true });
    if (path === null) return;
    try {
      await projectRegister(path);
    } catch (e) {
      registerError = toCmdError(e).message;
    }
  }
</script>

<nav class="rail">
  <div class="label heading">Sessions</div>
  {#each byProject() as group (group.project.id)}
    <div class="group">
      <div class="group-head label">
        <span class="name">{group.project.name}</span>
        <button
          class="mono new"
          onclick={() => {
            onCreate(group.project.id);
          }}
        >
          + session
        </button>
      </div>
      {#each group.sessions as session (session.id)}
        <SessionCard
          {session}
          selected={sessionsState.selected === session.id}
          onSelect={() => {
            selectSession(session.id);
          }}
          {onDelete}
        />
      {/each}
    </div>
  {/each}
  <button
    class="mono newproject"
    onclick={() => {
      void registerProject();
    }}
  >
    + project
  </button>
  {#if registerError !== null}<div class="err">{registerError}</div>{/if}
</nav>

<style>
  .rail { padding: 8px 0; overflow-y: auto; height: 100%; }
  .heading { padding: 0 10px 6px; }
  .group-head {
    display: flex; align-items: center; gap: 8px; padding: 6px 10px 4px;
  }
  .group-head .name { overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
  .group-head .new { margin-left: auto; color: var(--accent); }
  .newproject { color: var(--accent); text-align: left; padding: 6px 10px; width: 100%; }
  .err { padding: 4px 10px; color: var(--err); white-space: pre-wrap; }
</style>
