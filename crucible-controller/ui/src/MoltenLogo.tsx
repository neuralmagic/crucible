import { useEffect, useRef, useState } from 'react';
import { MASTHEAD_DEFAULTS, startMolten, type MoltenHandle } from './moltenSim';

export interface MoltenLogoProps {
  /// CSS size of the square canvas; the sim renders at 4x for crisp cells.
  size?: number;
  /// The static mark shown when WebGPU is unavailable or setup fails.
  fallback: React.ReactNode;
}

/// The wordmark's vessel, molten. Click sloshes; reduced-motion gets one settled frame; no
/// WebGPU gets the static mark unchanged.
export function MoltenLogo({ size = 30, fallback }: MoltenLogoProps) {
  const canvasRef = useRef<HTMLCanvasElement | null>(null);
  const handleRef = useRef<MoltenHandle | null>(null);
  const [live, setLive] = useState(false);

  useEffect(() => {
    const canvas = canvasRef.current;
    if (canvas === null) return;
    let cancelled = false;
    const still = window.matchMedia('(prefers-reduced-motion: reduce)').matches;
    void startMolten(canvas, { ...MASTHEAD_DEFAULTS, animate: !still }).then((handle) => {
      if (handle === null) return;
      if (cancelled) {
        handle.stop();
        return;
      }
      handleRef.current = handle;
      setLive(true);
    });
    return () => {
      cancelled = true;
      handleRef.current?.stop();
      handleRef.current = null;
    };
  }, []);

  return (
    <span className="relative inline-flex" style={{ width: size, height: size }}>
      {!live && <span className="absolute inset-0 inline-flex items-center">{fallback}</span>}
      <canvas
        ref={canvasRef}
        width={size * 4}
        height={size * 4}
        className="block h-full w-full cursor-crosshair"
        style={{ imageRendering: 'pixelated', opacity: live ? 1 : 0 }}
        aria-label="Crucible logo, molten: lava sloshing in the vessel. Click to slosh."
        onPointerDown={(e) => {
          const r = e.currentTarget.getBoundingClientRect();
          handleRef.current?.kick((e.clientX - r.left) / r.width < 0.5 ? 1 : -1);
        }}
      />
    </span>
  );
}
