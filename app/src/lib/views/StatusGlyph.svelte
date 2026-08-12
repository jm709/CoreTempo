<script lang="ts">
  import { STATE_GLYPHS } from "../format";
  import type { AgentState } from "../types";

  let { state }: { state: AgentState } = $props();
</script>

<span class="glyph {state}">{STATE_GLYPHS[state]}</span>
