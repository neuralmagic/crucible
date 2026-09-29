import { describe, expect, it } from 'vitest';
import { resourcesLabel } from './sandboxResources';

describe('resourcesLabel', () => {
  it('names what the sandbox asks for and nothing when it asks for nothing', () => {
    const none = { gpus: 0, cpu: null, memory: null, node_selector: {} };
    expect(resourcesLabel(none)).toBeNull();
    expect(resourcesLabel({ ...none, gpus: 1 })).toBe('1 GPU');
    expect(resourcesLabel({ ...none, gpus: 2, cpu: '8', memory: '32Gi' })).toBe(
      '2 GPUs · cpu 8 · memory 32Gi'
    );
    expect(resourcesLabel({ ...none, memory: '4Gi' })).toBe('memory 4Gi');
    expect(
      resourcesLabel({
        ...none,
        gpus: 1,
        node_selector: { 'nvidia.com/gpu.product': 'NVIDIA-H100-80GB-HBM3' },
      })
    ).toBe('1 GPU · on nvidia.com/gpu.product=NVIDIA-H100-80GB-HBM3');
  });
});
