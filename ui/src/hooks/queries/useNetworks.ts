import { useQuery } from '@tanstack/react-query';
import { api } from '../../lib/api-client';

export function useNetworks() {
  return useQuery({
    queryKey: ['networks'],
    queryFn: () => api.get<string[]>('/networks'),
  });
}
