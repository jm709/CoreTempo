<script lang="ts">
  import { sessionResume, sessionStop, toCmdError } from "../ipcSessions";
  import type { SessionView } from "../types";
  import { cardActions, cardLine2, resumeDisabled } from "./railHelpers";
  import StatusGlyph from "./StatusGlyph.svelte";

  let { session, selected, onSelect, onDelete }: {
    session: SessionView; selected: boolean;
    onSelect: () => void; onDelete: (s: SessionView) => void;
  } = $props();

  let actionError = $state<string | null>(null);

  async function act(fn: () => Promise<unknown>): Promise<void> {
    actionError = null;
    try {
      await fn();
    } catch (e) {
      actionError = toCmdError(e).message;
    }
  }
</script>

<div class="row" class:hl={selected}>
  <button class="card mono" onclick={onSelect}>
    <span class="line1">
      <StatusGlyph state={session.state} />
      <span class="title">{session.title}</span>
      {#if session.blocked !== null}
        <span class="blocked" title={`blocked on ${session.blocked.tool ?? "?"}`}>⏸</span>
      {/if}
    </span>
    <span class="line2 dim">{cardLine2(session)}</span>
  </button>
  <div class="actions">
    {#each cardActions(session) as action (action)}
      {#if action === "stop"}
        <button
          class="mono"
          onclick={() => {
            void act(() => sessionStop(session.id));
          }}
        >
          stop
        </button>
      {:else if action === "resume"}
        {@const disabledReason = resumeDisabled(session)}
        <button
          class="mono"
          disabled={disabledReason !== null}
          title={disabledReason ?? undefined}
          onclick={() => {
            void act(() => sessionResume(session.id));
          }}
        >
          resume
        </button>
      {:else}
        <button
          class="mono"
          onclick={() => {
            onDelete(session);
          }}
        >
          rm
        </button>
      {/if}
    {/each}
  </div>
  {#if actionError !== null}<div class="err">{actionError}</div>{/if}
</div>

<style>
  .row { display: flex; flex-direction: column; }
  .row.hl { background: color-mix(in srgb, var(--accent) 12%, transparent); }
  .card {
    display: flex; flex-direction: column; gap: 2px; text-align: left; width: 100%;
    padding: 4px 10px;
  }
  .line1 { display: flex; align-items: center; gap: 8px; }
  .title { overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
  .blocked { color: var(--err); margin-left: auto; }
  .line2.dim { color: var(--text-dim); font-size: var(--fs-label); }
  .actions { display: flex; gap: 8px; padding: 0 10px 4px 30px; }
  .actions button { color: var(--accent); }
  .err { padding: 0 10px 4px 30px; color: var(--err); white-space: pre-wrap; }
</style>
