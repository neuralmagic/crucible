import { useEffect, useState } from 'react';
import { statusTone, type StatusTone } from '../ui';
import { issueStatusColor } from '../pages/issueStatus';
import { isDarkTheme, subscribeTheme } from '../appTheme';

export interface ChartPalette {
  isDark: boolean;
  paper: string;
  surface: string;
  sunk: string;
  ink: string;
  ink2: string;
  ink3: string;
  rule: string;
  ruleHard: string;
  green: string;
  amber: string;
  red: string;
  blue: string;
  hi: string;
  mono: string;
  series: readonly string[];
}

const LIGHT_FALLBACK = {
  paper: '#edebe2',
  surface: '#ffffff',
  sunk: '#dddacd',
  ink: '#100f0e',
  ink2: '#464439',
  ink3: '#78756a',
  rule: '#c4c0b0',
  ruleHard: '#8a8676',
  green: '#00663a',
  amber: '#7e4f00',
  red: '#96181f',
  blue: '#1a4785',
  hi: '#ffeda8',
} as const;

const DARK_FALLBACK = {
  paper: '#0a0a09',
  surface: '#1c1c19',
  sunk: '#040403',
  ink: '#f5f3ea',
  ink2: '#adaa9d',
  ink3: '#7b786c',
  rule: '#3a3830',
  ruleHard: '#666254',
  green: '#5acb86',
  amber: '#dfa53f',
  red: '#e56b69',
  blue: '#83afe8',
  hi: '#463d18',
} as const;

const MONO_FALLBACK = "'Ioskeley Mono', ui-monospace, SFMono-Regular, Menlo, monospace";

function reader(): (name: string, fallback: string) => string {
  const computed = getComputedStyle(document.documentElement);
  return (name, fallback) => computed.getPropertyValue(name).trim() || fallback;
}

export function readChartPalette(): ChartPalette {
  const isDark = isDarkTheme();
  const fallback = isDark ? DARK_FALLBACK : LIGHT_FALLBACK;
  const token = reader();
  const palette = {
    isDark,
    paper: token('--color-paper', fallback.paper),
    surface: token('--color-surface', fallback.surface),
    sunk: token('--color-sunk', fallback.sunk),
    ink: token('--color-ink', fallback.ink),
    ink2: token('--color-ink-2', fallback.ink2),
    ink3: token('--color-ink-3', fallback.ink3),
    rule: token('--color-rule', fallback.rule),
    ruleHard: token('--color-rule-hard', fallback.ruleHard),
    green: token('--color-green', fallback.green),
    amber: token('--color-amber', fallback.amber),
    red: token('--color-red', fallback.red),
    blue: token('--color-blue', fallback.blue),
    hi: token('--color-hi', fallback.hi),
    mono: token('--font-mono', MONO_FALLBACK),
  };
  return {
    ...palette,
    series: [palette.green, palette.blue, palette.amber, palette.red, palette.ink2, palette.ink3],
  };
}

function signature(palette: ChartPalette): string {
  return `${palette.isDark}|${palette.ink}|${palette.surface}|${palette.rule}|${palette.mono}`;
}

export function useChartPalette(): ChartPalette {
  const [palette, setPalette] = useState<ChartPalette>(readChartPalette);

  useEffect(() => {
    const sync = () =>
      setPalette((prev) => {
        const next = readChartPalette();
        return signature(prev) === signature(next) ? prev : next;
      });
    const unsubscribe = subscribeTheme(sync);
    sync();
    return unsubscribe;
  }, []);

  return palette;
}

export function toneColor(tone: StatusTone, palette: ChartPalette): string {
  switch (tone) {
    case 'green':
      return palette.green;
    case 'amber':
      return palette.amber;
    case 'blue':
      return palette.blue;
    case 'red':
      return palette.red;
    case 'grey':
      return palette.ink3;
  }
}

