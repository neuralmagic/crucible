import type { ScorePoint } from './feed';

interface SparklineProps {
  points: ScorePoint[];
  height?: number;
}

const PAD = 3;

/// A dependency-free inline-SVG score trajectory: one dot per measured candidate (filled = kept,
/// hollow = dropped), a line threading the kept ones, and a dashed line at the first score. Same
/// marker vocabulary as the run detail page's full chart — this is its live, always-moving little
/// sibling. Higher score sits higher.
export function Sparkline({ points, height = 36 }: SparklineProps) {
  if (points.length < 2) return null;

  const width = 100;
  const scores = points.map((p) => p.score);
  const min = Math.min(...scores);
  const max = Math.max(...scores);
  const span = max - min || 1;
  const x = (i: number) => PAD + (i / (points.length - 1)) * (width - 2 * PAD);
  const y = (score: number) => PAD + (1 - (score - min) / span) * (height - 2 * PAD);

  const kept = points.map((p, i) => ({ p, i })).filter(({ p }) => p.kept);
  const trail = kept
    .map(({ p, i }, k) => `${k === 0 ? 'M' : 'L'}${x(i).toFixed(1)},${y(p.score).toFixed(1)}`)
    .join(' ');
  const baseline = y(points[0].score);

  return (
    <svg
      viewBox={`0 0 ${width} ${height}`}
      preserveAspectRatio="none"
      style={{ height: `${height}px` }}
      className="block w-full"
      aria-label={`score sparkline, ${points.length} measurements, latest ${points[points.length - 1].score}`}
      data-testid="live-sparkline"
    >
      <line
        x1={0}
        y1={baseline}
        x2={width}
        y2={baseline}
        className="stroke-rule-hard"
        strokeWidth="1"
        strokeDasharray="2 2"
        vectorEffect="non-scaling-stroke"
      />
      {kept.length >= 2 && (
        <path
          d={trail}
          fill="none"
          className="stroke-green"
          strokeWidth="1.75"
          vectorEffect="non-scaling-stroke"
        />
      )}
      {points.map((p, i) => (
        <circle
          key={p.id}
          cx={x(i)}
          cy={y(p.score)}
          r="2"
          className={p.kept ? 'fill-green' : 'fill-surface stroke-red'}
          strokeWidth={p.kept ? 0 : 1.25}
          vectorEffect="non-scaling-stroke"
        />
      ))}
    </svg>
  );
}
