import { tick } from "svelte";
import { flashTerminal, openAgentTerminal } from "../state/ui.svelte";
import { focusAgentTerminal } from "./manager";

/// Jump to an agent's terminal from anywhere: switch the run view, then focus
/// and flash only after the flush — focus inside display:none is a silent no-op.
export function jumpToAgentTerminal(agent: string): void {
  openAgentTerminal(agent);
  void tick().then(() => {
    focusAgentTerminal(agent);
    flashTerminal(agent);
  });
}
