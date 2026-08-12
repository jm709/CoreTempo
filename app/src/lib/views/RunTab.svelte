<script lang="ts">
  import { feedTime } from "../format";
  import { selectTrigger, triggersState } from "../state/triggers.svelte";
  import { classify } from "./triggerHelpers";
  import OutputRenderer from "./OutputRenderer.svelte";

  const current = $derived(
    triggersState.list.find((t) => t.id === triggersState.selectedId)
      ?? triggersState.list.at(-1)
      ?? null,
  );
  // oxlint-disable-next-line no-array-reverse -- ES2022 lib; the spread copy is safe to mutate
  const history = $derived([...triggersState.list].reverse());
</script>

<div class="run">
  {#if current === null}
    <p class="label empty">
      No trigger yet. Fire the workflow's webhook (or start an on_start workflow)
      and its lifecycle appears here.
    </p>
  {:else}
    <section class="lifecycle">
      <header>
        <span class="mono id">{current.id}</span>
        {#if current.agent !== null}<span class="label">→ {current.agent}</span>{/if}
        <span class="label phase {current.phase}">
          {#if current.phase === "working"}in progress…{:else}{current.result ?? "failed"}{/if}
        </span>
      </header>
      {#if current.body !== null}<p class="body">{current.body}</p>{/if}

      {#each current.rejections as rejection, i (i)}
        <details class="rejection">
          <summary class="label">repair {i + 1}: schema validation failed</summary>
          <pre class="mono">{rejection.errors}</pre>
        </details>
      {/each}

      {#if current.phase === "completed" && current.output !== null}
        <div class="output"><OutputRenderer node={classify(current.output)} /></div>
      {:else if current.phase === "completed" && current.code === 1}
        <p class="declined">Agent declined (code 1):</p>
        {#if current.reply !== null}<p class="body">{current.reply}</p>{/if}
      {:else if current.phase === "completed" && current.reply !== null}
        <p class="body">{current.reply}</p>
      {:else if current.phase === "failed"}
        {#if current.reasonCode !== null}
          <span class="mono reason-code">{current.reasonCode}</span>
        {/if}
        {#if current.reason !== null}<p class="body">{current.reason}</p>{/if}
        {#if current.result === "timeout"}<p class="body">The kickoff timed out.</p>{/if}
      {/if}
    </section>

    {#if history.length > 1}
      <section class="history">
        <div class="label">History</div>
        {#each history as t (t.id)}
          <button
            class="mono row"
            class:active={t.id === current.id}
            onclick={() => selectTrigger(t.id === triggersState.list.at(-1)?.id ? null : t.id)}
          >
            <span>{t.id}</span>
            <span class="label time">{feedTime(t.startedAt ?? "")}</span>
            <span class="label">
              {t.phase === "working" ? "in progress…" : (t.result ?? "failed")}
            </span>
          </button>
        {/each}
      </section>
    {/if}
  {/if}
</div>

<style>
  .run { height: 100%; overflow-y: auto; padding: 8px; display: flex;
         flex-direction: column; gap: 12px; }
  .empty { padding: 12px 4px; }
  header { display: flex; gap: 8px; align-items: baseline; }
  .phase.failed { color: var(--err); }
  .body { white-space: pre-wrap; overflow-wrap: anywhere; margin: 4px 0; }
  .rejection pre { overflow-x: auto; }
  .output { border: 1px solid var(--panel-edge); border-radius: 4px; padding: 8px; }
  .output, .body, .rejection pre { user-select: text; }
  .history { border-top: 1px solid var(--panel-edge); padding-top: 8px; }
  .row { display: flex; justify-content: space-between; width: 100%; padding: 3px 4px; }
  .row.active { color: var(--accent); }
  .row .time { color: var(--text-dim); }
</style>
