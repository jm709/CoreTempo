<script lang="ts">
  import { feedTime } from "../format";
  import { fireFlow, runFlows, toCmdError } from "../ipc";
  import { runState } from "../state/run.svelte";
  import { selectTrigger, triggersState } from "../state/triggers.svelte";
  import type { FlowInfo } from "../types";
  import { classify } from "./triggerHelpers";
  import OutputRenderer from "./OutputRenderer.svelte";

  const current = $derived(
    triggersState.list.find((t) => t.id === triggersState.selectedId)
      ?? triggersState.list.at(-1)
      ?? null,
  );
  // oxlint-disable-next-line no-array-reverse -- ES2022 lib; the spread copy is safe to mutate
  const history = $derived([...triggersState.list].reverse());

  let flows = $state<FlowInfo[]>([]);
  let fireError = $state<string | null>(null);

  // Re-fetched per run: the roster is frozen for the run's lifetime.
  $effect(() => {
    if (runState.info === null) {
      flows = [];
      fireError = null;
      return;
    }
    void runFlows()
      .then((list) => {
        flows = list;
      })
      .catch((e: unknown) => {
        flows = [];
        fireError = toCmdError(e).message;
      });
  });

  async function fire(name: string): Promise<void> {
    fireError = null;
    try {
      await fireFlow(name);
      // The lifecycle itself arrives via message.created / workflow.completed
      // bus events — nothing else to do here.
    } catch (e) {
      fireError = toCmdError(e).message;
    }
  }
</script>

<div class="run">
  {#if flows.length > 0}
    <section class="flows">
      <div class="label">Flows</div>
      {#each flows as flow (flow.name)}
        <div class="mono row">
          <span class="name">{flow.name}</span>
          <span class="label">→ {flow.target}</span>
          {#if flow.type === "on_start"}
            <button class="mono" onclick={() => void fire(flow.name)}>fire</button>
          {:else}
            <span class="label">webhook</span>
          {/if}
        </div>
      {/each}
    </section>
  {/if}
  {#if fireError !== null}<p class="mono err">{fireError}</p>{/if}
  {#if current === null}
    <p class="label empty">
      No trigger yet. Fire an on_start flow above, POST to a webhook flow,
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
  .flows { border-bottom: 1px solid var(--panel-edge); padding-bottom: 8px; }
  .flows .row { display: flex; gap: 8px; align-items: baseline; padding: 2px 0; }
  .flows .name { color: var(--accent); }
  .err { color: var(--err); }
</style>
