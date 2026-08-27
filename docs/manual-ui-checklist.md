# Manual UI checklist

The part of the desktop app that only a person at the keyboard can verify.
Everything else in issue #5 is covered by the automated pass described there
(screenshots, XTEST clicks, paste), which cannot press keys into the webview
under WSLg. Run this after touching `app/src/lib/keys.ts`, the terminal panes,
the Chat composer or the Run tab.

Each item states what to do and what you should see. Tick them off in a PR
comment or in issue #5.

## Setup (no tokens)

A fake `claude` that reports state over the API, echoes what you type, and
answers asks with `ok`. Put it first on PATH so the app spawns it instead of
the real binary.

```bash
mkdir -p ~/.coretempo/ui-check/bin ~/.coretempo/ui-check/a1 ~/.coretempo/ui-check/a2
cat > ~/.coretempo/ui-check/bin/claude <<'EOF'
#!/bin/bash
me="$CORETEMPO_AGENT_ID"
post() {
  exec 3<>"/dev/tcp/127.0.0.1/$CORETEMPO_PORT" || return 1
  printf 'POST %s HTTP/1.1\r\nHost: 127.0.0.1\r\n' "$1" >&3
  printf 'Authorization: Bearer %s\r\n' "$CORETEMPO_TOKEN" >&3
  printf 'X-CoreTempo-Agent: %s\r\n' "$me" >&3
  printf 'Content-Type: application/json\r\nContent-Length: %d\r\n' "${#2}" >&3
  printf 'Connection: close\r\n\r\n%s' "$2" >&3
  cat <&3 >/dev/null
  exec 3>&-
}
printf 'fake claude [%s] pid %s — type here\n> ' "$me" "$$"
post "/v1/agents/$me/state" '{"state":"idle"}'
last=""
while IFS= read -r line; do
  printf '[%s] got: %s\n> ' "$me" "$line"
  [[ "$line" =~ (m-[0-9a-f]+) ]] || continue
  id="${BASH_REMATCH[1]}"
  [ "$id" = "$last" ] && continue
  last="$id"
  post "/v1/agents/$me/state" '{"state":"working"}'
  sleep 1
  post "/v1/messages/$id/reply" "{\"code\":0,\"body\":\"ok from $me\"}"
  post "/v1/agents/$me/state" '{"state":"idle"}'
done
EOF
chmod +x ~/.coretempo/ui-check/bin/claude

cat > ~/.coretempo/ui-check/tempo.toml <<EOF
[workflow]
name = "ui-check"
db = "$HOME/.coretempo/ui-check/tempo.db"
idle_debounce_seconds = 0.5

[agents.a1]
dir = "$HOME/.coretempo/ui-check/a1"
prompt = "You are a1."

[agents.a2]
dir = "$HOME/.coretempo/ui-check/a2"
prompt = "You are a2."

[flows.hello]
agents = ["a1"]
trigger = { type = "on_start", edge = { to = "a1", kind = "ask" }, message = "hello from the flow" }
EOF

PATH=~/.coretempo/ui-check/bin:$PATH ./dev
```

Open `~/.coretempo/ui-check/tempo.toml` from the welcome card (type the path
into the box or use Browse…), press ▶ Run, and accept the trust dialog the
first time (it lists `a1` and `a2`; later runs do not ask). Switch the centre
to `terminals`. To use a real `claude` instead, drop the `PATH=` prefix; keep
`model = "haiku"` on both agents and expect a few tokens per item.

## Terminal keys

- [ ] **Keys reach the agent.** Click the `a1` pane (accent border appears),
      type `abc` and Enter. The pane echoes `abc` and the fake prints
      `[a1] got: abc`. With a real `claude`, the text lands in its prompt.
- [ ] **``mod+` `` releases capture.** With `a1` focused, press ``Ctrl+` ``
      (``⌘+` `` on macOS). The accent border goes away; typing no longer
      reaches the pane.
- [ ] **`mod+1..9` jumps panes.** Press `Ctrl+2`: `a2` gets the border and
      typed keys; `Ctrl+1` returns to `a1`. A number with no pane does nothing.
- [ ] **`mod+Enter` maximises with buffers hot.** Press `Ctrl+Enter` on `a1`:
      it fills the centre. Type a line; then `Ctrl+Enter` again restores the
      grid and both panes still show their full history, `a2` included.
- [ ] **`Esc` passes through.** With `a1` focused, press `Esc`. Nothing in the
      app reacts (no release, no dialog closes). The fake echoes `^[` on the
      prompt line; a real `claude` treats it as its own Esc.
- [ ] **App-scope chords.** `Ctrl+F` switches the dock to Feed, `Ctrl+T` to
      Chat, `Ctrl+E` toggles graph/terminals, `Ctrl+R` restarts the focused
      agent (the pane shows a fresh `fake claude … ready` banner and a new pid).
      Each works both when a pane is focused and when none is.

## Chat

- [ ] **Enter sends.** In the Chat tab pick `a1` / `ask`, type `ping`, press
      Enter. The composer clears, the ask appears in the Chat history and in
      Feed, `a1`'s pane shows the injected `[CoreTempo m-… from … — reply
      expected] ping`, and within ~2 s the history shows `⌀0 replied` with
      `ok from a1`.
- [ ] **Shift+Enter inserts a newline** instead of sending; the message sends
      as one body with the line break.
- [ ] **`send` needs no reply.** Switch the kind to `send`, send `fyi`. The
      item reaches `done` once `a1` goes idle again, with no reply line.

## Editor

- [ ] **Unsaved edits block Run.** With the run stopped, type anything into
      the toml view: the title gains `•`, the header shows `save the workflow
      to run it` and ▶ Run is disabled. Save re-enables it. Stop a running
      workflow the same way with edits pending: ■ Stop stays enabled
      throughout.

## Run tab

- [ ] **Fire an `on_start` flow.** In the Run tab press `fire` next to
      `hello`. The trigger card shows the kickoff working, then `replied` with
      `ok from a1`; `a1`'s pane shows the header `[CoreTempo m-… from http,
      flow hello — reply expected] hello from the flow`. A second `fire` adds
      a History row.
- [ ] **Webhook flows have no fire button** (add a `type = "webhook"` flow to
      see it): the row shows a `webhook` label instead, and the empty-state
      text points at `POST /v1/flows/<name>/trigger`.

## Native dialogs

- [ ] **Browse…** opens the system file picker; choosing a `tempo.toml` opens
      it in the editor and adds it to Recent.
- [ ] **New workflow…** opens a folder picker; choosing an empty folder creates
      `tempo.toml` there, opens it, and Save writes it.
- [ ] **Cancel** on either picker returns to the welcome card with no error.
- [ ] **Trust dialog → Cancel** (use fresh agent dirs): the run does not start,
      the header stays ▶ Run, and nothing is written to `~/.claude.json`.

## Cleanup

```bash
trash ~/.coretempo/ui-check   # or rm -r; nothing else references it
```

The trust entries for `~/.coretempo/ui-check/a1` and `a2` stay in
`~/.claude.json`; they are harmless and Claude Code owns that file.
