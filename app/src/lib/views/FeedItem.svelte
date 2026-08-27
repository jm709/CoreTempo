<script lang="ts">
  import { feedTime, isExternal, isFresh, lifecycleGlyph, originAgent, originLabel }
    from "../format";
  import { uiState } from "../state/ui.svelte";
  import { runState } from "../state/run.svelte";
  import { jumpToAgentTerminal } from "../term/jump";
  import type { MessageRecord } from "../types";

  let { message }: { message: MessageRecord } = $props();

  function open(): void {
    if (runState.phase !== "running") return;
    jumpToAgentTerminal(message.to);
  }

  function hover(on: boolean): void {
    uiState.hoverFrom = on ? originAgent(message.from) : null;
    uiState.hoverTo = on ? message.to : null;
  }
</script>

<article
  class="item mono"
  class:fresh={isFresh(message.created_at, Date.now())}
  onmouseenter={() => hover(true)}
  onmouseleave={() => hover(false)}
>
  <button class="body-btn" onclick={open}>
    <div class="line">
      <span class="time">{feedTime(message.created_at)}</span>
      <span class="route">{originLabel(message.from)} → {message.to}</span>
      <span class="label">{message.kind}</span>
      {#if isExternal(message.from)}<span class="chip label">external</span>{/if}
    </div>
    <div class="text">"{message.body}"</div>
    <div class="status" class:err={message.status === "failed"}>
      {lifecycleGlyph(message)} {message.status}
    </div>
    {#if message.status === "failed" && message.reason}
      <div class="reason">{message.reason}</div>
    {/if}
    {#if message.reply !== null}
      <div class="reply">↳ {message.reply}</div>
    {/if}
  </button>
</article>

<style>
  .item { border-bottom: 1px solid var(--panel-edge); }
  .item.fresh { animation: ct-fade-in var(--t-feed-fade) ease-out; }
  .body-btn { display: block; width: 100%; text-align: left; padding: 6px 10px; }
  .line { display: flex; gap: 8px; align-items: baseline; }
  .time { color: var(--text-dim); }
  .route { color: var(--text); }
  .chip { border: 1px solid var(--panel-edge); border-radius: 2px; padding: 0 4px; }
  .text {
    color: var(--text-dim); margin-top: 2px;
    overflow: hidden; display: -webkit-box; -webkit-box-orient: vertical;
    -webkit-line-clamp: 2; line-clamp: 2;
  }
  .status { color: var(--info); margin-top: 2px; }
  .status.err { color: var(--err); }
  .reason { color: var(--text-dim); margin-top: 2px; white-space: normal; word-break: break-word; }
  .reply { color: var(--ok); margin-top: 2px; }
</style>
