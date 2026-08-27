<script lang="ts">
  import type {
    AgentModel, EdgeKind, EdgeModel, OutputModel, TriggerModel, WorkflowModel,
  } from "../types";
  import { coerceNumber, duplicateEdgeError } from "./graphEditing";
  import {
    moveEdge, outputNodeFlow, removeAgent, removeEdge, removeFlow, removeOutput,
    renameAgent, setEdgeKind, triggerNodeFlow, WORKFLOW_NODE_ID,
  } from "./graphModel";
  import { inlineSchemaSummary } from "./triggerHelpers";

  let {
    model, selected = $bindable(null), onchanged,
  }: { model: WorkflowModel; selected: string | null; onchanged: () => void } = $props();

  let idDraft = $state("");
  let renameError = $state<string | null>(null);
  let edgeError = $state<string | null>(null);

  // The "§" node ids are not agents: the workflow node, and a trigger/output node keyed by
  // flow name; everything else selected is an agent id.
  const triggerFlow = $derived(selected === null ? null : triggerNodeFlow(selected));
  const outputFlow = $derived(selected === null ? null : outputNodeFlow(selected));
  const agentId = $derived(
    selected === WORKFLOW_NODE_ID || triggerFlow !== null || outputFlow !== null
      ? null
      : selected,
  );
  const agent = $derived(agentId === null ? null : (model.agents[agentId] ?? null));

  // A new selection discards whatever the id field was mid-edit, errors included.
  $effect(() => {
    idDraft = agentId ?? "";
    renameError = null;
    edgeError = null;
  });

  function commitRename(): void {
    if (agentId === null) return;
    const next = idDraft.trim();
    if (next === agentId) {
      renameError = null;
      return;
    }
    renameError = renameAgent(model, agentId, next);
    if (renameError === null) {
      selected = next;
      onchanged();
    }
  }

  /** The kind `<select>` reads out of the model, so a refused change would otherwise sit
   * there displaying a value the model never took: put the model's kind back on the
   * element, since an unchanged model schedules no re-render. */
  function setKind(select: HTMLSelectElement, from: string, edge: EdgeModel): void {
    const kind: EdgeKind =
      select.value === "send" ? "send" : select.value === "loop" ? "loop" : "ask";
    edgeError = duplicateEdgeError(model, from, edge.to, kind);
    if (edgeError !== null) {
      select.value = edge.kind;
      return;
    }
    setEdgeKind(model, from, edge.to, kind);
    onchanged();
  }

  function setPort(raw: string): void {
    const n = coerceNumber(raw);
    if (n === null) return;
    model.workflow.port = n;
    onchanged();
  }

  function setAskTimeout(raw: string): void {
    const n = coerceNumber(raw);
    if (n === null) return;
    model.workflow.ask_timeout_minutes = n;
    onchanged();
  }

  function setIdleDebounce(raw: string): void {
    const n = coerceNumber(raw);
    if (n === null) return;
    model.workflow.idle_debounce_seconds = n;
    onchanged();
  }

  function optional(raw: string): string | null {
    return raw === "" ? null : raw;
  }

  /** Negative and non-integer values are blocked because they fail u32 deserialization
   * inside workflow_merge as an opaque IPC error. The 0..=5 range is save-time validation's
   * job ("max_repairs must be 0..=5" in the footer) — matching setPort's convention of
   * letting range checks fall through to the footer rather than swallowing them here. */
  function setMaxRepairs(o: OutputModel, raw: string): void {
    const n = coerceNumber(raw);
    if (n === null || !Number.isInteger(n) || n < 0) return;
    o.max_repairs = n;
    onchanged();
  }
</script>

