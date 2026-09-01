import type { CreateSessionRequest } from "../types";

export function buildCreateRequest(form: {
  project: string; worktree: boolean; cwd: string; title: string; prompt: string;
  model: string; permissionMode: "default" | "bypassPermissions"; isolatedConfig: boolean;
}): CreateSessionRequest {
  const req: CreateSessionRequest = {
    project: form.project,
    worktree: form.worktree,
    isolated_config: form.isolatedConfig,
  };
  if (form.cwd !== "") req.cwd = form.cwd;
  if (form.title !== "") req.title = form.title;
  if (form.prompt !== "") req.prompt = form.prompt;
  if (form.model !== "") req.model = form.model;
  if (form.permissionMode !== "default") req.permission_mode = form.permissionMode;
  return req;
}
