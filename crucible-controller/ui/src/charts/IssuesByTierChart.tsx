import type { EChartsOption } from 'echarts';
import { EChart } from './EChart';
import { ChartEmpty } from './ChartEmpty';
import { useChartPalette, type ChartPalette } from './chartTheme';
import type { components } from '../api/schema';

type TierCount = components['schemas']['TierCount'];

interface IssuesByTierChartProps {
  tiers: TierCount[];
}

function tierColor(tier: string, palette: ChartPalette): string {
  const name = tier.toUpperCase();
  if (name === 'T0') return palette.red;
  if (name === 'T1') return palette.amber;
  return palette.ink2;
}

export function IssuesByTierChart({ tiers }: IssuesByTierChartProps) {
  const palette = useChartPalette();

  if (tiers.length === 0) {
    return <ChartEmpty label="No tier data" height={220} />;
  }

  const option: EChartsOption = {
    tooltip: {
      trigger: 'axis',
      axisPointer: {
        type: 'shadow',
      },
    },
    grid: {
      left: '3%',
      right: '4%',
      bottom: '3%',
      top: '3%',
      containLabel: true,
    },
    xAxis: {
      type: 'value',
    },
    yAxis: {
      type: 'category',
      data: tiers.map((t) => t.tier),
      axisLabel: {
        formatter: (value: string) => value.toUpperCase(),
      },
    },
    series: [
      {
        type: 'bar',
        data: tiers.map((t) => ({
          value: t.count,
          itemStyle: { color: tierColor(t.tier, palette), borderRadius: 0 },
        })),
      },
    ],
  };

  return <EChart option={option} style={{ height: '220px' }} />;
}
