<script lang="ts">
  import { VList } from "virtua/svelte";
  import { messagesState } from "../state/messages.svelte";
  import { isAtBottom } from "./feedHelpers";
  import FeedItem from "./FeedItem.svelte";
  import type { MessageRecord } from "../types";

  interface VListHandle {
    getScrollOffset(): number;
    getScrollSize(): number;
    getViewportSize(): number;
    scrollToIndex(index: number, opts?: { align?: "start" | "end" }): void;
  }

  let vlist = $state<VListHandle | null>(null);
  let stick = $state(true);

  function onScroll(offset: number): void {
    if (vlist === null) return;
    stick = isAtBottom(offset, vlist.getViewportSize(), vlist.getScrollSize());
  }

  $effect(() => {
    const n = messagesState.list.length;
    if (n > 0 && stick && vlist !== null) {
      vlist.scrollToIndex(n - 1, { align: "end" });
    }
  });
</script>

<div class="feed">
  <VList
    data={messagesState.list}
    getKey={(m: MessageRecord) => m.id}
    style="height: 100%;"
    bind:this={vlist}
    onscroll={onScroll}
  >
    {#snippet children(item: MessageRecord)}
      <FeedItem message={item} />
    {/snippet}
  </VList>
</div>

<style>
  .feed { height: 100%; }
</style>
