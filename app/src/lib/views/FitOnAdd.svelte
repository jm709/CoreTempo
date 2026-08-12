<script lang="ts">
  import { useSvelteFlow } from "@xyflow/svelte";

  // Must render inside <SvelteFlow> — that is where the flow store lives. A node added from
  // the toolbar takes the next free auto-layout slot, which is often below the fold; without
  // this the agent looks like it was never created.
  let { count }: { count: number } = $props();

  const flow = useSvelteFlow();
  let seen: number | null = null;

  $effect(() => {
    const now = count;
    if (seen !== null && now > seen) void flow.fitView();
    seen = now;
  });
</script>
