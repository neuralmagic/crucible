import { useEffect, useRef } from 'react';
import * as echarts from 'echarts/core';
import type { EChartsOption, ECharts } from 'echarts';
import { BarChart, LineChart, PieChart, GaugeChart } from 'echarts/charts';
import {
  GridComponent,
  TooltipComponent,
  TitleComponent,
  LegendComponent,
} from 'echarts/components';
import { CanvasRenderer } from 'echarts/renderers';
import { buildChartTheme, useChartPalette } from './chartTheme';
import { createCRT, type Crt } from './crt';
import { usePrefs } from '../api/prefs';

echarts.use([
  BarChart,
  LineChart,
  PieChart,
  GaugeChart,
  GridComponent,
  TooltipComponent,
  TitleComponent,
  LegendComponent,
  CanvasRenderer,
]);

const THEME_NAME = 'crucible';

interface EChartProps {
  option: EChartsOption;
  style?: React.CSSProperties;
  /**
   * Phosphor post-process. Dark theme only, and skipped when the browser has no WebGL or the
   * viewer asked for reduced motion. Turn it off for charts carrying dense labels, where the
   * curvature and grille cost more legibility than the look is worth.
   */
  crt?: boolean;
}

export function EChart({ option, style, crt = true }: EChartProps) {
  const containerRef = useRef<HTMLDivElement>(null);
  const chartHostRef = useRef<HTMLDivElement>(null);
  const glRef = useRef<HTMLCanvasElement>(null);
  const chartRef = useRef<ECharts | null>(null);
  const palette = useChartPalette();
  const { prefs } = usePrefs();
  const postEnabled = crt && prefs.chartCrt;

  useEffect(() => {
    const host = chartHostRef.current;
    const container = containerRef.current;
    const overlay = glRef.current;
    if (!host || !container || !overlay) return;

    echarts.registerTheme(THEME_NAME, buildChartTheme(palette));
    const chart = echarts.init(host, THEME_NAME);
    chartRef.current = chart;

    const resizeObserver = new ResizeObserver(() => {
      chart.resize();
    });
    resizeObserver.observe(host);

    const cleanups: (() => void)[] = [];

    // echarts.init does not create the canvas; the first setOption does, and that runs in a later
    // effect. So the post-process attaches on the first render rather than here.
    let post: Crt | null = null;
    const attach = (): void => {
      if (post || !postEnabled || !palette.isDark) return;
      const source = host.querySelector('canvas');
      if (!source) return;
      try {
        post = createCRT(source, overlay);
        host.style.opacity = '0';
      } catch (err) {
        // No WebGL, or a driver that will not compile the passes: show the plain canvas rather
        // than a blank box, and say so instead of degrading in silence.
        console.warn('chart post-process unavailable, falling back to the plain canvas', err);
        host.style.opacity = '';
        post = null;
      }
    };

    const onFinished = () => {
      attach();
      post?.pulse();
    };
    chart.on('finished', onFinished);
    cleanups.push(() => {
      chart.off('finished', onFinished);
    });

    const onPointer = () => {
      post?.pulse(600);
    };
    container.addEventListener('pointermove', onPointer);
    cleanups.push(() => {
      container.removeEventListener('pointermove', onPointer);
    });

    // A chart scrolled out of view has no business holding a frame loop.
    const io = new IntersectionObserver((entries) => {
      for (const entry of entries) {
        if (!entry.isIntersecting) post?.stop();
      }
    });
    io.observe(container);
    cleanups.push(() => {
      io.disconnect();
    });
    cleanups.push(() => {
      post?.stop();
    });

    return () => {
      for (const fn of cleanups) fn();
      // The host div outlives this effect, so its hidden state has to be handed back or turning
      // the post-process off leaves a blank box where the chart was.
      host.style.opacity = '';
      // The overlay canvas outlives it too, still holding the last composited frame. Resetting the
      // backing store drops it, so turning the post-process off shows the plain chart immediately.
      overlay.width = 0;
      overlay.height = 0;
      resizeObserver.disconnect();
      chart.dispose();
      chartRef.current = null;
    };
  }, [palette, postEnabled]);

  // postEnabled is a dependency because toggling it re-creates the chart above, and a fresh
  // instance has no option until this runs again.
  useEffect(() => {
    chartRef.current?.setOption(option, true);
  }, [option, palette, postEnabled]);

  return (
    <div ref={containerRef} style={{ position: 'relative', width: '100%', height: '100%', ...style }}>
      <div ref={chartHostRef} style={{ position: 'absolute', inset: 0 }} />
      <canvas
        ref={glRef}
        aria-hidden
        style={{ position: 'absolute', inset: 0, width: '100%', height: '100%', pointerEvents: 'none' }}
      />
    </div>
  );
}
