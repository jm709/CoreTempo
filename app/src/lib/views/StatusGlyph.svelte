<script lang="ts">
  import { STATE_GLYPHS } from "../format";
  import type { AgentState, SessionState } from "../types";

  let { state }: { state: AgentState | SessionState } = $props();
</script>

<span class="glyph {state}">{STATE_GLYPHS[state]}</span>
