import {
  createColumnHelper,
  createExpandedRowModel,
  createSortedRowModel,
  metaHelper,
  rowExpandingFeature,
  rowSortingFeature,
  sortFn_alphanumeric,
  sortFn_basic,
  sortFn_datetime,
  sortFn_text,
  tableFeatures,
  useTable,
} from '@tanstack/react-table';
import type { ReactTable, RowData, TableOptions } from '@tanstack/react-table';

export interface DataColumnMeta {
  /** `end` right-aligns the header and cell, for numeric columns. */
  align?: 'start' | 'end';
  /** Cell padding: 10px, the 8px identifier gutter, or flush. */
  pad?: 'normal' | 'tight' | 'none';
  /** Let the cell wrap, for the title column. */
  wrap?: boolean;
  /** Collapse the column to its content width. */
  shrink?: boolean;
  /** Drop the column below 1100px. */
  hideNarrow?: boolean;
  /** CSS width for the column, e.g. '42%'. */
  width?: string;
  className?: string;
  headerClassName?: string;
}

export const dataTableFeatures = tableFeatures({
  rowSortingFeature,
  sortedRowModel: createSortedRowModel(),
  sortFns: {
    alphanumeric: sortFn_alphanumeric,
    basic: sortFn_basic,
    datetime: sortFn_datetime,
    text: sortFn_text,
  },
  rowExpandingFeature,
  expandedRowModel: createExpandedRowModel(),
  columnMeta: metaHelper<DataColumnMeta>(),
});

export type DataTableFeatures = typeof dataTableFeatures;

export type DataTableInstance<TData extends RowData> = ReactTable<DataTableFeatures, TData>;

export type DataTableOptions<TData extends RowData> = Omit<
  TableOptions<DataTableFeatures, TData>,
  'features'
>;

export function createDataColumnHelper<TData extends RowData>() {
  return createColumnHelper<DataTableFeatures, TData>();
}

export function useDataTable<TData extends RowData>(
  options: DataTableOptions<TData>,
): DataTableInstance<TData> {
  return useTable({
    getRowCanExpand: () => true,
    ...options,
    features: dataTableFeatures,
  });
}
