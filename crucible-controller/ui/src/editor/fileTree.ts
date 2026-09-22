/// A file tree over a pack's flat path map. A pack nests (`skills/read/SKILL.md`), so the panel
/// that picks a file has to nest too.
export interface TreeNode {
  name: string;
  /// The full pack-relative path for a file; the directory prefix for a directory.
  path: string;
  kind: 'file' | 'dir';
  children: TreeNode[];
}

/// The two files a pack is read from, pinned to the top of the root level in this order.
const LEAD = ['crucible.toml', 'workflow.star'];

interface Building {
  files: Map<string, string>;
  dirs: Map<string, Building>;
}

function empty(): Building {
  return { files: new Map(), dirs: new Map() };
}

function rank(node: TreeNode, depth: number): [number, number, string] {
  const lead = depth === 0 ? LEAD.indexOf(node.name) : -1;
  return [lead === -1 ? 1 : 0, lead === -1 ? (node.kind === 'dir' ? 1 : 0) : lead, node.name];
}

function order(nodes: TreeNode[], depth: number): TreeNode[] {
  return nodes.sort((a, b) => {
    const [ag, ao, an] = rank(a, depth);
    const [bg, bo, bn] = rank(b, depth);
    if (ag !== bg) return ag - bg;
    if (ao !== bo) return ao - bo;
    return an.localeCompare(bn);
  });
}

function flatten(node: Building, prefix: string, depth: number): TreeNode[] {
  const dirs: TreeNode[] = [...node.dirs].map(([name, child]) => ({
    name,
    path: `${prefix}${name}/`,
    kind: 'dir' as const,
    children: flatten(child, `${prefix}${name}/`, depth + 1),
  }));
  const files: TreeNode[] = [...node.files].map(([name, path]) => ({
    name,
    path,
    kind: 'file' as const,
    children: [],
  }));
  return order([...dirs, ...files], depth);
}

export function fileTree(paths: readonly string[]): TreeNode[] {
  const root = empty();
  for (const path of paths) {
    const segments = path.split('/').filter((segment) => segment.length > 0);
    if (segments.length === 0) continue;
    let at = root;
    for (const segment of segments.slice(0, -1)) {
      let next = at.dirs.get(segment);
      if (next === undefined) {
        next = empty();
        at.dirs.set(segment, next);
      }
      at = next;
    }
    at.files.set(segments[segments.length - 1], path);
  }
  return flatten(root, '', 0);
}

/// Every directory path in a tree, so a freshly loaded panel can open all of them: a pack is
/// small enough that hiding its files behind a disclosure helps nobody.
export function directoryPaths(nodes: readonly TreeNode[]): string[] {
  return nodes.flatMap((node) =>
    node.kind === 'dir' ? [node.path, ...directoryPaths(node.children)] : []
  );
}

/// The first file in tree order — what a freshly loaded studio opens on.
export function firstFile(nodes: readonly TreeNode[]): string | null {
  for (const node of nodes) {
    if (node.kind === 'file') return node.path;
    const nested = firstFile(node.children);
    if (nested !== null) return nested;
  }
  return null;
}

/// The id the tree hangs everything off. A directory is keyed by its prefix and a file by its
/// path, so nothing collides with the root's empty string.
export const TREE_ROOT = '';

export interface PackTree {
  root: TreeNode;
  byId: ReadonlyMap<string, TreeNode>;
}

function index(nodes: readonly TreeNode[], into: Map<string, TreeNode>): void {
  for (const node of nodes) {
    into.set(node.path, node);
    index(node.children, into);
  }
}

/// The same tree, addressable by id: what a tree view walks when it asks for one item at a time.
export function packTree(paths: readonly string[]): PackTree {
  const children = fileTree(paths);
  const root: TreeNode = { name: '', path: TREE_ROOT, kind: 'dir', children };
  const byId = new Map<string, TreeNode>([[TREE_ROOT, root]]);
  index(children, byId);
  return { root, byId };
}

/// The directory a new file typed on this node belongs to: the node itself when it is one, and
/// its parent when it is a file.
export function directoryOf(path: string): string {
  if (path.endsWith('/')) return path;
  const cut = path.lastIndexOf('/');
  return cut === -1 ? '' : path.slice(0, cut + 1);
}

/// Where a node lands when its own name is retyped. A rename edits one segment, so the directory
/// it sits in is carried over and a directory keeps its trailing slash.
export function renamedTo(node: TreeNode, name: string): string | null {
  const named = name.trim();
  if (named.length === 0 || named.includes('/')) return null;
  const parent = directoryOf(node.kind === 'dir' ? node.path.slice(0, -1) : node.path);
  return node.kind === 'dir' ? `${parent}${named}/` : `${parent}${named}`;
}
