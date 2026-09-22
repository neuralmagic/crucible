import { useEffect, useMemo, useRef, useState } from 'react';
import type { HTMLAttributes, Ref } from 'react';
import {
  hotkeysCoreFeature,
  type ItemInstance,
  renamingFeature,
  syncDataLoaderFeature,
  type TreeInstance,
} from '@headless-tree/core';
import { useTree } from '@headless-tree/react';
import { cn } from '../ui';
import { directoryPaths, packTree, renamedTo, TREE_ROOT, type TreeNode } from './fileTree';

export interface FileTreePanelProps {
  paths: readonly string[];
  active: string;
  onSelect: (path: string) => void;
  /// Files carrying an engine complaint, marked where they are picked.
  flagged?: ReadonlySet<string>;
  /// Absent means the pack cannot be restructured here: no new file, no rename, no delete.
  onAdd?: (path: string) => void;
  onRename?: (from: string, to: string) => void;
  onRemove?: (path: string) => void;
}

/// The library hands its wiring over untyped; this is the one place it is given a shape.
type Wiring = HTMLAttributes<HTMLDivElement> & { ref?: Ref<HTMLDivElement> };

function rowWiring(item: ItemInstance<TreeNode>): Wiring {
  return item.getProps();
}

function containerWiring(tree: TreeInstance<TreeNode>, label: string): Wiring {
  return tree.getContainerProps(label);
}

const MISSING: TreeNode = { name: '', path: '', kind: 'file', children: [] };
const NONE: ReadonlySet<string> = new Set();

const ACTION =
  'cursor-pointer border-0 bg-transparent px-1 font-mono text-micro tracking-label text-ink-3 uppercase opacity-0 group-hover:opacity-100 group-focus-within:opacity-100 hover:text-ink focus-visible:opacity-100';

