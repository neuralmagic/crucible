import type { EChartsOption } from 'echarts';
import { EChart } from './EChart';
import { ChartEmpty } from './ChartEmpty';
import { useChartPalette } from './chartTheme';
import type { components } from '../api/schema';

type LedgerTagDto = components['schemas']['LedgerTagDto'];

interface SpendByTagChartProps {
  tags: LedgerTagDto[];
  ceiling?: number | null;
}

/** Today's ledgered spend broken down by cost tag (rank-grounded / scope / run / …), as a
 * horizontal bar chart. The subtitle carries the day's total against the daily ceiling so the
 * aggregate gauge and this breakdown always tell one story. */
export function SpendByTagChart({ tags, ceiling }: SpendByTagChartProps) {
  const palette = useChartPalette();

  if (tags.length === 0) {
    return <ChartEmpty label="No spend today" height={220} />;
  }

  // Largest spender on top.
  const sorted = [...tags].sort((a, b) => a.total_usd - b.total_usd);
  const total = tags.reduce((sum, t) => sum + t.total_usd, 0);
  const subtitle =
    ceiling !== null && ceiling !== undefined
      ? `$${total.toFixed(2)} of $${ceiling.toFixed(2)} daily ceiling`
      : `$${total.toFixed(2)} today`;

  const option: EChartsOption = {
    title: {
      text: subtitle.toUpperCase(),
      left: 'center',
      top: 0,
    },
    tooltip: {
      trigger: 'axis',
      axisPointer: { type: 'shadow' },
      formatter: (params) => {
        if (!Array.isArray(params) || params.length === 0) return '';
        const param = params[0];
        const value = typeof param.value === 'number' ? param.value : 0;
        return `${param.name}<br/>$${value.toFixed(2)}`;
      },
    },
    grid: {
      left: '3%',
      right: '8%',
      bottom: '3%',
      top: 30,
      containLabel: true,
    },
    xAxis: {
      type: 'value',
      axisLabel: { formatter: '${value}' },
    },
    yAxis: {
      type: 'category',
      data: sorted.map((t) => t.tag),
      axisLabel: {
        formatter: (value: string) => value.toUpperCase(),
      },
    },
    series: [
      {
        type: 'bar',
        data: sorted.map((t) => t.total_usd),
        itemStyle: { color: palette.blue, borderRadius: 0 },
        label: {
          show: true,
          position: 'right',
          fontFamily: palette.mono,
          fontSize: 10,
          color: palette.ink2,
          formatter: ({ value }) => (typeof value === 'number' ? `$${value.toFixed(2)}` : ''),
        },
      },
    ],
  };

  return <EChart option={option} style={{ height: '220px' }} />;
}
