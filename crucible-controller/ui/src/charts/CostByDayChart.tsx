import type { EChartsOption } from 'echarts';
import { EChart } from './EChart';
import { ChartEmpty } from './ChartEmpty';
import { useChartPalette } from './chartTheme';
import type { components } from '../api/schema';

type LedgerDayDto = components['schemas']['LedgerDayDto'];

interface CostByDayChartProps {
  days: LedgerDayDto[];
}

export function CostByDayChart({ days }: CostByDayChartProps) {
  const palette = useChartPalette();

  if (days.length === 0) {
    return <ChartEmpty label="No cost data" height={300} />;
  }

  const option: EChartsOption = {
    tooltip: {
      trigger: 'axis',
      axisPointer: {
        type: 'shadow',
      },
      formatter: (params) => {
        if (!Array.isArray(params) || params.length === 0) return '';
        const param = params[0];
        const value = typeof param.value === 'number' ? param.value : 0;
        return `${param.name}<br/>Cost: $${value.toFixed(2)}`;
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
      type: 'category',
      data: days.map((d) => d.day),
      axisLabel: {
        rotate: 45,
      },
    },
    yAxis: {
      type: 'value',
      axisLabel: {
        formatter: '${value}',
      },
    },
    series: [
      {
        type: 'bar',
        data: days.map((d) => d.total_usd),
        itemStyle: {
          color: palette.blue,
          borderRadius: 0,
        },
      },
    ],
  };

  return <EChart option={option} style={{ height: '300px' }} />;
}