/** Series color for an issue status off the wire. */
export function statusChartColor(status: string, palette: ChartPalette): string {
  return toneColor(statusTone(issueStatusColor(status.replaceAll('_', '-'))), palette);
}

interface TextTheme {
  fontFamily: string;
  fontSize: number;
  fontWeight?: number;
  color: string;
}

interface AxisTheme {
  axisLine: { show: boolean; lineStyle: { color: string; width: number } };
  axisTick: { show: boolean; lineStyle: { color: string } };
  axisLabel: TextTheme;
  splitLine: { show: boolean; lineStyle: { color: string; width: number } };
  splitArea: { show: boolean };
}

export interface ChartTheme {
  darkMode: boolean;
  animation: boolean;
  color: readonly string[];
  backgroundColor: string;
  textStyle: TextTheme;
  title: { textStyle: TextTheme; subtextStyle: TextTheme };
  tooltip: {
    backgroundColor: string;
    borderColor: string;
    borderWidth: number;
    padding: number[];
    extraCssText: string;
    textStyle: TextTheme;
    axisPointer: {
      lineStyle: { color: string; width: number };
      crossStyle: { color: string; width: number };
      shadowStyle: { color: string; opacity: number };
    };
  };
  legend: { icon: string; itemWidth: number; itemHeight: number; textStyle: TextTheme };
  grid: { show: boolean; borderColor: string; borderWidth: number };
  categoryAxis: AxisTheme;
  valueAxis: AxisTheme;
  logAxis: AxisTheme;
  timeAxis: AxisTheme;
  bar: {
    itemStyle: {
      borderRadius: number;
      shadowBlur: number;
      shadowColor: string;
    };
  };
  pie: { itemStyle: { borderRadius: number; borderColor: string; borderWidth: number } };
  graph: { itemStyle: { borderRadius: number } };
}

export function buildChartTheme(palette: ChartPalette): ChartTheme {
  const label: TextTheme = {
    fontFamily: palette.mono,
    fontSize: 10,
    color: palette.ink3,
  };
  const axis: AxisTheme = {
    axisLine: { show: true, lineStyle: { color: palette.rule, width: 1 } },
    axisTick: { show: false, lineStyle: { color: palette.rule } },
    axisLabel: label,
    splitLine: { show: true, lineStyle: { color: palette.rule, width: 1 } },
    splitArea: { show: false },
  };

  return {
    darkMode: palette.isDark,
    animation: false,
    color: palette.series,
    backgroundColor: 'transparent',
    textStyle: { fontFamily: palette.mono, fontSize: 11, color: palette.ink },
    title: {
      textStyle: { fontFamily: palette.mono, fontSize: 11, fontWeight: 600, color: palette.ink2 },
      subtextStyle: label,
    },
    tooltip: {
      backgroundColor: palette.surface,
      borderColor: palette.ruleHard,
      borderWidth: 1,
      padding: [5, 8],
      extraCssText: 'border-radius:0;box-shadow:none',
      textStyle: { fontFamily: palette.mono, fontSize: 11, color: palette.ink },
      axisPointer: {
        lineStyle: { color: palette.ruleHard, width: 1 },
        crossStyle: { color: palette.ruleHard, width: 1 },
        shadowStyle: { color: palette.hi, opacity: 0.55 },
      },
    },
    legend: {
      icon: 'rect',
      itemWidth: 8,
      itemHeight: 8,
      textStyle: { fontFamily: palette.mono, fontSize: 10, color: palette.ink2 },
    },
    grid: { show: true, borderColor: palette.rule, borderWidth: 1 },
    categoryAxis: axis,
    valueAxis: axis,
    logAxis: axis,
    timeAxis: axis,
    bar: {
      itemStyle: {
        borderRadius: 0,
        shadowBlur: 0,
        shadowColor: palette.green,
      },
    },
    pie: { itemStyle: { borderRadius: 0, borderColor: palette.surface, borderWidth: 1 } },
    graph: { itemStyle: { borderRadius: 0 } },
  };
}
