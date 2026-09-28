import { useQueryClient } from '@tanstack/react-query';
import { useNavigate } from 'react-router-dom';
import { $api } from './client';

/// Start and stop viewing as another user. Either one changes who every query answers for, so
/// both drop the whole cache and land on the home page.
export function useViewAs() {
  const qc = useQueryClient();
  const navigate = useNavigate();
  const start = $api.useMutation('post', '/api/impersonation');
  const stop = $api.useMutation('delete', '/api/impersonation');
  const reset = async () => {
    await qc.resetQueries();
    await navigate('/');
  };
  return {
    start: async (login: string) => {
      await start.mutateAsync({ body: { login } });
      await reset();
    },
    stop: async () => {
      await stop.mutateAsync({});
      await reset();
    },
    pending: start.isPending || stop.isPending,
  };
}