<aside class="inspector">
  {#if selected === WORKFLOW_NODE_ID}
    {@render workflowForm()}
  {:else if triggerFlow !== null && model.flows[triggerFlow] !== undefined}
    {@render triggerForm(triggerFlow, model.flows[triggerFlow]!.trigger)}
  {:else if outputFlow !== null && model.flows[outputFlow]?.output !== undefined}
    {@render outputForm(outputFlow, model.flows[outputFlow]!.output!)}
  {:else if agent !== null && agentId !== null}
    {@render agentForm(agentId, agent)}
  {:else}
    <p class="mono dim">select a node</p>
  {/if}
</aside>

{#snippet agentForm(id: string, a: AgentModel)}
  <div class="label">Agent</div>
  <label class="field">
    <span class="mono key">id</span>
    <input
      class="mono"
      value={idDraft}
      oninput={(e) => {
        idDraft = e.currentTarget.value;
      }}
      onblur={commitRename}
      onkeydown={(e) => {
        if (e.key === "Enter") commitRename();
      }}
    />
  </label>
  {#if renameError !== null}<p class="mono err">{renameError}</p>{/if}

  <label class="field">
    <span class="mono key">dir</span>
    <input
      class="mono"
      class:missing={a.dir === ""}
      placeholder="~/projects/my-project"
      value={a.dir}
      oninput={(e) => {
        a.dir = e.currentTarget.value;
        onchanged();
      }}
    />
  </label>

  <label class="field">
    <span class="mono key">model</span>
    <input
      class="mono"
      placeholder="default"
      value={a.model ?? ""}
      oninput={(e) => {
        a.model = optional(e.currentTarget.value);
        onchanged();
      }}
    />
  </label>

  <label class="field">
    <span class="mono key">permission_mode</span>
    <input
      class="mono"
      placeholder="default"
      value={a.permission_mode ?? ""}
      oninput={(e) => {
        a.permission_mode = optional(e.currentTarget.value);
        onchanged();
      }}
    />
  </label>

  <label class="field row">
    <input
      type="checkbox"
      checked={a.auto_clear}
      onchange={(e) => {
        a.auto_clear = e.currentTarget.checked;
        onchanged();
      }}
    />
    <span class="mono key">auto_clear</span>
  </label>

  {#if a.isolated_config}
    <div class="field">
      <span class="mono key">isolated_config</span>
      <span class="mono ro">
        true{a.skills.length ? ` · skills: ${a.skills.join(", ")}` : ""}
      </span>
    </div>
  {/if}

  <label class="field">
    <span class="mono key">prompt</span>
    <textarea
      class="mono"
      class:missing={a.prompt === ""}
      rows="5"
      spellcheck="false"
      placeholder="You are the planning agent."
      value={a.prompt}
      oninput={(e) => {
        a.prompt = e.currentTarget.value;
        onchanged();
      }}
    ></textarea>
  </label>

  <div class="label">Edges</div>
  {#if edgeError !== null}<p class="mono err">{edgeError}</p>{/if}
  <ul class="edges">
    {#each a.edges as edge, i (edge.to + ":" + edge.kind)}
      <li class="mono">
        <span class="to">{edge.to}</span>
        <select
          class="mono"
          value={edge.kind}
          onchange={(e) => {
            setKind(e.currentTarget, id, edge);
          }}
        >
          <option value="ask">ask</option>
          <option value="send">send</option>
          <option value="loop">loop</option>
        </select>
        <button
          title="move up"
          disabled={i === 0}
          onclick={() => {
            moveEdge(model, id, i, -1);
            onchanged();
          }}>↑</button
        >
        <button
          title="move down"
          disabled={i === a.edges.length - 1}
          onclick={() => {
            moveEdge(model, id, i, 1);
            onchanged();
          }}>↓</button
        >
        <button
          title="remove edge"
          class="err"
          onclick={() => {
            removeEdge(model, id, edge.to, edge.kind);
            edgeError = null; // removing an edge is the fix a kind clash asks for
            onchanged();
          }}>✕</button
        >
      </li>
    {:else}
      <li class="mono dim">none — drag from this node's right handle</li>
    {/each}
  </ul>

  <button
    class="mono danger"
    onclick={() => {
      removeAgent(model, id);
      selected = null;
      onchanged();
    }}>delete agent</button
  >
{/snippet}

{#snippet triggerForm(name: string, t: TriggerModel)}
  <div class="label">Trigger — flows.{name}</div>
  <div class="field">
    <span class="mono key">type</span>
    <span class="mono ro">
      {t.type === "on_start"
        ? "on_start — fires from the Run tab or `run --flow`"
        : `webhook — fires on POST /v1/flows/${name}/trigger`}
    </span>
  </div>

  <div class="field">
    <span class="mono key">target</span>
    <span class="mono ro">{t.edge.to}</span>
    <span class="mono dim hint">drag from the trigger node to re-aim it</span>
  </div>

  <label class="field">
    <span class="mono key">kind</span>
    <select
      class="mono"
      value={t.edge.kind}
      onchange={(e) => {
        t.edge.kind = e.currentTarget.value === "send" ? "send" : "ask";
        onchanged();
      }}
    >
      <option value="ask">ask — done when the target replies</option>
      <option value="send">send — done when every agent goes idle</option>
    </select>
  </label>

  {#if t.type === "on_start"}
    <label class="field">
      <span class="mono key">message</span>
      <textarea
        class="mono"
        class:missing={(t.message ?? "") === ""}
        rows="4"
        spellcheck="false"
        placeholder="Build the thing."
        value={t.message ?? ""}
        oninput={(e) => {
          t.message = e.currentTarget.value;
          onchanged();
        }}
      ></textarea>
    </label>
  {/if}

  <button
    class="mono danger"
    onclick={() => {
      removeFlow(model, name);
      selected = null;
      onchanged();
    }}>delete flow</button
  >
{/snippet}

{#snippet outputForm(name: string, o: OutputModel)}
  <div class="label">Output — flows.{name}.output</div>
  {#if o.schema !== undefined}
    <div class="field">
      <span class="mono key">schema</span>
      <span class="mono ro">{inlineSchemaSummary(o.schema)}</span>
      <span class="mono dim hint">edit the inline schema in the toml view</span>
    </div>
  {:else}
    <label class="field">
      <span class="mono key">schema_file</span>
      <input
        class="mono"
        class:missing={(o.schema_file ?? "") === ""}
        placeholder="schema.json"
        value={o.schema_file ?? ""}
        oninput={(e) => {
          o.schema_file = e.currentTarget.value;
          onchanged();
        }}
      />
      <span class="mono dim hint">JSON Schema path, relative to the tempo.toml</span>
    </label>
  {/if}

  <label class="field">
    <span class="mono key">max_repairs</span>
    <input
      class="mono"
      type="number"
      min="0"
      max="5"
      value={o.max_repairs}
      oninput={(e) => {
        setMaxRepairs(o, e.currentTarget.value);
      }}
    />
  </label>

  <button
    class="mono danger"
    onclick={() => {
      removeOutput(model, name);
      selected = null;
      onchanged();
    }}>delete output</button
  >
{/snippet}

{#snippet workflowForm()}
  <div class="label">Workflow</div>
  <label class="field">
    <span class="mono key">name</span>
    <input
      class="mono"
      value={model.workflow.name}
      oninput={(e) => {
        model.workflow.name = e.currentTarget.value;
        onchanged();
      }}
    />
  </label>
  <label class="field">
    <span class="mono key">port</span>
    <input
      class="mono"
      type="number"
      value={model.workflow.port}
      oninput={(e) => {
        setPort(e.currentTarget.value);
      }}
    />
  </label>
  <label class="field">
    <span class="mono key">ask_timeout_minutes</span>
    <input
      class="mono"
      type="number"
      value={model.workflow.ask_timeout_minutes}
      oninput={(e) => {
        setAskTimeout(e.currentTarget.value);
      }}
    />
  </label>
  <label class="field">
    <span class="mono key">idle_debounce_seconds</span>
    <input
      class="mono"
      type="number"
      value={model.workflow.idle_debounce_seconds}
      oninput={(e) => {
        setIdleDebounce(e.currentTarget.value);
      }}
    />
  </label>
  <label class="field">
    <span class="mono key">db</span>
    <input
      class="mono"
      value={model.workflow.db}
      oninput={(e) => {
        model.workflow.db = e.currentTarget.value;
        onchanged();
      }}
    />
  </label>
{/snippet}

<style>
  .inspector {
    border-left: 1px solid var(--panel-edge); padding: 10px;
    display: flex; flex-direction: column; gap: 6px; overflow-y: auto;
  }
  .field { display: flex; flex-direction: column; gap: 2px; }
  .field.row { flex-direction: row; align-items: center; gap: 6px; }
  .key { color: var(--text-dim); }
  input, textarea, select {
    background: var(--terminal-bg); color: var(--text);
    border: 1px solid var(--panel-edge); padding: 2px 4px; width: 100%;
  }
  input[type="checkbox"] { width: auto; }
  textarea { resize: vertical; }
  .missing { border-color: var(--err); }
  .edges { list-style: none; display: flex; flex-direction: column; gap: 4px; }
  .edges li { display: flex; align-items: center; gap: 4px; }
  .edges .to { flex: 1; overflow: hidden; text-overflow: ellipsis; }
  .edges select { width: auto; }
  .ro { color: var(--text); }
  .hint { font-size: var(--fs-label); }
  .danger { color: var(--err); align-self: flex-start; }
  .err { color: var(--err); }
  .dim { color: var(--text-dim); }
</style>
