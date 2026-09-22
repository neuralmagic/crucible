/// The `[agent]` table's `sandbox_image` line, read and written in the draft's manifest text. The
/// studio edits crucible.toml as a buffer, so the picker patches the one line rather than
/// re-serializing the document and losing the author's comments and ordering.

const AGENT_HEADER = /^\[agent\]\s*(#.*)?$/;
const TABLE_HEADER = /^\[/;
const SANDBOX_LINE = /^(\s*sandbox_image\s*=\s*)("[^"]*"|'[^']*')(.*)$/;

interface AgentTable {
  /** Index of the `[agent]` header line, or -1. */
  header: number;
  /** Index one past the table's last line. */
  end: number;
  /** Index of the `sandbox_image` line inside it, or -1. */
  image: number;
}

function agentTable(lines: readonly string[]): AgentTable {
  const header = lines.findIndex((l) => AGENT_HEADER.test(l.trim()));
  if (header === -1) return { header: -1, end: -1, image: -1 };
  let end = lines.length;
  let image = -1;
  for (let i = header + 1; i < lines.length; i += 1) {
    const line = lines[i] ?? '';
    if (TABLE_HEADER.test(line.trim())) {
      end = i;
      break;
    }
    if (image === -1 && SANDBOX_LINE.test(line)) image = i;
  }
  return { header, end, image };
}

/** The sandbox image the manifest names under `[agent]`, or null. */
export function readSandboxImage(toml: string): string | null {
  const lines = toml.split('\n');
  const table = agentTable(lines);
  if (table.image === -1) return null;
  const match = SANDBOX_LINE.exec(lines[table.image] ?? '');
  if (match === null) return null;
  const quoted = match[2] ?? '';
  const value = quoted.slice(1, -1).trim();
  return value.length === 0 ? null : value;
}

/** The manifest with `sandbox_image` set to `reference`: the existing line rewritten in place, or
 * a new line at the top of `[agent]`, or a new `[agent]` table when there is none. */
export function setSandboxImage(toml: string, reference: string): string {
  const lines = toml.split('\n');
  const table = agentTable(lines);
  const assignment = `sandbox_image = "${reference}"`;
  if (table.image !== -1) {
    const match = SANDBOX_LINE.exec(lines[table.image] ?? '');
    const trailing = match?.[3] ?? '';
    lines[table.image] = `${match?.[1] ?? 'sandbox_image = '}"${reference}"${trailing}`;
    return lines.join('\n');
  }
  if (table.header !== -1) {
    lines.splice(table.header + 1, 0, assignment);
    return lines.join('\n');
  }
  const body = toml.endsWith('\n') || toml.length === 0 ? toml : `${toml}\n`;
  return `${body}${body.length === 0 ? '' : '\n'}[agent]\n${assignment}\n`;
}

/** `repository@digest`: what the picker writes, so the manifest pins what it matched. */
export function pinnedReference(repository: string, digest: string): string {
  return `${repository}@${digest}`;
}

/** The short digest a human compares by eye. */
export function shortDigest(digest: string): string {
  const hex = digest.startsWith('sha256:') ? digest.slice(7) : digest;
  return hex.slice(0, 12);
}
