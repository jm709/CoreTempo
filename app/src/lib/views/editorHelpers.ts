const AGENT_HEADER = /^\s*\[agents\.([a-z0-9][a-z0-9_-]{0,31})\]\s*(?:#.*)?$/;

/// Display-only roster preview for the stopped-mode editor. The id pattern matches
/// contracts §2.1 AgentId; nested sub-tables ([agents.x.y]) do not match because the
/// capture group excludes dots. Validation authority stays with workflow_parse.
export function plannedRoster(text: string): string[] {
  const ids: string[] = [];
  for (const line of text.split("\n")) {
    const m = AGENT_HEADER.exec(line);
    const id = m?.[1];
    if (id !== undefined && !ids.includes(id)) ids.push(id);
  }
  // oxlint-disable-next-line no-array-sort -- ES2022 lib; ids is a fresh local array
  return ids.sort();
}

export const WORKFLOW_TEMPLATE = `[workflow]
name = "my-workflow"

[agents.planner]
dir = "~/projects/my-project"
prompt = "You are the planning agent."
`;
