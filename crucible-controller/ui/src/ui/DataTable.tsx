import { Fragment } from 'react';
import type { ReactNode } from 'react';
import type { RowData, Row } from '@tanstack/react-table';
import { cn } from './cn';
import type { DataColumnMeta, DataTableFeatures, DataTableInstance } from './table';

const PAD: Record<NonNullable<DataColumnMeta['pad']>, string> = {
  normal: 'px-2.5',
  tight: 'px-2',
  none: 'px-0',
};

function bodyCellClass(meta: DataColumnMeta | undefined): string {
  return cn(
    'h-row border-r border-b border-rule align-middle',
    PAD[meta?.pad ?? 'normal'],
    meta?.wrap === true ? 'max-w-0 whitespace-normal' : 'whitespace-nowrap',
    meta?.align === 'end' && 'text-right',
    meta?.shrink === true && 'w-px',
    meta?.hideNarrow === true && 'max-wide:hidden',
    meta?.className,
  );
}

function headerCellClass(meta: DataColumnMeta | undefined, sorted: boolean): string {
  return cn(
    'sticky z-5 border-r border-b border-rule-hard bg-sunk py-1.5 text-left align-middle font-mono text-label font-semibold uppercase tracking-group whitespace-nowrap',
    PAD[meta?.pad ?? 'normal'],
    sorted ? 'text-ink' : 'text-ink-2',
    meta?.align === 'end' && 'text-right',
    meta?.shrink === true && 'w-px',
    meta?.hideNarrow === true && 'max-wide:hidden',
    meta?.headerClassName,
  );
}

export interface DataTableProps<TData extends RowData> {
  table: DataTableInstance<TData>;
  /** Detail panel drawn under an expanded row; supplying it adds the expander column. */
  renderSubRow?: (row: Row<DataTableFeatures, TData>) => ReactNode;
  /** Summary line in the table footer. */
  footer?: ReactNode;
  /** Rendered in place of the body when there are no rows. */
  empty?: ReactNode;
  className?: string;
}

export function DataTable<TData extends RowData>({
  table,
  renderSubRow,
  footer,
  empty,
  className,
}: DataTableProps<TData>) {
  const rows = table.getRowModel().rows;
  const expandable = renderSubRow !== undefined;
  const span = table.getAllLeafColumns().length + (expandable ? 1 : 0);

  return (
    <table className={cn('w-full border-collapse bg-surface', className)}>
      <thead>
        {table.getHeaderGroups().map((group) => (
          <tr key={group.id}>
            {expandable && (
              <th
                style={{ top: 0 }}
                className={headerCellClass({ pad: 'tight', shrink: true }, false)}
                aria-label="Expand"
              />
            )}
            {group.headers.map((header) => {
              const meta = header.column.columnDef.meta;
              const sorted = header.column.getIsSorted();
              const canSort = header.column.getCanSort();
              const toggle = header.column.getToggleSortingHandler();
              return (
                <th
                  key={header.id}
                  colSpan={header.colSpan}
                  style={{ top: 0, width: meta?.width }}
                  aria-sort={sorted === 'asc' ? 'ascending' : sorted === 'desc' ? 'descending' : undefined}
                  className={cn(headerCellClass(meta, sorted !== false), canSort && 'cursor-pointer select-none')}
                  onClick={toggle}
                >
                  {header.isPlaceholder ? null : (
                    <span className="inline-flex items-center gap-1">
                      <table.FlexRender header={header} />
                      {sorted !== false && <span className="text-green">{sorted === 'asc' ? '▲' : '▼'}</span>}
                    </span>
                  )}
                </th>
              );
            })}
          </tr>
        ))}
      </thead>
      <tbody>
        {rows.length === 0 && empty !== undefined && (
          <tr>
            <td colSpan={span} className="border-b border-rule p-0">
              {empty}
            </td>
          </tr>
        )}
        {rows.map((row) => {
          const open = row.getIsExpanded();
          return (
            <Fragment key={row.id}>
              <tr className={cn('group/row', open ? 'bg-hi' : 'hover:bg-hi')}>
                {expandable && (
                  <td className={bodyCellClass({ pad: 'tight', shrink: true })}>
                    <button
                      type="button"
                      aria-expanded={open}
                      aria-label={open ? 'Collapse row' : 'Expand row'}
                      onClick={() => {
                        row.toggleExpanded();
                      }}
                      className="cursor-pointer border-0 bg-transparent p-0 font-mono text-label text-ink-3 hover:text-ink"
                    >
                      {open ? '▾' : '▸'}
                    </button>
                  </td>
                )}
                {row.getAllCells().map((cell) => (
                  <td key={cell.id} className={bodyCellClass(cell.column.columnDef.meta)}>
                    <table.FlexRender cell={cell} />
                  </td>
                ))}
              </tr>
              {open && renderSubRow !== undefined && (
                <tr>
                  <td colSpan={span} className="h-auto border-b border-rule bg-sunk p-0 align-top">
                    {renderSubRow(row)}
                  </td>
                </tr>
              )}
            </Fragment>
          );
        })}
      </tbody>
      {footer !== undefined && (
        <tfoot>
          <tr>
            <td colSpan={span} className="border-t border-rule-hard px-2.5 py-2 font-mono text-data text-ink-3">
              {footer}
            </td>
          </tr>
        </tfoot>
      )}
    </table>
  );
}
