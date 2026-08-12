<script lang="ts">
  import { uiState } from "../state/ui.svelte";
  import Chat from "./Chat.svelte";
  import Feed from "./Feed.svelte";
  import RunTab from "./RunTab.svelte";
</script>

<div class="dock">
  <div class="tabs">
    <button
      class="label tab"
      class:active={uiState.dockTab === "feed"}
      onclick={() => { uiState.dockTab = "feed"; }}
    >Feed</button>
    <button
      class="label tab"
      class:active={uiState.dockTab === "chat"}
      onclick={() => { uiState.dockTab = "chat"; }}
    >Chat</button>
    <button
      class="label tab"
      class:active={uiState.dockTab === "run"}
      onclick={() => { uiState.dockTab = "run"; }}
    >Run</button>
  </div>
  <div class="pane" class:off={uiState.dockTab !== "feed"}><Feed /></div>
  <div class="pane" class:off={uiState.dockTab !== "chat"}><Chat /></div>
  <div class="pane" class:off={uiState.dockTab !== "run"}><RunTab /></div>
</div>

<style>
  .dock { display: flex; flex-direction: column; height: 100%; }
  .tabs { display: flex; border-bottom: 1px solid var(--panel-edge); }
  .tab { padding: 6px 12px; }
  .tab.active { color: var(--accent); border-bottom: 1px solid var(--accent); }
  .pane { flex: 1; min-height: 0; }
  .pane.off { display: none; }   /* all stay mounted: feed scroll state survives tab flips */
</style>