/// The pack as it is on disk. A pack nests, so this nests: directories come out of the path
/// prefixes, and a file typed into one implies every directory above it.
export function FileTreePanel({
  paths,
  active,
  onSelect,
  flagged = NONE,
  onAdd,
  onRename,
  onRemove,
}: FileTreePanelProps) {
  const { root, byId } = useMemo(() => packTree(paths), [paths]);
  const directories = useMemo(() => directoryPaths(root.children), [root]);
  const [expanded, setExpanded] = useState<string[]>(directories);
  const [renamingValue, setRenamingValue] = useState<string | undefined>(undefined);
  /// The directory a new file is being typed into, or null when none is.
  const [adding, setAdding] = useState<string | null>(null);
  const [typed, setTyped] = useState('');
  const cancelled = useRef(false);

  // A pack is small enough that every directory stays open, including one a new file just implied.
  useEffect(() => {
    setExpanded((previous) => [...new Set([...previous, ...directories])]);
  }, [directories]);

  const tree = useTree<TreeNode>({
    rootItemId: TREE_ROOT,
    state: { expandedItems: expanded, renamingValue },
    setExpandedItems: setExpanded,
    setRenamingValue,
    getItemName: (item) => item.getItemData().name,
    isItemFolder: (item) => item.getItemData().kind === 'dir',
    canRename: () => onRename !== undefined,
    onRename: (item, value) => {
      const node = item.getItemData();
      const target = renamedTo(node, value);
      if (target !== null) onRename?.(node.path, target);
    },
    onPrimaryAction: (item) => {
      const node = item.getItemData();
      if (node.kind === 'file') onSelect(node.path);
    },
    dataLoader: {
      getItem: (id) => byId.get(id) ?? MISSING,
      getChildren: (id) => (byId.get(id)?.children ?? []).map((child) => child.path),
    },
    features: [syncDataLoaderFeature, hotkeysCoreFeature, renamingFeature],
  });

  useEffect(() => {
    tree.rebuildTree();
  }, [tree, byId]);

  const openAdd = (directory: string) => {
    cancelled.current = false;
    setAdding(directory);
    setTyped('');
  };

  const commitAdd = () => {
    const named = typed.trim();
    if (!cancelled.current && named.length > 0 && adding !== null) onAdd?.(`${adding}${named}`);
    cancelled.current = false;
    setAdding(null);
    setTyped('');
  };

  return (
    <div className="flex max-h-full min-h-0 w-full flex-none flex-col border border-rule-hard bg-surface min-[900px]:w-[15rem]">
      <div className="flex items-center justify-between border-b border-rule bg-sunk px-2 py-0.5">
        <span className="font-mono text-micro tracking-label text-ink-3 uppercase">Files</span>
        {onAdd === undefined ? null : (
          <button
            type="button"
            aria-label="New file"
            title="New file at the pack root"
            onClick={() => {
              openAdd('');
            }}
            className="cursor-pointer border-0 bg-transparent px-1 font-mono text-micro tracking-label text-ink-2 uppercase hover:text-ink"
          >
            New
          </button>
        )}
      </div>

      <div
        {...containerWiring(tree, 'Draft files')}
        data-testid="draft-file-tree"
        className="min-h-0 flex-1 overflow-auto py-1 outline-none"
      >
        {tree.getItems().map((item) => {
          const node = item.getItemData();
          const level = item.getItemMeta().level;
          const selected = node.path === active;
          const renaming = item.isRenaming();
          return (
            <div
              key={item.getKey()}
              {...rowWiring(item)}
              aria-selected={selected}
              style={{ paddingLeft: `${0.5 + level * 0.75}rem` }}
              className={cn(
                'group flex cursor-pointer items-center gap-1 py-0.5 pr-1 font-mono text-micro outline-none',
                node.kind === 'dir' && 'tracking-label text-ink-3 uppercase',
                node.kind === 'file' && (selected ? 'bg-hi font-semibold text-ink' : 'text-ink-2'),
                node.kind === 'file' && !selected && 'hover:text-ink',
                node.kind === 'file' && flagged.has(node.path) && !selected && 'text-red'
              )}
              title={node.path}
              onDoubleClick={() => {
                item.startRenaming();
              }}
            >
              {renaming ? (
                <input
                  autoFocus
                  aria-label={`Rename ${node.path}`}
                  value={renamingValue ?? ''}
                  onChange={(event) => {
                    setRenamingValue(event.target.value);
                  }}
                  onBlur={() => {
                    tree.abortRenaming();
                  }}
                  onClick={(event) => {
                    event.stopPropagation();
                  }}
                  onDoubleClick={(event) => {
                    event.stopPropagation();
                  }}
                  className="min-w-0 flex-1 border border-rule-hard bg-paper px-1 font-mono text-micro text-ink"
                />
              ) : (
                <span className="min-w-0 flex-1 truncate">
                  {node.kind === 'dir' ? `${node.name}/` : node.name}
                </span>
              )}
              {renaming ? null : (
                <span className="flex flex-none items-center">
                  {node.kind === 'dir' && onAdd !== undefined && (
                    <button
                      type="button"
                      aria-label={`New file in ${node.path}`}
                      onClick={(event) => {
                        event.stopPropagation();
                        openAdd(node.path);
                      }}
                      className={ACTION}
                    >
                      New
                    </button>
                  )}
                  {onRename !== undefined && (
                    <button
                      type="button"
                      aria-label={`Rename ${node.path}`}
                      onClick={(event) => {
                        event.stopPropagation();
                        item.startRenaming();
                      }}
                      className={ACTION}
                    >
                      Rename
                    </button>
                  )}
                  {onRemove !== undefined && (
                    <button
                      type="button"
                      aria-label={`Delete ${node.path}`}
                      onClick={(event) => {
                        event.stopPropagation();
                        onRemove(node.path);
                      }}
                      className={ACTION}
                    >
                      Delete
                    </button>
                  )}
                </span>
              )}
            </div>
          );
        })}
      </div>

      {adding === null ? null : (
        <div className="flex items-center gap-1 border-t border-rule px-1.5 py-1">
          <span className="font-mono text-micro text-ink-3">{adding}</span>
          <input
            autoFocus
            aria-label="New file path"
            placeholder="SKILL.md"
            value={typed}
            onChange={(event) => {
              setTyped(event.target.value);
            }}
            onKeyDown={(event) => {
              if (event.key === 'Enter') commitAdd();
              if (event.key === 'Escape') {
                cancelled.current = true;
                setAdding(null);
              }
            }}
            onBlur={commitAdd}
            className="min-w-0 flex-1 border border-rule bg-paper px-1 font-mono text-micro text-ink"
          />
        </div>
      )}
    </div>
  );
}
