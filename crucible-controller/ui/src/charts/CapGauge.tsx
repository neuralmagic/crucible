import type { EChartsOption } from 'echarts';
import { EChart } from './EChart';
import { useChartPalette } from './chartTheme';

interface CapGaugeProps {
  title: string;
  current: number;
  cap?: number | null;
}

export function CapGauge({ title, current, cap }: CapGaugeProps) {
  const palette = useChartPalette();

  // cap <= 0 means uncapped: a zero cap would divide out to Infinity%.
  const hasCap = cap !== null && cap !== undefined && cap > 0;
  const percentage = hasCap ? (current / cap) * 100 : 0;
  const displayValue = hasCap ? percentage.toFixed(1) : current.toString();
  const displayText = hasCap ? `${displayValue}%` : displayValue;

  const gaugeColor = hasCap
    ? percentage > 90
      ? palette.red
      : percentage > 70
        ? palette.amber
        : palette.green
    : palette.blue;

  const option: EChartsOption = {
    series: [
      {
        type: 'gauge',
        startAngle: 200,
        endAngle: -20,
        min: 0,
        max: hasCap ? 100 : current * 1.2,
        splitNumber: 4,
        itemStyle: {
          color: gaugeColor,
          borderRadius: 0,
        },
        progress: {
          show: true,
          width: 12,
          roundCap: false,
        },
        pointer: {
          show: false,
        },
        axisLine: {
          roundCap: false,
          lineStyle: {
            width: 12,
            color: [[1, palette.rule]],
          },
        },
        axisTick: {
          show: false,
        },
        splitLine: {
          length: 8,
          lineStyle: {
            width: 1,
            color: palette.ruleHard,
          },
        },
        axisLabel: {
          show: false,
        },
        title: {
          show: true,
          offsetCenter: [0, '80%'],
          fontFamily: palette.mono,
          fontSize: 10,
          color: palette.ink3,
        },
        detail: {
          valueAnimation: false,
          formatter: displayText,
          fontFamily: palette.mono,
          fontSize: 20,
          fontWeight: 600,
          color: palette.ink,
          offsetCenter: [0, '0%'],
        },
        data: [
          {
            // Clamp the arc at the gauge max; the detail text still reports the real overage.
            value: hasCap ? Math.min(percentage, 100) : current,
            name: title.toUpperCase(),
          },
        ],
      },
    ],
  };

  return <EChart option={option} style={{ height: '180px' }} />;
}
