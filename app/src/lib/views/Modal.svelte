<script lang="ts">
  import type { Snippet } from "svelte";

  let { onClose, children }: { onClose: () => void; children: Snippet } = $props();

  function onKeydown(ev: KeyboardEvent): void {
    if (ev.key === "Escape") onClose();
  }
</script>

<svelte:window onkeydown={onKeydown} />

<!-- svelte-ignore a11y_click_events_have_key_events -->
<!-- svelte-ignore a11y_no_static_element_interactions -->
<div class="backdrop" onclick={onClose}>
  <!-- svelte-ignore a11y_click_events_have_key_events -->
  <!-- svelte-ignore a11y_no_static_element_interactions -->
  <div class="panel" onclick={(ev) => { ev.stopPropagation(); }}>
    {@render children()}
  </div>
</div>

<style>
  .backdrop {
    position: fixed; inset: 0; display: flex; align-items: center; justify-content: center;
    background: color-mix(in srgb, black 55%, transparent);
  }
  .panel {
    background: var(--panel); border: 1px solid var(--panel-edge);
    min-width: 360px; max-width: 560px; max-height: 80vh; overflow-y: auto; padding: 16px;
  }
</style>
