import type { ITerminalInitOnlyOptions, ITerminalOptions, ITheme } from "@xterm/xterm";

export const ansiTheme: ITheme = {
  background: "#0C0E10", // --terminal-bg
  foreground: "#D8DEE4", // --text
  cursor: "#E2A85F", // --accent
  cursorAccent: "#0C0E10",
  selectionBackground: "#E2A85F33",
  black: "#16191C",
  red: "#C4585A", // --err family
  green: "#7FB069", // --ok family
  yellow: "#E2A85F", // --accent family
  blue: "#6699CC", // --info family
  magenta: "#B07FB0",
  cyan: "#6FB3A8",
  white: "#D8DEE4",
  brightBlack: "#7C8691", // --text-dim
  brightRed: "#D4787A",
  brightGreen: "#9BC489",
  brightYellow: "#EBC08A",
  brightBlue: "#8AB4DC",
  brightMagenta: "#C79BC7",
  brightCyan: "#8FC7BE",
  brightWhite: "#F0F4F8",
};

export function terminalOptions(
  scrollback: number,
): ITerminalOptions & ITerminalInitOnlyOptions {
  return {
    scrollback,
    fontSize: 13, // spec §9.3: 13 px terminals
    fontFamily: '"JetBrainsMono NL", "JetBrains Mono NL", "JetBrains Mono", monospace',
    cursorBlink: false, // motion policy: no idle animation
    allowProposedApi: true, // required by the unicode11 addon
    theme: ansiTheme,
  };
}
