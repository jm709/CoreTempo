<script lang="ts">
  import type { OutputNode } from "./triggerHelpers";
  // oxlint-disable-next-line import/no-self-import -- Svelte 5 recursion; <svelte:self> is gone
  import OutputRenderer from "./OutputRenderer.svelte";

  let { node }: { node: OutputNode } = $props();
</script>

{#if node.kind === "card"}
  <dl class="card">
    {#each node.entries as entry (entry.key)}
      <dt class="label">{entry.key}</dt>
      <dd><OutputRenderer node={entry.value} /></dd>
    {/each}
  </dl>
{:else if node.kind === "table"}
  <table class="mono">
    <thead>
      <tr>{#each node.columns as column (column)}<th class="label">{column}</th>{/each}</tr>
    </thead>
    <tbody>
      {#each node.rows as row, i (i)}
        <tr>{#each row as cell, j (j)}<td>{cell}</td>{/each}</tr>
      {/each}
    </tbody>
  </table>
{:else if node.kind === "list"}
  <ul>
    {#each node.items as item, i (i)}<li><OutputRenderer node={item} /></li>{/each}
  </ul>
{:else if node.kind === "prose"}
  <p class="prose">{node.text}</p>
{:else if node.kind === "json"}
  <pre class="mono">{node.text}</pre>
{:else}
  <span class="mono">{node.text}</span>
{/if}

<style>
  .card { display: grid; grid-template-columns: max-content 1fr; gap: 2px 10px; margin: 0; }
  .card dd { margin: 0; min-width: 0; overflow-wrap: anywhere; }
  table { border-collapse: collapse; width: 100%; font-size: var(--fs-data); }
  th, td { text-align: left; padding: 2px 8px 2px 0; border-bottom: 1px solid var(--panel-edge); }
  ul { margin: 0; padding-left: 16px; }
  .prose { white-space: pre-wrap; margin: 0; }
  pre { margin: 0; overflow-x: auto; }
</style>
