<script lang="ts">
  import { sendChat, toCmdError } from "../ipc";
  import { agentsState } from "../state/agents.svelte";
  import { messagesState, upsertMessage } from "../state/messages.svelte";
  import { runState } from "../state/run.svelte";
  import { isChat } from "../format";
  import FeedItem from "./FeedItem.svelte";
  import type { MessageKind } from "../types";

  const chatList = $derived(messagesState.list.filter(isChat));

  let to = $state("");
  let kind = $state<MessageKind>("ask");
  let draft = $state("");
  let error = $state<string | null>(null);

  $effect(() => {
    const first = agentsState.order[0];
    if (to === "" && first !== undefined) to = first;
  });

  async function submit(): Promise<void> {
    const body = draft.trim();
    if (body === "" || to === "" || runState.phase !== "running") return;
    try {
      upsertMessage(await sendChat(to, kind, body));
      draft = "";
      error = null;
    } catch (e: unknown) {
      error = toCmdError(e).message;
    }
  }

  function onKeydown(ev: KeyboardEvent): void {
    if (ev.key === "Enter" && !ev.shiftKey) {
      ev.preventDefault();
      void submit();
    }
  }
</script>

<div class="chat">
  <div class="history">
    {#each chatList as m (m.id)}
      <FeedItem message={m} />
    {/each}
    {#if chatList.length === 0}
      <p class="empty label">No human ↔ agent traffic yet</p>
    {/if}
  </div>
  {#if error !== null}<p class="error mono">{error}</p>{/if}
  <div class="composer">
    <select class="mono" bind:value={to} disabled={runState.phase !== "running"}>
      {#each agentsState.order as id (id)}<option value={id}>{id}</option>{/each}
    </select>
    <select class="mono" bind:value={kind}>
      <option value="ask">ask</option>
      <option value="send">send</option>
    </select>
    <textarea
      class="mono"
      rows="2"
      placeholder={runState.phase === "running" ? "Message an agent…" : "Start a run to chat"}
      bind:value={draft}
      onkeydown={onKeydown}
      disabled={runState.phase !== "running"}
    ></textarea>
  </div>
</div>

<style>
  .chat { display: flex; flex-direction: column; height: 100%; }
  .history { flex: 1; overflow-y: auto; }
  .empty { padding: 10px; }
  .error { color: var(--err); padding: 4px 10px; }
  .composer {
    display: grid; grid-template-columns: 1fr auto; gap: 4px; padding: 6px;
    border-top: 1px solid var(--panel-edge);
  }
  .composer textarea {
    grid-column: 1 / -1; background: var(--terminal-bg); color: var(--text);
    border: 1px solid var(--panel-edge); padding: 4px 6px; resize: none;
  }
  .composer select {
    background: var(--panel); color: var(--text); border: 1px solid var(--panel-edge);
  }
</style>
