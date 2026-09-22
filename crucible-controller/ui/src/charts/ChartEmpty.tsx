interface ChartEmptyProps {
  label: string;
  height: number;
}

export function ChartEmpty({ label, height }: ChartEmptyProps) {
  return (
    <div
      className="flex items-center justify-center font-mono text-label font-semibold uppercase tracking-[0.1em] text-ink-3"
      style={{ height: `${height}px` }}
    >
      {label}
    </div>
  );
}
