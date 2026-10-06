import { useEffect, useRef } from 'react';
import { useNavigate } from 'react-router-dom';
import { $api } from '../api/client';
import { notifyDecision, unseen } from './decisions';

/// Raise a browser notification for each decision request the signed-in user may answer, as it
/// opens. Requests already open when the page loads are seen, not announced.
export function useDecisionNotifications(): void {
  const navigate = useNavigate();
  const decisions = $api.useQuery('get', '/api/decisions', {}, { refetchInterval: 30_000 });
  const seen = useRef<Set<string> | null>(null);

  useEffect(() => {
    const open = decisions.data?.open;
    if (open === undefined) return;
    if (seen.current === null) {
      seen.current = new Set(open.map((d) => d.id));
      return;
    }
    for (const d of unseen(seen.current, open)) {
      seen.current.add(d.id);
      notifyDecision(d, (id) => {
        void navigate(`/decisions/${encodeURIComponent(id)}`);
      });
    }
  }, [decisions.data, navigate]);
}
