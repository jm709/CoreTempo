<script lang="ts">
  import { sessionResume, toCmdError } from "../ipcSessions";
  import { sessionsState } from "../state/sessions.svelte";
  import { resumeDisabled } from "./railHelpers";
  import { bannerFor, retryStream, syncSelection } from "./sessionTerminalHelpers";

  let pane = $state<HTMLElement | null>(null);
  let actionError = $state<string | null>(null);

  // Selection bookkeeping for the effect below; deliberately not $state — reading
  // these back would re-arm the effect that writes them.
  let previous: string | null = null;
  let generation = 0;
  // Every open/suspend/retry runs on this chain: two concurrent `ensure` calls for
  // one id would build two terminals for it, and an open that lost the race would
  // attach a superseded session into the shared pane.
  let queue: Promise<void> = Promise.resolve();

  const selectedId = $derived(sessionsState.selected);
  const selected = $derived(
    selectedId === null ? null : sessionsState.sessions[selectedId] ?? null,
  );
  const banner = $derived(selected === null ? null : bannerFor(selected));
  const streamError = $derived(
    selectedId === null ? null : sessionsState.streamErrors[selectedId] ?? null,
  );

  function enqueue(work: () => Promise<void>): void {
    queue = queue.then(work).catch((e: unknown) => {
      actionError = toCmdError(e).message;
    });
  }

  $effect(() => {
    const id = sessionsState.selected;
    const el = pane;
    // The daemon connection is a dependency, not decoration: a reconnect drops every
    // terminal (sessionsWire) before it lands here, so this re-run is what reopens the
    // selected one against the new daemon. While it is down there is no stream to
    // subscribe to, so the open waits for it.
    const connected = sessionsState.conn === "connected";
    actionError = null;
    generation += 1;
    const gen = generation;
    enqueue(async () => {
      if (gen !== generation) return; // a newer selection is already queued behind us
      previous = await syncSelection(previous, id, el, connected);
    });
  });

  function resume(id: string): void {
    actionError = null;
    enqueue(async () => {
      await sessionResume(id);
    });
  }

  function retry(id: string): void {
    actionError = null;
    enqueue(() => retryStream(id, pane));
  }
</script>

<section class="center">
  <div
    class="pane"
    class:hidden={selectedId === null}
    class:dim={banner !== null}
    bind:this={pane}
  ></div>
  {#if selectedId === null}
    <div class="empty label">select a session</div>
  {/if}
  {#if selected !== null && banner !== null}
    {@const disabledReason = resumeDisabled(selected)}
    <div class="banner mono">
      <span>[{banner}]</span>
      <button
        class="action"
        disabled={disabledReason !== null}
        title={disabledReason ?? undefined}
        onclick={() => {
          resume(selected.id);
        }}
      >
        resume
      </button>
    </div>
  {/if}
  {#if selectedId !== null && streamError !== null}
    <div class="streamerr mono">
      <span>{streamError}</span>
      <button
        class="action"
        onclick={() => {
          retry(selectedId);
        }}
      >
        retry
      </button>
    </div>
  {/if}
  {#if actionError !== null}<div class="err mono">{actionError}</div>{/if}
</section>

<style>
  .center {
    position: relative; height: 100%; min-width: 0; min-height: 0;
    background: var(--terminal-bg);
  }
  .pane { height: 100%; padding: 4px; }
  .pane.hidden { display: none; }
  .pane.dim { opacity: 0.4; }                              /* last screen of a dead session */
  .empty {
    position: absolute; inset: 0; display: flex;
    align-items: center; justify-content: center; color: var(--text-dim);
  }
  .banner, .streamerr {
    position: absolute; left: 0; right: 0; display: flex; gap: 12px;
    align-items: center; justify-content: center; padding: 4px 10px;
    background: var(--panel); border-bottom: 1px solid var(--panel-edge);
    font-size: var(--fs-data);
  }
  .banner { top: 0; color: var(--text-dim); }
  .streamerr { top: 0; color: var(--err); }
  .banner ~ .streamerr { top: 26px; }
  .action {
    color: var(--accent); border: 1px solid var(--panel-edge); padding: 0 10px;
    background: var(--bg);
  }
  .err {
    position: absolute; left: 0; right: 0; bottom: 0; padding: 4px 10px;
    color: var(--err); background: var(--panel); white-space: pre-wrap;
    font-size: var(--fs-data);
  }
</style>
