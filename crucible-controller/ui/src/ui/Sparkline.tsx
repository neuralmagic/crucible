import { cn } from './cn';

export type SparklineTone = 'win' | 'bad' | 'neutral';

const STROKE: Record<SparklineTone, string> = {
  win: 'stroke-green',
  bad: 'stroke-red',
  neutral: 'stroke-ink-2',
};

const DOT: Record<SparklineTone, string> = {
  win: 'fill-green',
  bad: 'fill-red',
  neutral: 'fill-ink-2',
};

export interface SparklineProps {
  /** Score per iteration, oldest first. The first value is usually the baseline. */
  values: readonly number[];
  /** Dashed reference line; defaults to the first value. */
  baseline?: number;
  tone?: SparklineTone;
  width?: number;
  height?: number;
  className?: string;
}

const PAD = 3;

export function Sparkline({
  values,
  baseline,
  tone = 'win',
  width = 76,
  height = 18,
  className,
}: SparklineProps) {
  if (values.length === 0) return null;

  const base = baseline ?? values[0];
  const all = [...values, base];
  const min = Math.min(...all);
  const max = Math.max(...all);
  const span = max - min;
  const top = PAD;
  const bottom = height - PAD;
  const y = (value: number) => (span === 0 ? height / 2 : top + ((max - value) / span) * (bottom - top));
  const step = values.length > 1 ? (width - PAD * 2) / (values.length - 1) : 0;
  const points = values.map((value, i) => `${(PAD + step * i).toFixed(2)},${y(value).toFixed(2)}`).join(' ');
  const last = values[values.length - 1];

  return (
    <svg
      width={width}
      height={height}
      viewBox={`0 0 ${width} ${height}`}
      aria-hidden="true"
      className={cn('block', className)}
    >
      <line
        x1={0}
        y1={y(base)}
        x2={width}
        y2={y(base)}
        className="stroke-rule-hard"
        strokeWidth={1}
        strokeDasharray="2 2"
      />
      <polyline points={points} fill="none" strokeWidth={tone === 'win' ? 1.5 : 1.25} className={STROKE[tone]} />
      <circle cx={PAD + step * (values.length - 1)} cy={y(last)} r={2} className={DOT[tone]} />
    </svg>
  );
}
