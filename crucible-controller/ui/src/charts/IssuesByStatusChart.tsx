import type { EChartsOption } from 'echarts';
import { EChart } from './EChart';
import { ChartEmpty } from './ChartEmpty';
import { statusChartColor, useChartPalette } from './chartTheme';
import type { components } from '../api/schema';

type StatusCount = components['schemas']['StatusCount'];

interface IssuesByStatusChartProps {
  statuses: StatusCount[];
}

export function IssuesByStatusChart({ statuses }: IssuesByStatusChartProps) {
  const palette = useChartPalette();

  if (statuses.length === 0) {
    return <ChartEmpty label="No status data" height={220} />;
  }

  const option: EChartsOption = {
    tooltip: {
      trigger: 'item',
      formatter: '{b}: {c} ({d}%)',
    },
    legend: {
      orient: 'vertical',
      right: '10%',
      top: 'center',
      formatter: (name) => name.toUpperCase(),
    },
    series: [
      {
        type: 'pie',
        radius: ['40%', '70%'],
        center: ['35%', '50%'],
        avoidLabelOverlap: true,
        itemStyle: {
          borderRadius: 0,
          borderColor: palette.surface,
          borderWidth: 1,
        },
        label: {
          show: false,
        },
        emphasis: {
          scale: false,
          label: {
            show: true,
            fontFamily: palette.mono,
            fontSize: 12,
            fontWeight: 600,
            color: palette.ink,
            formatter: '{b}',
          },
        },
        data: statuses.map((s) => ({
          value: s.count,
          name: s.status,
          itemStyle: {
            color: statusChartColor(s.status, palette),
          },
        })),
      },
    ],
  };

  return <EChart option={option} style={{ height: '220px' }} />;
}
