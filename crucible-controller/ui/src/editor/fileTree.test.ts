import { describe, expect, it } from 'vitest';
import {
  directoryOf,
  directoryPaths,
  fileTree,
  firstFile,
  packTree,
  renamedTo,
  TREE_ROOT,
  type TreeNode,
} from './fileTree';

function shape(nodes: readonly TreeNode[]): string[] {
  return nodes.flatMap((node) =>
    node.kind === 'file' ? [node.path] : [`${node.path}`, ...shape(node.children)]
  );
}

const PACK = [
  'skills/read/SKILL.md',
  'workflow.star',
  'crucible.toml',
  'skills/write/SKILL.md',
  'README.md',
];

describe('fileTree', () => {
  it('nests a pack and leads with the manifest then the workflow', () => {
    expect(shape(fileTree(PACK))).toEqual([
      'crucible.toml',
      'workflow.star',
      'README.md',
      'skills/',
      'skills/read/',
      'skills/read/SKILL.md',
      'skills/write/',
      'skills/write/SKILL.md',
    ]);
  });

  it('puts directories after files at every level below the lead', () => {
    const tree = fileTree(['a/z.md', 'a/nested/b.md', 'a/a.md']);
    const a = tree[0];
    expect(a.kind).toBe('dir');
    expect(a.children.map((child) => child.name)).toEqual(['a.md', 'z.md', 'nested']);
  });

  it('lists every directory so the panel can open them all', () => {
    expect(directoryPaths(fileTree(PACK))).toEqual(['skills/', 'skills/read/', 'skills/write/']);
  });

  it('opens on the first file in tree order', () => {
    expect(firstFile(fileTree(PACK))).toBe('crucible.toml');
    expect(firstFile(fileTree(['skills/read/SKILL.md']))).toBe('skills/read/SKILL.md');
    expect(firstFile([])).toBeNull();
  });

  /// A file's node carries the map key it came from, never a cleaned-up copy: the studio looks
  /// its buffer up by that string.
  it('drops empty segments without rewriting the path a file is keyed by', () => {
    expect(shape(fileTree(['', 'a//b.md']))).toEqual(['a/', 'a//b.md']);
  });
});

describe('packTree', () => {
  it('addresses every node by the id the tree view asks for', () => {
    const { root, byId } = packTree(PACK);
    expect(root.path).toBe(TREE_ROOT);
    expect(root.children.map((child) => child.path)).toEqual([
      'crucible.toml',
      'workflow.star',
      'README.md',
      'skills/',
    ]);
    expect(byId.get('skills/read/')?.kind).toBe('dir');
    expect(byId.get('skills/read/SKILL.md')?.name).toBe('SKILL.md');
    expect(byId.get('nothing.md')).toBeUndefined();
  });
});

describe('directoryOf', () => {
  it('is the node itself for a directory and the parent for a file', () => {
    expect(directoryOf('skills/read/')).toBe('skills/read/');
    expect(directoryOf('skills/read/SKILL.md')).toBe('skills/read/');
    expect(directoryOf('crucible.toml')).toBe('');
  });
});

describe('renamedTo', () => {
  const file: TreeNode = {
    name: 'SKILL.md',
    path: 'skills/read/SKILL.md',
    kind: 'file',
    children: [],
  };
  const dir: TreeNode = { name: 'read', path: 'skills/read/', kind: 'dir', children: [] };

  it('retypes one segment and keeps the directory it sits in', () => {
    expect(renamedTo(file, 'NOTES.md')).toBe('skills/read/NOTES.md');
    expect(renamedTo(dir, 'write')).toBe('skills/write/');
    expect(renamedTo({ ...file, path: 'workflow.star', name: 'workflow.star' }, 'plan.star')).toBe(
      'plan.star'
    );
  });

  it('refuses a name that is empty or a path of its own', () => {
    expect(renamedTo(file, '  ')).toBeNull();
    expect(renamedTo(file, 'a/b.md')).toBeNull();
  });
});
