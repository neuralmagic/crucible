import type { StatusTone } from '../ui';
import type { ConnState } from './useLiveSession';

export function connLabel(connState: ConnState): { tone: StatusTone; text: string; live: boolean } {
  switch (connState.state) {
    case 'connecting':
      return { tone: 'grey', text: 'connecting', live: true };
    case 'live':
      return { tone: 'green', text: 'live', live: true };
    case 'reconnecting':
      return { tone: 'amber', text: 'reconnecting', live: true };
    case 'ended':
      return {
        tone: connState.reason.startsWith('error') ? 'red' : 'blue',
        text: `ended · ${connState.reason}`,
        live: false,
      };
  }
}
