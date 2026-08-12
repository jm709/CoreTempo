/// Value-driven classification of a validated [trigger.output] object into
/// renderable nodes. Value-driven, not schema-driven: serde_json alphabetizes
/// object keys core-side, so a schema could not restore author order anyway
/// (design 2026-08-07). Layout is stable because the key order is deterministic.

export type OutputNode =
  | { kind: "scalar"; text: string }
  | { kind: "prose"; text: string }
  | { kind: "list"; items: OutputNode[] }
  | { kind: "table"; columns: string[]; rows: string[][] }
  | { kind: "card"; entries: { key: string; value: OutputNode }[] }
  | { kind: "json"; text: string };

const PROSE_LENGTH = 120; // a string longer than this (or multi-line) reads as prose
const MAX_DEPTH = 3; // beyond this, structure degrades to pretty JSON

function isPlainObject(v: unknown): v is Record<string, unknown> {
  return typeof v === "object" && v !== null && !Array.isArray(v);
}

function isScalar(v: unknown): v is string | number | boolean | null {
  return v === null || typeof v === "string" || typeof v === "number" || typeof v === "boolean";
}

/// One table cell / card scalar as text; "—" for null/undefined keeps rows scannable.
function cellText(v: unknown): string {
  if (v === null || v === undefined) return "—";
  if (typeof v === "string") return v;
  if (typeof v === "number" || typeof v === "boolean") return String(v);
  return JSON.stringify(v);
}

function json(value: unknown): OutputNode {
  return { kind: "json", text: JSON.stringify(value, null, 2) };
}

export function classify(value: unknown, depth = 0): OutputNode {
  if (depth >= MAX_DEPTH) return json(value);
  if (typeof value === "string") {
    const prose = value.includes("\n") || value.length > PROSE_LENGTH;
    return prose ? { kind: "prose", text: value } : { kind: "scalar", text: value };
  }
  if (isScalar(value)) return { kind: "scalar", text: cellText(value) };
  if (Array.isArray(value)) {
    if (value.length === 0) return { kind: "scalar", text: "(empty)" };
    if (value.every(isPlainObject)) {
      const columns: string[] = [];
      for (const row of value) {
        for (const key of Object.keys(row)) if (!columns.includes(key)) columns.push(key);
      }
      return {
        kind: "table",
        columns,
        rows: value.map((row) => columns.map((c) => cellText(row[c]))),
      };
    }
    if (value.every(isScalar)) {
      return { kind: "list", items: value.map((v) => classify(v, depth + 1)) };
    }
    return json(value);
  }
  if (isPlainObject(value)) {
    const keys = Object.keys(value);
    if (keys.length === 0) return { kind: "scalar", text: "(empty)" };
    return {
      kind: "card",
      entries: keys.map((key) => ({ key, value: classify(value[key], depth + 1) })),
    };
  }
  return json(value);
}

const PREVIEW_KEYS = 4;
const PREVIEW_VALUE_LENGTH = 40;

function previewLine(v: unknown): string {
  const line = cellText(v).split("\n")[0] ?? "";
  return line.length > PREVIEW_VALUE_LENGTH ? `${line.slice(0, PREVIEW_VALUE_LENGTH)}…` : line;
}

/// Up to PREVIEW_KEYS top-level `key: value` lines for the graph node's compact
/// completed state; the Run tab owns the full rendering. Arrays serialize via
/// cellText's JSON form — the node is a glance, not a table.
export function outputPreview(output: unknown): string[] {
  if (!isPlainObject(output)) return [previewLine(output)];
  const keys = Object.keys(output);
  if (keys.length === 0) return ["(empty)"];
  const lines = keys.slice(0, PREVIEW_KEYS).map((k) => `${k}: ${previewLine(output[k])}`);
  if (keys.length > PREVIEW_KEYS) lines.push(`+${keys.length - PREVIEW_KEYS} more`);
  return lines;
}

/// Inspector row text for an inline schema. A JSON Schema may legally be a
/// boolean, so non-objects fall back to the bare label.
export function inlineSchemaSummary(schema: unknown): string {
  if (!isPlainObject(schema)) return "inline schema";
  return `inline schema — ${Object.keys(schema).length} top-level keys`;
}
